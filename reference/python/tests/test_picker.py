"""The picker engine, driven with real key presses through prompt_toolkit pipe input."""

from __future__ import annotations

import threading
import time

import pytest

from gpu.picker import Job, Option, PickerCancelled, Screen

ENTER, BACK, DOWN, UP, F2, ESC = "\r", "`", "\x1b[B", "\x1b[A", "\x1b[12~", "\x1b"
LEFT, RIGHT, BACKSPACE = "\x1b[D", "\x1b[C", "\x7f"
# Home, End, PageUp, PageDown, Insert, Delete, F-keys except F2, Shift-arrows, Ctrl-Left.
SPECIAL_KEYS = [
    "\x1b[H", "\x1b[F", "\x1b[1~", "\x1b[4~", "\x1b[5~", "\x1b[6~", "\x1b[2~", "\x1b[3~",
    "\x1bOP", "\x1bOR", "\x1bOS", "\x1b[15~", "\x1b[17~", "\x1b[18~", "\x1b[19~",
    "\x1b[20~", "\x1b[21~", "\x1b[23~", "\x1b[24~",
    "\x1b[1;2D", "\x1b[1;2C", "\x1b[1;2A", "\x1b[1;2B", "\x1b[1;5D",
]


def menu(title, values, then=lambda value: value, **kwargs):
    return Screen(title, options=lambda: [Option(v, str(v)) for v in values], select=then, **kwargs)


def chain():
    """Three screens; the result records the value chosen on each."""
    return menu(
        "First",
        ["a", "b"],
        lambda first: menu(
            "Second",
            ["c", "d"],
            lambda second: menu("Third", ["e", "f"], lambda third: (first, second, third)),
        ),
    )


def gated(values, gate, calls=None):
    """A loader that blocks until `gate` is set, like a slow gh or vastai call."""

    def load():
        if calls is not None:
            calls.append(True)
        assert gate.wait(5), "test never released the loader"
        return values

    return load


def test_typing_filters_and_enter_selects_the_highlighted_match(drive):
    async def script(keys):
        await keys.type("alp")
        await keys.press(DOWN)
        await keys.press(ENTER)

    assert drive(menu("Choose", ["alpha", "beta", "alphabet"]), script) == "alphabet"


def test_digits_quick_select_only_short_unfiltered_lists(drive):
    async def pick_second(keys):
        await keys.press("2")

    assert drive(menu("Choose", ["one", "two"]), pick_second) == "two"

    names = ["Tesla T4", "RTX 4090", *[f"GPU {n}" for n in range(10)]]

    async def filter_4090(keys):
        await keys.type("4090")
        await keys.press(ENTER)

    assert drive(menu("Long", names), filter_4090) == "RTX 4090"
    # GPU screens keep digits as filter text even when the list is short.
    assert drive(menu("GPU", names[:2], quick_select=False), filter_4090) == "RTX 4090"


def test_search_list_replaces_options_while_filtering(drive):
    models = ["Tesla T4", "RTX 4090"]
    screen = menu(
        "GPU",
        ["Datacenter", "Consumer"],
        search=lambda: [Option(m, m) for m in models],
        quick_select=False,
    )

    async def script(keys):
        await keys.type("t4")
        await keys.press(ENTER)

    assert drive(screen, script) == "Tesla T4"


@pytest.mark.parametrize("back", [BACK, LEFT], ids=["backtick", "left"])
def test_back_clears_filter_then_goes_back_one_screen_then_quits_on_first_twice(drive, back):
    async def script(keys):
        await keys.press(DOWN)
        await keys.press(ENTER, until=keys.on("Second"))
        await keys.press(ENTER, until=keys.on("Third"))
        await keys.type("x")
        await keys.press(back, until=lambda: keys.picker.filter == "")
        assert keys.picker.title == "Third"
        await keys.press(back, until=keys.on("Second"))
        await keys.press(back, until=keys.on("First"))
        await keys.press(back, until=lambda: "again to quit" in keys.picker._footer())
        await keys.press(back)

    with pytest.raises(PickerCancelled) as cancelled:
        drive(chain(), script)
    assert cancelled.value.status == 1


def test_going_back_restores_the_previous_screen_as_it_was_left(drive):
    async def script(keys):
        await keys.press(DOWN)
        await keys.press(ENTER, until=keys.on("Second"))
        await keys.press(BACK, until=keys.on("First"))
        await keys.press(ENTER, until=keys.on("Second"))
        await keys.press(ENTER, until=keys.on("Third"))
        await keys.press(ENTER)

    assert drive(chain(), script) == ("b", "c", "e")


@pytest.mark.parametrize("enter", [ENTER, RIGHT], ids=["enter", "right"])
def test_right_selects_like_enter(drive, enter):
    async def script(keys):
        await keys.press(DOWN)
        await keys.press(enter, until=keys.on("Second"))
        await keys.type("d")
        await keys.press(enter, until=keys.on("Third"))
        await keys.press(enter)

    assert drive(chain(), script) == ("b", "d", "e")


@pytest.mark.parametrize("other", [DOWN, "e", ESC, "\x1b[H", "\t", "\x0c", "\x01"],
                         ids=["down", "letter", "esc", "home", "tab", "ctrl-l", "ctrl-a"])
def test_one_back_then_another_key_on_the_first_screen_does_not_quit(drive, other):
    async def script(keys):
        assert "`/← quit" in keys.picker._footer()
        await keys.press(BACK, until=lambda: "Press `/← again to quit" in keys.picker._footer())
        await keys.press(other, until=lambda: "again to quit" not in keys.picker._footer())
        await keys.press(BACKSPACE)  # drops the typed letter, if any
        await keys.press(BACK, until=lambda: "again to quit" in keys.picker._footer())
        await keys.press(DOWN, until=lambda: "again to quit" not in keys.picker._footer())
        await keys.press(BACK, until=lambda: "again to quit" in keys.picker._footer())
        await keys.press(RIGHT, until=keys.on("Second"))
        await keys.press(ENTER, until=keys.on("Third"))
        await keys.press(ENTER)

    assert drive(chain(), script)[1:] == ("c", "e")


def test_backtick_then_left_quits_the_first_screen(drive):
    async def script(keys):
        await keys.press(BACK, until=lambda: "again to quit" in keys.picker._footer())
        await keys.press(LEFT)

    with pytest.raises(PickerCancelled) as cancelled:
        drive(chain(), script)
    assert cancelled.value.status == 1


def test_special_keys_never_leak_into_the_filter_or_url_entry(drive):
    def url_screen():
        return Screen("URL", options=list, select=lambda text: text, text_input=True, quick_select=False)

    screen = menu("First", ["alpha", "beta"], lambda value: url_screen())

    async def script(keys):
        await keys.type("a")
        for key in SPECIAL_KEYS:
            await keys.press(key)
        assert keys.picker.filter == "a"
        assert keys.picker.title == "First"
        await keys.press(RIGHT, until=keys.on("URL"))
        await keys.type("o/r")
        for key in SPECIAL_KEYS:
            await keys.press(key)
        assert keys.picker.filter == "o/r"
        await keys.press(RIGHT)

    assert drive(screen, script) == "o/r"


def test_key_hints_leave_out_ctrl_c(drive):
    async def script(keys):
        assert "Ctrl-C" not in keys.picker._footer()
        await keys.press(ENTER, until=keys.on("Second"))
        assert "Ctrl-C" not in keys.picker._footer()
        await keys.press("\x03")

    with pytest.raises(PickerCancelled):
        drive(chain(), script)


@pytest.mark.parametrize("first_screen", [True, False], ids=["first", "deep"])
@pytest.mark.parametrize(("key", "status"), [("\x03", 130), ("\x04", 1)])
def test_ctrl_c_and_ctrl_d_quit_at_once_even_with_a_quit_pending(drive, key, status, first_screen):
    async def script(keys):
        if first_screen:
            await keys.press(BACK, until=lambda: "again to quit" in keys.picker._footer())
        else:
            await keys.press(ENTER, until=keys.on("Second"))
        await keys.press(key)

    with pytest.raises(PickerCancelled) as cancelled:
        drive(chain(), script)
    assert cancelled.value.status == status


@pytest.mark.parametrize(("key", "status"), [("\x03", 130), ("\x04", 1)])
def test_ctrl_c_and_ctrl_d_quit_from_a_deep_screen(drive, key, status):
    async def script(keys):
        await keys.press(ENTER, until=keys.on("Second"))
        await keys.press(ENTER, until=keys.on("Third"))
        await keys.press(key)

    with pytest.raises(PickerCancelled) as cancelled:
        drive(chain(), script)
    assert cancelled.value.status == status


def test_escape_is_ignored_and_never_leaks_into_navigation(drive):
    async def script(keys):
        await keys.press(ENTER, until=keys.on("Second"))
        await keys.press(ESC + ESC)
        await keys.press(ESC)
        assert keys.picker.title == "Second"
        await keys.press(BACK, until=keys.on("First"))
        await keys.press(DOWN)
        await keys.press(ENTER, until=keys.on("Second"))
        await keys.press(ENTER, until=keys.on("Third"))
        await keys.press(ENTER)

    assert drive(chain(), script) == ("b", "c", "e")


def test_arrow_key_split_across_reads_still_moves(drive):
    async def script(keys):
        await keys.press("\x1b")
        await keys.press("[")
        await keys.press("B")
        await keys.press(ENTER)

    assert drive(menu("Choose", ["one", "two"]), script) == "two"


def test_loading_screen_stays_responsive_while_its_loader_blocks(drive):
    gate, calls = threading.Event(), []
    job = Job(gated(["late"], gate, calls))
    slow = Screen("Slow", options=lambda: [Option(v, v) for v in job.result], select=lambda v: v, load=job)
    first = menu("First", ["go"], lambda _: slow)

    async def script(keys):
        latency = await keys.press(ENTER, until=keys.on("Slow"))
        assert latency < 0.5
        await keys.wait(lambda: job.state == "running")
        await keys.press(ENTER)  # nothing to select yet
        assert keys.picker.title == "Slow"
        await keys.type("la")
        await keys.press(BACK, until=lambda: keys.picker.filter == "")
        latency = await keys.press(BACK, until=keys.on("First"))
        assert latency < 0.5 and job.state == "running"
        await keys.press(ENTER, until=keys.on("Slow"))
        gate.set()
        await keys.wait(lambda: job.state == "done")
        await keys.press(ENTER)

    try:
        assert drive(first, script) == "late"
    finally:
        gate.set()
    assert calls == [True]


def test_failed_load_shows_error_and_enter_retries(drive):
    attempts = []

    def flaky():
        attempts.append(True)
        if len(attempts) == 1:
            raise RuntimeError("gh: not logged in")
        return ["repo"]

    job = Job(flaky)
    screen = Screen("Repository", options=lambda: [Option(v, v) for v in job.result], select=lambda v: v, load=job)

    async def script(keys):
        await keys.wait(lambda: job.state == "failed")
        assert job.error == "gh: not logged in"
        await keys.press(ENTER, until=lambda: job.state == "done")
        await keys.press(ENTER)

    assert drive(screen, script) == "repo"
    assert len(attempts) == 2


def test_f2_loads_prices_in_the_background_once(drive):
    gate, calls = threading.Event(), []
    prices = Job(gated({"one": "$1"}, gate, calls))

    def options():
        stats = prices.result or {}
        return [Option(v, f"{v} {stats.get(v, '')}".strip()) for v in ["one", "two"]]

    screen = Screen("GPU", options=options, select=lambda v: v, prices=prices, quick_select=False)

    async def script(keys):
        await keys.press(F2, until=lambda: prices.state == "running")
        # Keys still work while the price search blocks.
        await keys.type("tw")
        await keys.press(BACK, until=lambda: keys.picker.filter == "")
        gate.set()
        await keys.wait(lambda: prices.state == "done")
        assert [o.label for o in options()] == ["one $1", "two"]
        await keys.press(F2)
        await keys.press(ENTER)

    try:
        assert drive(screen, script) == "one"
    finally:
        gate.set()
    assert calls == [True]


def test_quitting_does_not_wait_for_a_running_loader(drive):
    gate = threading.Event()
    job = Job(gated([], gate))
    screen = Screen("Offer", options=list, select=lambda v: v, load=job)

    async def script(keys):
        await keys.wait(lambda: job.state == "running")
        await keys.press("\x03")

    start = time.perf_counter()
    try:
        with pytest.raises(PickerCancelled):
            drive(screen, script)
    finally:
        gate.set()
    assert time.perf_counter() - start < 2
