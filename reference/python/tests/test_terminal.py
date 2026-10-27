"""End to end: the real `gpu up --dry-run` in a pseudo-terminal, keys typed one at a time.

Fake `gh` and `vastai` executables on PATH log each call and answer slowly,
like the real network-bound CLIs. The screen is read through a terminal
emulator, as a person would see it.
"""

from __future__ import annotations

import fcntl
import json
import os
import select
import signal
import struct
import subprocess
import sys
import termios
import time
from pathlib import Path

import pyte
import pytest

SRC = Path(__file__).resolve().parents[1] / "src"
ENTER, BACK, LEFT, RIGHT = "\r", "`", "\x1b[D", "\x1b[C"
# Home, End, PageUp, PageDown, Delete, F5, Shift-Left, Shift-Right, Ctrl-Left.
SPECIAL_KEYS = ["\x1b[H", "\x1b[F", "\x1b[5~", "\x1b[6~", "\x1b[3~", "\x1b[15~", "\x1b[1;2D", "\x1b[1;2C", "\x1b[1;5D"]
FAKE_CLI = """
import json, os, sys, time

tool, args = sys.argv[1], sys.argv[2:]
with open(os.environ["FAKE_CLI_LOG"], "a") as log:
    log.write(json.dumps([tool, *args]) + "\\n")
time.sleep(0.3)
if tool == "gh" and args[:2] == ["repo", "list"]:
    print(json.dumps([{"name": "gpu-kernels"}, {"name": "other-repo"}]))
elif tool == "vastai" and args[:2] == ["search", "offers"]:
    query = dict(term.split("=", 1) for term in args[2].split() if "=" in term)
    gpus = [query["gpu_name"].replace("_", " ")] if "gpu_name" in query else ["RTX 4090", "Tesla T4"]
    print(json.dumps([
        {"id": 100 + i, "gpu_name": gpu, "num_gpus": 1, "dph_total": 0.3 + i / 10,
         "reliability": 0.99, "geolocation": "Test, XX", "verification": "verified"}
        for i, gpu in enumerate(gpus)
    ]))
else:
    sys.exit("unexpected call: " + " ".join(sys.argv[1:]))
"""


class _Screen(pyte.Screen):
    """Answers cursor position requests the way a real terminal does."""

    def __init__(self, columns: int, lines: int, fd: int):
        super().__init__(columns, lines)
        self._fd = fd

    def write_process_input(self, data: str) -> None:
        os.write(self._fd, data.encode())


class Terminal:
    def __init__(self, argv: list[str], env: dict[str, str], columns: int = 100, lines: int = 30):
        self._fd, child = os.openpty()
        fcntl.ioctl(child, termios.TIOCSWINSZ, struct.pack("HHHH", lines, columns, 0, 0))
        self.process = subprocess.Popen(
            argv, stdin=child, stdout=child, stderr=child, env=env, start_new_session=True
        )
        os.close(child)
        self.screen = _Screen(columns, lines, self._fd)
        self._stream = pyte.ByteStream(self.screen)
        self.output = b""

    @property
    def lines(self) -> list[str]:
        return [line.rstrip() for line in self.screen.display if line.strip()]

    @property
    def title(self) -> str:
        return self.lines[0] if self.lines else ""

    def _pump(self, timeout: float) -> None:
        if select.select([self._fd], [], [], timeout)[0]:
            try:
                data = os.read(self._fd, 65536)
            except OSError:  # the child closed the terminal
                data = b""
            if data:
                self.output += data
                self._stream.feed(data)
            else:
                time.sleep(timeout)

    def wait(self, condition, timeout: float = 5.0) -> float:
        start = time.perf_counter()
        while not condition():
            if time.perf_counter() - start > timeout:
                raise AssertionError("timed out; screen:\n" + "\n".join(self.lines))
            self._pump(0.005)
        return time.perf_counter() - start

    def press(self, key: str, until=None) -> float:
        time.sleep(0.05)  # a person's pace: every key arrives in its own read
        start = time.perf_counter()
        os.write(self._fd, key.encode())
        if until is not None:
            self.wait(until)
        return time.perf_counter() - start

    def on(self, title: str):
        return lambda: self.title == title

    def exit_status(self, timeout: float = 5.0) -> int:
        self.wait(lambda: self.process.poll() is not None, timeout)
        self._pump(0.05)
        return self.process.returncode

    def close(self) -> None:
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait(5)
        os.close(self._fd)


@pytest.fixture
def gpu_up(tmp_path):
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    (tmp_path / "fake_cli.py").write_text(FAKE_CLI)
    for tool in ("gh", "vastai"):
        wrapper = bin_dir / tool
        wrapper.write_text(f'#!/bin/sh\nexec "{sys.executable}" "{tmp_path / "fake_cli.py"}" {tool} "$@"\n')
        wrapper.chmod(0o755)
    log = tmp_path / "calls.log"
    log.touch()
    env = {
        **os.environ,
        "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
        "PYTHONPATH": str(SRC),
        "TERM": "xterm-256color",
        "FAKE_CLI_LOG": str(log),
    }
    terminals = []

    def start() -> Terminal:
        terminal = Terminal([sys.executable, "-m", "gpu.cli", "up", "--dry-run"], env)
        terminals.append(terminal)
        return terminal

    start.calls = lambda: [json.loads(line) for line in log.read_text().splitlines()]
    yield start
    for terminal in terminals:
        terminal.close()


def test_backtick_from_the_deepest_screen_steps_back_to_the_first_then_quits(gpu_up):
    term = gpu_up()
    term.wait(lambda: any("gpu-kernels" in line for line in term.lines))
    assert term.title == "Repository"
    term.press(ENTER, until=term.on("Mode"))
    term.press(ENTER, until=term.on("GPU"))
    for char in "4090":
        term.press(char)
    term.wait(lambda: term.lines[1] == "Filter: 4090")
    term.press(ENTER, until=term.on("Offer: RTX 4090"))
    term.wait(lambda: any("id=100" in line for line in term.lines))

    steps = [
        ("GPU", lambda: term.title == "GPU" and term.lines[1] == "Filter: 4090"),
        ("GPU", lambda: term.title == "GPU" and not term.lines[1].startswith("Filter:")),
        ("Mode", term.on("Mode")),
        ("Repository", term.on("Repository")),
    ]
    for title, shown in steps:
        latency = term.press(BACK, until=shown)
        assert term.process.poll() is None, f"quit instead of going back to {title}"
        assert latency < 1.0, f"back to {title} took {latency:.2f}s"

    term.press(BACK, until=lambda: "Press `/← again to quit" in term.lines[-1])
    assert term.process.poll() is None, "one ` on the first screen must not quit"
    term.press(BACK)
    assert term.exit_status() == 1
    assert b"Cancelled." in term.output
    tools = [call[0] for call in gpu_up.calls()]
    assert tools == ["gh", "vastai"], "navigation must not rerun gh or vastai"


def test_keyboard_dry_run_launch_after_going_back(gpu_up):
    term = gpu_up()
    term.wait(lambda: any("other-repo" in line for line in term.lines))
    term.press(ENTER, until=term.on("Mode"))
    term.press(ENTER, until=term.on("GPU"))
    term.press(BACK, until=term.on("Mode"))
    term.press(BACK, until=term.on("Repository"))
    term.press("4", until=term.on("Mode"))  # digits pick on short lists
    term.press("2", until=term.on("GPU"))
    for char in "t4":
        term.press(char)
    term.press(ENTER, until=term.on("Offer: Tesla T4"))
    term.wait(lambda: any("id=100" in line for line in term.lines))
    term.press(ENTER)
    assert term.exit_status() == 0
    output = term.output.decode()
    assert "DRY RUN: no instance created" in output
    assert '"100"' in output and "/workspace/other-repo" in output
    assert '"docker.io/vastai/kvm"' in output
    assert [call[0] for call in gpu_up.calls()] == ["gh", "vastai"]


@pytest.mark.parametrize("repo", [None, "https://github.com/sgl-project/sglang.git"])
def test_new_repository_choices_dry_run_in_terminal(gpu_up, repo):
    term = gpu_up()
    term.wait(lambda: any("No repository" in line for line in term.lines))
    if repo is None:
        term.press("1", until=term.on("Mode"))
    else:
        term.press("2", until=term.on("Clone from URL"))
        term.press("\x1b[200~" + repo + "\x1b[201~")
        term.press(ENTER, until=term.on("Mode"))
    term.press("1", until=term.on("GPU"))
    for char in "t4":
        term.press(char)
    term.press(ENTER, until=term.on("Offer: Tesla T4"))
    term.wait(lambda: any("id=100" in line for line in term.lines))
    term.press(ENTER)
    assert term.exit_status() == 0
    output = term.output.decode()
    assert "DRY RUN: no instance created" in output
    if repo is None:
        assert "git clone" not in output and "uv sync" not in output
        assert '"no-repo"' in output
    else:
        assert f"git clone {repo} /workspace/sglang" in output
        assert '"sglang"' in output
    assert all(call[1:3] in (["repo", "list"], ["search", "offers"]) for call in gpu_up.calls())


def test_arrow_keys_navigate_and_special_keys_never_reach_the_filter(gpu_up):
    term = gpu_up()
    term.wait(lambda: any("gpu-kernels" in line for line in term.lines))
    assert "Ctrl-C" not in term.lines[-1]
    for char in "oth":
        term.press(char)
    for key in [*SPECIAL_KEYS, LEFT]:
        term.press(key)
    # Left cleared the filter; nothing else was typed into it.
    term.wait(lambda: term.lines[1] == "› 1. No repository")
    assert not any("[" in line for line in term.lines[:2])
    latency = term.press(RIGHT, until=term.on("Mode"))
    assert latency < 1.0, f"Right took {latency:.2f}s"
    for key in SPECIAL_KEYS:
        term.press(key)
    term.press(LEFT, until=term.on("Repository"))
    term.press("\x1b[B")
    term.press(RIGHT, until=term.on("Clone from URL"))
    for char in "o/r":
        term.press(char)
    for key in SPECIAL_KEYS:
        term.press(key)
    term.wait(lambda: term.lines[1] == "Repository: o/r")
    term.press(LEFT, until=lambda: not term.lines[1].startswith("Repository:"))
    term.press(LEFT, until=term.on("Repository"))

    term.press(LEFT, until=lambda: "again to quit" in term.lines[-1])
    term.press("\x1b[B", until=lambda: "again to quit" not in term.lines[-1])
    term.press(LEFT, until=lambda: "again to quit" in term.lines[-1])
    assert term.process.poll() is None, "one Left on the first screen must not quit"
    start = time.perf_counter()
    term.press(LEFT)
    assert term.exit_status() == 1
    assert time.perf_counter() - start < 1.0
    assert b"DRY RUN" not in term.output


@pytest.mark.parametrize(("key", "status"), [("\x03", 130), ("\x04", 1)])
def test_ctrl_c_and_ctrl_d_quit_from_the_first_screen(gpu_up, key, status):
    term = gpu_up()
    term.wait(lambda: any("gpu-kernels" in line for line in term.lines))
    term.press(key)
    assert term.exit_status() == status


def test_ctrl_c_quits_while_an_offer_search_is_still_running(gpu_up):
    term = gpu_up()
    term.wait(lambda: any("gpu-kernels" in line for line in term.lines))
    term.press(ENTER, until=term.on("Mode"))
    term.press(ENTER, until=term.on("GPU"))
    for char in "4090":
        term.press(char)
    term.press(ENTER, until=term.on("Offer: RTX 4090"))
    start = time.perf_counter()
    term.press("\x03")
    assert term.exit_status() == 130
    assert time.perf_counter() - start < 1.0
