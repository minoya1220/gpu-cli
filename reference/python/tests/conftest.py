from __future__ import annotations

import asyncio
import contextlib
import os
import time
from collections.abc import Awaitable, Callable
from pathlib import Path
from typing import Any

import pytest
from prompt_toolkit.input.defaults import create_pipe_input
from prompt_toolkit.output import DummyOutput

from gpu.picker import Picker, Screen


@pytest.fixture(autouse=True)
def isolate_user_directories(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    home = tmp_path / "home"
    cache = tmp_path / "cache"
    config = tmp_path / "config"
    state = tmp_path / "state"
    for directory in (home, cache, config, state):
        directory.mkdir()
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: home))
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("XDG_CACHE_HOME", str(cache))
    monkeypatch.setenv("XDG_CONFIG_HOME", str(config))
    monkeypatch.setenv("XDG_STATE_HOME", str(state))
    assert Path(os.environ["HOME"]) == home
    assert Path(os.environ["XDG_CACHE_HOME"]) == cache
    assert Path(os.environ["XDG_CONFIG_HOME"]) == config
    assert Path(os.environ["XDG_STATE_HOME"]) == state


class Keyboard:
    """Presses keys in a running Picker through a pipe, one key per write."""

    def __init__(self, pipe: Any, picker: Picker):
        self._pipe = pipe
        self.picker = picker

    async def press(self, key: str, *, until: Callable[[], bool] | None = None) -> float:
        """Send one key; if `until` is given, wait for it and return the latency."""
        start = time.perf_counter()
        self._pipe.send_text(key)
        if until is None:
            await asyncio.sleep(0.02)
            return 0.0
        await self.wait(until)
        return time.perf_counter() - start

    async def type(self, text: str) -> None:
        for char in text:
            await self.press(char, until=lambda typed=self.picker.filter + char: self.picker.filter == typed)

    async def wait(self, condition: Callable[[], bool], timeout: float = 2.0) -> None:
        deadline = time.perf_counter() + timeout
        while not condition():
            if time.perf_counter() > deadline:
                raise AssertionError(f"timed out on screen {self.picker.title!r}")
            await asyncio.sleep(0.002)

    def on(self, title: str) -> Callable[[], bool]:
        return lambda: self.picker.title == title


Script = Callable[[Keyboard], Awaitable[None]]


def run_picker(first: Screen, script: Script, *, timeout: float = 10.0) -> Any:
    """Run a real Picker on pipe input while `script` presses keys; return its result."""

    async def main() -> Any:
        with create_pipe_input() as pipe:
            picker = Picker(first, input=pipe, output=DummyOutput())
            app = asyncio.ensure_future(picker.run_async())
            try:
                await script(Keyboard(pipe, picker))
                return await asyncio.wait_for(app, 2.0)
            finally:
                if not app.done():
                    app.cancel()
                    with contextlib.suppress(asyncio.CancelledError):
                        await app

    return asyncio.run(asyncio.wait_for(main(), timeout))


@pytest.fixture
def drive() -> Callable[..., Any]:
    return run_picker
