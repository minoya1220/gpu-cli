"""The `gpu up` screen flow, driven with real key presses through pipe input."""

from __future__ import annotations

import threading

import pytest

from gpu.picker import Job, PickerCancelled
from gpu.wizard import LaunchWizard, Picks

ENTER, BACK, DOWN, F2 = "\r", "`", "\x1b[B", "\x1b[12~"
MODELS = ["B200", "H100 SXM", "RTX 4090", "RTX 5090", "Tesla T4"]


def offer(offer_id, gpu, price):
    return {
        "id": offer_id,
        "gpu_name": gpu,
        "num_gpus": 1,
        "dph_total": price,
        "reliability": 0.99,
        "geolocation": "Test, XX",
        "verification": "verified",
    }


class FakeSearch:
    """Stands in for OfferSearch; records calls and can hold offer searches open."""

    def __init__(self):
        self.offer_calls = []
        self.stats_calls = []
        self.gate = threading.Event()
        self.gate.set()

    def offers(self, mode, gpu=None):
        self.offer_calls.append((mode, gpu))
        assert self.gate.wait(5), "test never released the offer search"
        return [offer(1, gpu, 0.5), offer(2, gpu, 0.3)]

    def stats(self, mode):
        self.stats_calls.append(mode)
        return {"B200": (1.735, 1), "RTX 4090": (0.3, 12)}


@pytest.fixture
def repo_calls():
    return []


def wizard(search, repo_calls, *, recent=(), choose_offer=True):
    def repos():
        repo_calls.append(True)
        return ["gpu-kernels", "other"]

    return LaunchWizard(
        repos=Job(repos),
        search=search,
        models=MODELS,
        recent_gpus=list(recent),
        choose_offer=choose_offer,
    )


def run(drive, flow, picks, script):
    return drive(flow.next(picks), script)


def test_back_walks_every_screen_without_refetching_or_quitting(drive, repo_calls):
    search = FakeSearch()

    async def script(keys):
        await keys.wait(lambda: keys.picker.labels == ["No repository", "Clone from URL...", "gpu-kernels", "other"])
        await keys.press(ENTER, until=keys.on("Mode"))
        await keys.press(ENTER, until=keys.on("GPU"))
        await keys.press(ENTER, until=keys.on("Datacenter Blackwell/Hopper"))
        await keys.press(ENTER, until=keys.on("Offer: B200"))
        for title in ["Datacenter Blackwell/Hopper", "GPU", "Mode", "Repository"]:
            latency = await keys.press(BACK, until=keys.on(title))
            assert latency < 0.5
        # Forward again with different choices; earlier picks must not leak.
        await keys.press("4", until=keys.on("Mode"))
        await keys.press(DOWN)
        await keys.press(ENTER, until=keys.on("GPU"))
        await keys.type("4090")
        await keys.press(ENTER, until=keys.on("Offer: RTX 4090"))
        await keys.wait(lambda: search.offer_calls[-1] == ("vm", "RTX 4090"))
        await keys.press(ENTER)

    picks = run(drive, wizard(search, repo_calls), Picks(), script)
    assert picks == Picks("other", "vm", "RTX 4090", offer(1, "RTX 4090", 0.5))
    assert repo_calls == [True]


def test_backtick_twice_on_the_first_real_screen_quits(drive, repo_calls):
    async def back_from_gpu_category(keys):
        await keys.press(ENTER, until=keys.on("Datacenter Blackwell/Hopper"))
        await keys.press(BACK, until=keys.on("GPU"))
        await keys.press(BACK)
        await keys.press(BACK)

    # --repo and --mode given: GPU is the first screen.
    with pytest.raises(PickerCancelled) as cancelled:
        run(drive, wizard(FakeSearch(), repo_calls), Picks("gpu-kernels", "container"), back_from_gpu_category)
    assert cancelled.value.status == 1
    assert repo_calls == []


def test_flags_for_every_pick_skip_the_picker():
    flow = wizard(FakeSearch(), [], choose_offer=False)
    assert flow.run(Picks("gpu-kernels", "vm", "RTX 4090")) == Picks("gpu-kernels", "vm", "RTX 4090")


def test_offer_search_never_blocks_navigation(drive, repo_calls):
    search = FakeSearch()
    search.gate.clear()

    async def script(keys):
        await keys.type("t4")
        latency = await keys.press(ENTER, until=keys.on("Offer: Tesla T4"))
        assert latency < 0.5
        await keys.wait(lambda: search.offer_calls == [("container", "Tesla T4")])
        await keys.press(ENTER)  # still searching: nothing to select
        assert keys.picker.title == "Offer: Tesla T4"
        latency = await keys.press(BACK, until=keys.on("GPU"))
        assert latency < 0.5
        await keys.press("\x03")

    try:
        with pytest.raises(PickerCancelled) as cancelled:
            run(drive, wizard(search, repo_calls), Picks("gpu-kernels", "container"), script)
    finally:
        search.gate.set()
    assert cancelled.value.status == 130


def test_f2_prices_are_shared_by_gpu_screens_and_loaded_once_per_mode(drive, repo_calls):
    search = FakeSearch()
    flow = wizard(search, repo_calls, choose_offer=False)
    gpu_labels = []

    async def script(keys):
        await keys.wait(lambda: "gpu-kernels" in keys.picker.labels)
        await keys.press(ENTER, until=keys.on("Mode"))
        await keys.press(ENTER, until=keys.on("GPU"))
        await keys.press(F2)
        await keys.wait(lambda: search.stats_calls == ["container"])
        await keys.wait(lambda: "(1 model, from $1.735/hr)" in " ".join(keys.picker.labels))
        await keys.press(ENTER, until=keys.on("Datacenter Blackwell/Hopper"))
        gpu_labels.extend(keys.picker.labels)
        await keys.press(F2)
        await keys.press(BACK, until=keys.on("GPU"))
        await keys.press(BACK, until=keys.on("Mode"))
        await keys.press(DOWN)
        await keys.press(ENTER, until=keys.on("GPU"))
        assert "from $" not in " ".join(keys.picker.labels)  # vm prices not loaded yet
        await keys.press(F2)
        await keys.wait(lambda: search.stats_calls == ["container", "vm"])
        await keys.type("4090")
        await keys.press(ENTER)

    assert run(drive, flow, Picks(), script) == Picks("", "vm", "RTX 4090")
    assert gpu_labels == ["B200  $1.735/hr (1 offer)", "H100 SXM"]


def test_recent_gpus_come_first_and_digits_filter_gpu_names(drive, repo_calls):
    flow = wizard(FakeSearch(), repo_calls, recent=["RTX 4090", "Retired GPU"], choose_offer=False)

    async def pick_recent(keys):
        await keys.press(ENTER)

    assert run(drive, flow, Picks("r", "container"), pick_recent) == Picks("r", "container", "RTX 4090")

    async def filter_by_digits(keys):
        await keys.type("5090")
        await keys.press(ENTER)

    assert run(drive, flow, Picks("r", "container"), filter_by_digits) == Picks("r", "container", "RTX 5090")
