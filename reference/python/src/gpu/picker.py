"""Keyboard picker: one prompt_toolkit Application hosting a stack of screens.

Moving between screens never leaves the running Application, so a transition
costs one redraw and no keypress can fall between two terminal sessions. Slow
work (GitHub and Vast CLI calls) runs in `Job`s off the event loop, so every
key, including ` (back) and Ctrl-C, is handled immediately even while a screen
is still loading.
"""

from __future__ import annotations

import asyncio
import threading
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from functools import wraps
from typing import Any, Generic, TypeVar

from prompt_toolkit.application import Application
from prompt_toolkit.formatted_text import StyleAndTextTuples
from prompt_toolkit.key_binding import KeyBindings, KeyPressEvent
from prompt_toolkit.keys import Keys
from prompt_toolkit.layout import Layout, ScrollOffsets
from prompt_toolkit.layout.containers import HSplit, Window
from prompt_toolkit.layout.controls import FormattedTextControl
from prompt_toolkit.styles import Style

T = TypeVar("T")

STYLE = Style.from_dict(
    {
        "title": "bold",
        "filter": "ansicyan",
        "selected": "bold reverse",
        "note": "italic",
        "error": "ansired",
    }
)


class PickerCancelled(Exception):
    """The user quit; `status` is the process exit status."""

    def __init__(self, status: int):
        super().__init__(status)
        self.status = status


class Job(Generic[T]):
    """A blocking call run in the background; the picker redraws when it settles.

    It runs on a daemon thread rather than an executor so that quitting never
    waits for a slow subprocess to finish.
    """

    def __init__(self, fn: Callable[[], T]):
        self._fn = fn
        self.state = "idle"  # idle -> running -> done | failed; failed may retry
        self.result: T | None = None
        self.error = ""
        self._listeners: list[Callable[[], None]] = []

    def start(self, on_settled: Callable[[], None]) -> None:
        """Start unless already running or done. Call from the event loop thread."""
        if self.state == "running":
            self._listeners.append(on_settled)
            return
        if self.state == "done":
            return
        self._listeners = [on_settled]
        self.state = "running"
        loop = asyncio.get_running_loop()

        def settle(result: T | None, error: str) -> None:
            self.result, self.error = result, error
            self.state = "failed" if error else "done"
            for listener in self._listeners:
                listener()
            self._listeners = []

        def work() -> None:
            try:
                outcome = (self._fn(), "")
            # Thread boundary: an escaping error would leave the screen loading forever.
            except Exception as exc:  # noqa: BLE001 - shown on the waiting screen
                outcome = (None, str(exc).strip() or type(exc).__name__)
            try:
                loop.call_soon_threadsafe(settle, *outcome)
            except RuntimeError:
                pass  # The picker exited and closed its loop; nobody needs the result.

        threading.Thread(target=work, name="gpu-picker-job", daemon=True).start()


@dataclass(frozen=True)
class Option:
    value: Any
    label: str


@dataclass(eq=False)
class Screen:
    """One step of a picker flow.

    `options` is called on every redraw, so labels can reflect data that loads
    later. `select` returns the next Screen, or the flow's final result.
    """

    title: str
    options: Callable[[], Sequence[Option]]
    select: Callable[[Any], Any]
    # Options searched instead of `options` while a filter is typed.
    search: Callable[[], Sequence[Option]] | None = None
    # Must finish before options are shown; starts when the screen first opens.
    load: Job[Any] | None = None
    loading: str = "Loading..."
    empty: str = "Nothing to choose from"
    # F2 starts it; `options` should read its result.
    prices: Job[Any] | None = None
    # Digits 1-9 choose directly on short, unfiltered lists instead of filtering.
    quick_select: bool = True
    # Optional background load: static choices remain usable while it runs/fails.
    load_required: bool = True
    # Text entry shares the same app/stack; validation returns an inline error.
    text_input: bool = False
    validate: Callable[[str], None] | None = None
    on_settled: Callable[[], Any] | None = None
    notice: str = ""
    wait_initial: bool = False


@dataclass(eq=False)
class _View:
    """A screen on the navigation stack with its filter and cursor."""

    screen: Screen
    filter: str = ""
    cursor: int = 0
    error: str = ""

    @property
    def ready(self) -> bool:
        if self.screen.wait_initial and self.screen.load and self.screen.load.state in ("idle", "running"):
            return False
        return not self.screen.load_required or self.screen.load is None or self.screen.load.state == "done"

    def entries(self) -> list[Option]:
        if not self.ready:
            return []
        source = self.screen.search if self.filter and self.screen.search else self.screen.options
        needle = self.filter.casefold()
        return [option for option in source() if needle in option.label.casefold()]

    def current(self, entries: list[Option]) -> int:
        return min(self.cursor, len(entries) - 1)

    def quick_select(self, entries: list[Option]) -> bool:
        return self.screen.quick_select and not self.filter and 0 < len(entries) <= 9


class Picker:
    """Runs a flow of Screens in a single Application.

    `/Left clears the filter, else goes back, else arms a second-press quit.
    Enter/Right selects, including in the append-only URL entry.
    Ctrl-C and Ctrl-D quit from any screen.
    """

    def __init__(self, first: Screen, *, input=None, output=None):
        self._stack = [_View(first)]
        self._quit_pending = False
        self.app: Application[Any] = Application(
            layout=Layout(
                HSplit(
                    [
                        Window(FormattedTextControl(self._header), dont_extend_height=True),
                        Window(
                            FormattedTextControl(self._body, show_cursor=False),
                            wrap_lines=True,
                            dont_extend_height=True,
                            scroll_offsets=ScrollOffsets(top=1, bottom=1),
                        ),
                        Window(FormattedTextControl(self._footer), height=1),
                    ]
                )
            ),
            key_bindings=self._key_bindings(),
            style=STYLE,
            full_screen=False,
            erase_when_done=True,
            mouse_support=False,
            input=input,
            output=output,
        )

    @property
    def title(self) -> str:
        return self._view.screen.title

    @property
    def filter(self) -> str:
        return self._view.filter

    @property
    def labels(self) -> list[str]:
        """Labels currently listed, after filtering."""
        return [option.label for option in self._view.entries()]

    @property
    def _view(self) -> _View:
        return self._stack[-1]

    def run(self) -> Any:
        return self.app.run(pre_run=self._open)

    async def run_async(self) -> Any:
        return await self.app.run_async(pre_run=self._open)

    def _open(self, *, retry: bool = False) -> None:
        screen = self._view.screen
        load = screen.load
        if load and (retry or load.state in ("idle", "running")):
            def settled() -> None:
                if screen.on_settled:
                    replacement = screen.on_settled()
                    if isinstance(replacement, Screen):
                        self._stack = [_View(replacement)]
                        self._open()
                    elif replacement is not None:
                        self.app.exit(result=replacement)
                self.app.invalidate()

            load.start(settled)

    def _choose(self, value: Any) -> None:
        following = self._view.screen.select(value)
        if isinstance(following, Screen):
            self._stack.append(_View(following))
            self._open()
        else:
            self.app.exit(result=following)

    def _key_bindings(self) -> KeyBindings:
        kb = KeyBindings()

        @kb.add("up")
        def _up(event: KeyPressEvent) -> None:
            self._move(-1)

        @kb.add("down")
        def _down(event: KeyPressEvent) -> None:
            self._move(1)

        @kb.add("right")
        @kb.add("enter")
        def _enter(event: KeyPressEvent) -> None:
            view = self._view
            if view.screen.text_input:
                try:
                    if view.screen.validate:
                        view.screen.validate(view.filter)
                except ValueError as exc:
                    view.error = str(exc)
                    return
                self._choose(view.filter)
                return
            if view.screen.load and view.screen.load.state == "failed" and view.screen.load_required:
                self._open(retry=True)  # retry
                return
            entries = view.entries()
            if entries:
                self._choose(entries[view.current(entries)].value)

        @kb.add("left")
        @kb.add("`")
        def _back(event: KeyPressEvent) -> None:
            if self._view.filter:
                self._view.filter, self._view.cursor, self._view.error = "", 0, ""
            elif len(self._stack) > 1:
                self._stack.pop()
            elif self._quit_pending:
                event.app.exit(exception=PickerCancelled(1))
            else:
                self._quit_pending = True

        @kb.add("backspace")
        def _backspace(event: KeyPressEvent) -> None:
            self._view.filter, self._view.cursor, self._view.error = self._view.filter[:-1], 0, ""

        @kb.add("c-r")
        def _retry(event: KeyPressEvent) -> None:
            self._open(retry=True)

        @kb.add("f2")
        def _prices(event: KeyPressEvent) -> None:
            if self._view.screen.prices:
                self._view.screen.prices.start(self.app.invalidate)

        @kb.add("c-c", eager=True)
        def _interrupt(event: KeyPressEvent) -> None:
            event.app.exit(exception=PickerCancelled(130))

        @kb.add("c-d", eager=True)
        def _eof(event: KeyPressEvent) -> None:
            event.app.exit(exception=PickerCancelled(1))

        @kb.add("<bracketed-paste>")
        def _paste(event: KeyPressEvent) -> None:
            view = self._view
            text = event.data.strip()
            if all(char.isprintable() for char in text):
                view.filter, view.cursor, view.error = view.filter + text, 0, ""

        @kb.add("escape", eager=True)
        @kb.add("<any>")
        def _type(event: KeyPressEvent) -> None:
            # The parser already recognizes special keys, but their data contains
            # the raw escape sequence. Never treat that payload as typed text.
            if isinstance(event.key_sequence[-1].key, Keys):
                return
            text = event.data
            if not text or not text.isprintable():
                return
            view = self._view
            if len(text) == 1 and text in "123456789":
                entries = view.entries()
                if view.quick_select(entries) and int(text) <= len(entries):
                    self._choose(entries[int(text) - 1].value)
                    return
            view.filter, view.cursor, view.error = view.filter + text, 0, ""

        def cancel_pending_quit(handler):
            @wraps(handler)
            def handle(event: KeyPressEvent):
                if event.key_sequence[-1].key not in ("`", Keys.Left):
                    self._quit_pending = False
                return handler(event)

            return handle

        # Include ignored keys: any intervening key disarms first-screen quit.
        for binding in kb.bindings:
            binding.handler = cancel_pending_quit(binding.handler)
        return kb

    def _move(self, delta: int) -> None:
        entries = self._view.entries()
        if entries:
            self._view.cursor = (self._view.current(entries) + delta) % len(entries)

    def _header(self) -> StyleAndTextTuples:
        view = self._view
        header: StyleAndTextTuples = [("class:title", view.screen.title)]
        if view.filter:
            prefix = "Repository" if view.screen.text_input else "Filter"
            header.append(("class:filter", f"\n{prefix}: {view.filter}"))
        return header

    def _body(self) -> StyleAndTextTuples:
        view = self._view
        load = view.screen.load
        if view.screen.text_input:
            return [("class:error", view.error)] if view.error else [
                ("class:note", "  Type HTTPS/git@ URL or owner/name, then Enter/→")
            ]
        if load and load.state == "failed" and view.screen.load_required:
            return [("class:error", "\n".join(f"  {line}" for line in load.error.splitlines()))]
        if not view.ready:
            return [("class:note", f"  {view.screen.loading}")]
        entries = view.entries()
        if not entries:
            return [("class:note", "  No matches" if view.filter else f"  {view.screen.empty}")]
        current = view.current(entries)
        width = len(str(len(entries)))
        lines: StyleAndTextTuples = []
        for index, option in enumerate(entries):
            if index:
                lines.append(("", "\n"))
            if index == current:
                # Scrolls the list so the selected line stays visible.
                lines.append(("[SetCursorPosition]", ""))
                lines.append(("class:selected", f"› {index + 1:>{width}}. {option.label}"))
            else:
                lines.append(("", f"  {index + 1:>{width}}. {option.label}"))
        if view.screen.notice:
            lines.insert(0, ("class:error", f"  {view.screen.notice}\n"))
        if load and not view.screen.load_required:
            if load.state == "running":
                lines.append(("class:note", f"\n  {view.screen.loading}"))
            elif load.state == "failed":
                lines.append(("class:error", f"\n  {load.error}"))
        return lines

    def _footer(self) -> str:
        view = self._view
        load = view.screen.load
        if view.screen.text_input:
            hints = ["Enter/→ continue", "Backspace edit"]
        elif load and load.state == "failed" and view.screen.load_required:
            hints = ["Enter/→ retry"]
        else:
            hints = ["↑/↓ move", "Enter/→ select"]
            if view.quick_select(view.entries()):
                hints.append("1-9 select")
        if view.filter:
            hints.append("`/← clear")
        elif self._quit_pending:
            hints.append("Press `/← again to quit")
        else:
            hints.append("`/← back" if len(self._stack) > 1 else "`/← quit (twice)")
        if load and load.state == "failed" and not view.screen.load_required:
            hints.append("Ctrl-R retry repositories")
        prices = view.screen.prices
        if prices and prices.state == "idle":
            hints.append("F2 prices")
        elif prices and prices.state == "running":
            hints.append("Loading prices...")
        elif prices and prices.state == "failed":
            hints.append(f"F2 retry prices ({prices.error.splitlines()[0]})")
        return "  ".join(hints)


def pick(first: Screen, *, input=None, output=None) -> Any:
    """Run a picker flow from `first`; return its result or raise PickerCancelled."""
    return Picker(first, input=input, output=output).run()
