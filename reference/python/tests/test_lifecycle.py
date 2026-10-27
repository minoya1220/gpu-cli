"""Lifecycle verification uses only fake provider state and a fake clock."""
import json

import pytest

from gpu import cli


@pytest.mark.parametrize("action", ["stop", "destroy"])
@pytest.mark.parametrize("outcome", ["success", "timeout", "failure", "poll_failure"])
def test_lifecycle_verifies_before_reporting(monkeypatch, tmp_path, capsys, action, outcome):
    monkeypatch.setenv("HOME", str(tmp_path))
    clock = [0.0]
    monkeypatch.setattr(cli.time, "monotonic", lambda: clock[0])
    monkeypatch.setattr(cli.time, "sleep", lambda seconds: clock.__setitem__(0, clock[0] + seconds))
    calls = []
    polls = 0
    requested = False
    removed = []
    monkeypatch.setattr(cli, "remove_host", removed.append)

    def run(argv, **kwargs):
        nonlocal polls, requested
        calls.append(argv)
        assert 0 < kwargs["timeout"] <= 60
        if argv[1] == action:
            requested = True
            if outcome == "failure":
                raise RuntimeError("provider refused request")
            return "success"
        assert argv == ["vastai", "show", "instances", "--raw"]
        if requested:
            polls += 1
            if outcome == "poll_failure":
                raise RuntimeError("provider unavailable")
        if outcome == "success" and polls >= 3:
            if action == "destroy":
                return "[]"
            status = "stopped"
        else:
            status = "running"
        return json.dumps([{"id": 12, "gpu_name": "RTX 4090", "actual_status": status}])

    monkeypatch.setattr(cli, "run", run)
    args = ["down", "12", "--yes"] if action == "destroy" else ["stop", "12"]
    assert cli.main(args) == (0 if outcome == "success" else 1)
    output = capsys.readouterr()
    if outcome == "success":
        assert polls == 3
        assert "Instance 12 (RTX 4090)" in output.out
        assert ("destroyed; billing stopped." if action == "destroy" else "stopped; storage charges may continue.") in output.out
        assert removed == (["12"] if action == "destroy" else [])
    else:
        assert not output.out
        assert "Instance 12" in output.err
        assert not removed
        if outcome == "timeout":
            assert clock[0] == 60
            assert "not verified" in output.err
        if outcome == "failure":
            assert polls == 0
            assert "failed" in output.err


def test_stop_disappearance_is_not_stopped(monkeypatch):
    clock = [0.0]
    monkeypatch.setattr(cli.time, "monotonic", lambda: clock[0])
    monkeypatch.setattr(cli.time, "sleep", lambda seconds: clock.__setitem__(0, clock[0] + seconds))
    monkeypatch.setattr(cli, "list_instances", lambda **kwargs: [])
    with pytest.raises(RuntimeError, match="stop not verified"):
        cli.wait_for_lifecycle("12", "stop", timeout=2)
