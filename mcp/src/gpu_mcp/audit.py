"""Append-only audit log: one JSON line per tool call or login."""

from __future__ import annotations

import json
import os
import threading
from datetime import UTC, datetime
from pathlib import Path
from typing import Any


class Audit:
    def __init__(self, path: Path):
        self.path = path
        self.lock = threading.Lock()

    def write(
        self,
        *,
        caller: str | None,
        tool: str,
        status: str,
        args: dict[str, Any] | None = None,
        job_id: str | None = None,
        error: str | None = None,
    ) -> None:
        line: dict[str, Any] = {
            "time": datetime.now(UTC).isoformat(timespec="seconds"),
            "caller": caller,
            "tool": tool,
            "args": args or {},
            "job_id": job_id,
            "status": status,
        }
        if error is not None:
            line["error"] = error[:500]
        text = json.dumps(line, sort_keys=False) + "\n"
        with self.lock:
            self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            fd = os.open(self.path, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
            with os.fdopen(fd, "a") as f:
                f.write(text)
