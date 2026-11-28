"""Runs the installed gpu binary with --json, as an argument list, never a shell."""

from __future__ import annotations

import asyncio
import json
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .config import SECRET_ENV

STDERR_TAIL = 2000


@dataclass
class GpuResult:
    code: int
    envelope: dict[str, Any] | None
    stderr: str

    @property
    def ok(self) -> bool:
        return self.code == 0 and bool(self.envelope and self.envelope.get("ok"))

    @property
    def result(self) -> dict[str, Any]:
        value = (self.envelope or {}).get("result")
        return value if isinstance(value, dict) else {}

    @property
    def error(self) -> str:
        error = (self.envelope or {}).get("error")
        if isinstance(error, dict) and error.get("message"):
            return str(error["message"])
        if isinstance(error, str) and error:
            return error
        tail = self.stderr.strip()[-STDERR_TAIL:]
        return tail or f"gpu exited with status {self.code}"


def child_env() -> dict[str, str]:
    """The server's environment minus its OAuth secrets and the owner override."""
    return {k: v for k, v in os.environ.items() if k not in SECRET_ENV and k != "GPU_OWNER"}


async def run(binary: str, args: list[str], cwd: Path, timeout: float | None) -> GpuResult:
    proc = await asyncio.create_subprocess_exec(
        binary,
        "--json",
        *args,
        cwd=cwd,
        env=child_env(),
        stdin=asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
        # Its own session, so a server restart does not interrupt a launch or
        # download midway; the gpu deadline still bounds the instance.
        start_new_session=True,
    )
    try:
        out, err = await asyncio.wait_for(proc.communicate(), timeout)
    except TimeoutError:
        proc.kill()
        await proc.wait()
        return GpuResult(124, None, f"gpu {' '.join(args[:2])} timed out after {timeout:.0f}s")
    envelope = None
    text = out.decode("utf-8", "replace").strip()
    for candidate in (text, text.splitlines()[-1] if text else ""):
        try:
            parsed = json.loads(candidate)
        except json.JSONDecodeError:
            continue
        if isinstance(parsed, dict):
            envelope = parsed
            break
    return GpuResult(proc.returncode or 0, envelope, err.decode("utf-8", "replace"))
