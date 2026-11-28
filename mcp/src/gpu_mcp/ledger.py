"""Per-caller daily spend cap: estimated cost of each launch in the last 24 hours.

Each launch reserves its worst case (the price cap times its lifetime) before
the gpu process starts, then replaces it with the rented price times its
lifetime once the job runs. A launch that gpu reports as never created gives
its reservation back. A ledger that cannot be read refuses launches.
"""

from __future__ import annotations

import json
import os
import secrets
import threading
import time
from pathlib import Path

WINDOW = 86400


class LedgerError(Exception):
    pass


class Ledger:
    def __init__(self, path: Path):
        self.path = path
        self.lock = threading.Lock()

    def _read(self) -> dict[str, list[dict]]:
        if not self.path.exists():
            return {}
        try:
            data = json.loads(self.path.read_text())
        except (OSError, json.JSONDecodeError) as exc:
            raise LedgerError(f"Spend ledger {self.path} is unreadable ({exc}); fix or remove it.") from None
        if not isinstance(data, dict):
            raise LedgerError(f"Spend ledger {self.path} is malformed; fix or remove it.")
        return data

    def _write(self, data: dict) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        tmp = self.path.with_suffix(".tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as f:
            json.dump(data, f)
        os.replace(tmp, self.path)

    @staticmethod
    def _window(entries: list[dict], now: float) -> list[dict]:
        return [e for e in entries if e.get("at", 0) > now - WINDOW]

    def spent(self, caller: str) -> float:
        with self.lock:
            return sum(e["usd"] for e in self._window(self._read().get(caller, []), time.time()))

    def reserve(self, caller: str, usd: float, cap: float) -> str:
        """Reserve `usd`, or raise LedgerError naming the amounts when over `cap`."""
        with self.lock:
            now = time.time()
            data = self._read()
            entries = self._window(data.get(caller, []), now)
            used = sum(e["usd"] for e in entries)
            if used + usd > cap + 1e-9:
                raise LedgerError(
                    f"Refused: daily spend cap for {caller} is ${cap:.2f}; "
                    f"${used:.2f} is estimated for the last 24 hours and this job could cost "
                    f"up to ${usd:.2f}. Use a cheaper max_price or a shorter expire, or wait."
                )
            key = secrets.token_hex(8)
            entries.append({"id": key, "at": now, "usd": usd})
            data[caller] = entries
            self._write(data)
            return key

    def settle(self, caller: str, key: str, usd: float | None, job_id: str | None = None) -> None:
        """Replace a reservation with `usd`, or drop it when `usd` is None."""
        with self.lock:
            data = self._read()
            entries = []
            for entry in data.get(caller, []):
                if entry.get("id") == key:
                    if usd is None:
                        continue
                    entry = {**entry, "usd": usd, "job_id": job_id}
                entries.append(entry)
            data[caller] = self._window(entries, time.time())
            self._write(data)
