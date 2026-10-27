"""Cached repository selection must never launch without fresh GitHub validation."""

import threading

import pytest

from gpu.picker import Job, PickerCancelled
from gpu.repos import load_cached_repos, save_cached_repos
from gpu.wizard import LaunchWizard, Picks


def flow(job):
    return LaunchWizard(repos=job, search=None, models=[], recent_gpus=[], choose_offer=False)


def test_cached_display_and_wait_before_launch(drive):
    save_cached_repos("minoya1220", ["old"])
    gate = threading.Event()
    job = Job(lambda: gate.wait(5) and ["old", "new"])

    async def script(keys):
        assert keys.picker.labels == ["No repository", "Clone from URL...", "old"]
        await keys.wait(lambda: job.state == "running")
        await keys.press("3", until=keys.on("Mode"))
        await keys.press("1", until=keys.on("Checking repositories"))
        assert not keys.picker.app.is_done
        gate.set()

    try:
        assert drive(flow(job).next(Picks(gpu="B200")), script) == Picks("old", "container", "B200")
    finally:
        gate.set()


def test_invalid_cache_resets_with_message_and_never_launches(drive):
    save_cached_repos("minoya1220", ["deleted"])
    gate = threading.Event()
    job = Job(lambda: gate.wait(5) and ["new"])

    async def script(keys):
        await keys.press("3", until=keys.on("Mode"))
        await keys.press("1", until=keys.on("Checking repositories"))
        gate.set()
        await keys.wait(lambda: keys.picker.title == "Repository")
        assert keys.picker._view.screen.notice == "Repository not available or deleted: deleted"
        assert keys.picker.labels == ["No repository", "Clone from URL...", "new"]
        await keys.press("\x03")

    try:
        with pytest.raises(PickerCancelled):
            drive(flow(job).next(Picks(gpu="B200")), script)
    finally:
        gate.set()


def test_sync_error_resets_without_launch(drive):
    save_cached_repos("minoya1220", ["old"])
    gate = threading.Event()

    def fail():
        assert gate.wait(5)
        raise RuntimeError("GitHub unavailable")

    job = Job(fail)

    async def script(keys):
        await keys.press("3", until=keys.on("Mode"))
        await keys.press("1", until=keys.on("Checking repositories"))
        gate.set()
        await keys.wait(lambda: keys.picker.title == "Repository")
        assert "GitHub unavailable" in keys.picker._view.screen.notice
        await keys.press("\x03")

    try:
        with pytest.raises(PickerCancelled):
            drive(flow(job).next(Picks(gpu="B200")), script)
    finally:
        gate.set()


def test_first_run_waits_for_sync(drive):
    gate = threading.Event()
    job = Job(lambda: gate.wait(5) and ["fresh"])

    async def script(keys):
        assert keys.picker.labels == []
        assert keys.picker._view.screen.loading == "Loading repositories..."
        gate.set()
        await keys.wait(lambda: "fresh" in keys.picker.labels)
        await keys.press("3")

    try:
        assert drive(flow(job).next(Picks(mode="vm", gpu="B200")), script).repo == "fresh"
    finally:
        gate.set()


def test_sync_rewrites_cache_with_names_only(monkeypatch):
    from gpu import cli

    monkeypatch.setattr(cli, "github_repos", lambda owner: ["fresh"])
    save_cached_repos("minoya1220", ["old"])
    assert cli.sync_repos("minoya1220") == ["fresh"]
    assert load_cached_repos("minoya1220") == ["fresh"]
