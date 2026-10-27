"""No test in this file reaches the live Vast account, SSH, or Cursor."""
import argparse
import json
import subprocess
from pathlib import Path

import pytest

from gpu import cli, editors, instances


@pytest.fixture(autouse=True)
def available_gui_cli(monkeypatch):
    monkeypatch.setattr(editors.shutil, "which", lambda command: f"/bin/{command}")

INSTANCES = [
    {"id": 12, "gpu_name": "RTX 4090", "actual_status": "running", "dph_total": 0.5,
     "duration": 7342.81, "label": "ThunderKittens", "ssh_host": "ssh6.vast.ai", "ssh_port": 12345},
    {"id": 34, "gpu_name": "H100", "actual_status": "loading", "dph_total": 1.2,
     "duration": 60.5, "label": "no-repo", "ssh_host": "ssh7.vast.ai", "ssh_port": 2222},
]


def fake_vast(monkeypatch, instances=INSTANCES):
    calls = []
    live = [dict(item) for item in instances]

    def run(argv, **kwargs):
        calls.append(argv)
        if argv[:3] == ["vastai", "show", "instances"]:
            return json.dumps(live)
        if argv[1] == "destroy":
            live[:] = [item for item in live if str(item["id"]) != argv[3]]
        if argv[1] == "stop":
            for item in live:
                if str(item["id"]) == argv[3]:
                    item["actual_status"] = "stopped"
        return "ok"

    monkeypatch.setattr(cli, "run", run)
    return calls


@pytest.mark.parametrize("count", [0, 1, 2])
def test_picker_zero_one_many(monkeypatch, count, capsys):
    calls = fake_vast(monkeypatch, INSTANCES[:count])
    picked = []

    def select(screen):
        options = screen.options()
        picked.extend(options)
        return screen.select(options[-1].value)

    monkeypatch.setattr(instances, "pick", select)
    assert cli.main(["stop"]) == 0
    if count == 0:
        assert "No instances" in capsys.readouterr().out
        assert len(calls) == 1
    else:
        assert len(picked) == count
        assert "RTX 4090" in picked[0].label
        assert "running" in picked[0].label
        assert "$0.500/hr" in picked[0].label
        assert "2h 02m" in picked[0].label
        assert "7342" not in picked[0].label
        assert "ThunderKittens" in picked[0].label
        assert calls[-2] == ["vastai", "stop", "instance", str(INSTANCES[count - 1]["id"])]


@pytest.mark.parametrize("count", [1, 2])
def test_instance_picker_real_keys(drive, count):
    from gpu.picker import PickerCancelled

    async def cancel(keys):
        await keys.wait(lambda: len(keys.picker.labels) == count)
        await keys.press("`")
        await keys.press("`")

    with pytest.raises(PickerCancelled):
        drive(instances.Screen("Instances", options=lambda: [
            instances.Option(str(item["id"]), instances.instance_label(item))
            for item in INSTANCES[:count]
        ], select=lambda value: value), cancel)

    async def select(keys):
        await keys.wait(lambda: len(keys.picker.labels) == count)
        await keys.press("\r")

    screen = instances.Screen("Instances", options=lambda: [
        instances.Option(str(item["id"]), instances.instance_label(item))
        for item in INSTANCES[:count]
    ], select=lambda value: value)
    assert drive(screen, select) == "12"


def test_explicit_id_skips_picker(monkeypatch):
    calls = fake_vast(monkeypatch)
    monkeypatch.setattr(instances, "pick", lambda screen: pytest.fail("unexpected picker"))
    assert cli.main(["ssh", "12"]) == 0
    assert cli.main(["stop", "12"]) == 0
    assert calls == [["vastai", "ssh-url", "12"], ["vastai", "show", "instances", "--raw"],
                     ["vastai", "stop", "instance", "12"], ["vastai", "show", "instances", "--raw"]]


def test_down_selection_still_confirms_and_cancel_is_safe(monkeypatch, tmp_path):
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    calls = fake_vast(monkeypatch)
    monkeypatch.setattr(instances, "pick", lambda screen: screen.select(screen.options()[0].value))
    monkeypatch.setattr("builtins.input", lambda prompt: "no")
    assert cli.main(["down"]) == 0
    assert len(calls) == 1
    assert not (tmp_path / ".ssh").exists()
    managed = tmp_path / ".ssh" / "vst_config"
    instances.update_host(INSTANCES[0])
    instances.update_host(INSTANCES[1])
    monkeypatch.setattr("builtins.input", lambda prompt: "yes")
    assert cli.main(["down"]) == 0
    assert calls[-2] == ["vastai", "destroy", "instance", "12", "--yes"]
    assert "Host vast-12\n" not in managed.read_text()
    assert "Host vast-34\n" in managed.read_text()
    monkeypatch.setattr(instances, "pick", lambda screen: (_ for _ in ()).throw(cli.PickerCancelled(1)))
    assert cli.main(["down"]) == 1
    assert len(calls) == 6


def test_cursor_repo_and_fallback(monkeypatch, tmp_path, capsys):
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    fake_vast(monkeypatch, [INSTANCES[0], {**INSTANCES[1], "actual_status": "running"}])
    launched = []
    def run(argv, **kwargs):
        launched.append(argv)
        return subprocess.CompletedProcess(argv, 0)

    monkeypatch.setattr(cli.subprocess, "run", run)
    for instance_id, path in [("12", "/workspace/ThunderKittens"), ("34", "/workspace")]:
        assert cli.main(["cursor", instance_id]) == 0
        assert launched[-1] == ["cursor", "--remote", f"ssh-remote+vast-{instance_id}", path]
    assert "No repository recorded" in capsys.readouterr().out


def test_managed_ssh_config_preserves_user_bytes(monkeypatch, tmp_path):
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    ssh = tmp_path / ".ssh"
    ssh.mkdir()
    original = b"# user header\r\nHost *\r\n  User alice\r\nHost personal\r\n  HostName example.com\r\n"
    (ssh / "config").write_bytes(original)
    assert instances.update_host(INSTANCES[0]) == "vast-12"
    assert instances.update_host(INSTANCES[0]) == "vast-12"
    assert instances.update_host(INSTANCES[1]) == "vast-34"
    assert (ssh / "config").read_bytes() == b"Include ~/.ssh/vst_config\n" + original
    managed = ssh / "vst_config"
    assert managed.read_text().count("Host vast-12\n") == 1
    assert "Host vast-34\n" in managed.read_text()
    instances.remove_host("12")
    assert "Host vast-12\n" not in managed.read_text()
    assert "Host vast-34\n" in managed.read_text()
    assert (ssh / "config").read_bytes() == b"Include ~/.ssh/vst_config\n" + original
    assert "token" not in managed.read_text()


def test_ssh_wait_timeout(monkeypatch, tmp_path, capsys):
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    fake_vast(monkeypatch, [{**INSTANCES[0], "actual_status": "loading"}])
    clock = [0.0]
    monkeypatch.setattr(cli.time, "monotonic", lambda: clock[0])
    monkeypatch.setattr(cli.time, "sleep", lambda seconds: clock.__setitem__(0, clock[0] + seconds))
    monkeypatch.setattr(cli.subprocess, "Popen", lambda *a, **kw: pytest.fail("launched before ready"))
    with pytest.raises(RuntimeError, match="Timed out waiting"):
        cli.cursor_instance("12", timeout=4)
    assert "Waiting for instance" in capsys.readouterr().err


def test_ssh_probe_failure_times_out(monkeypatch, tmp_path):
    monkeypatch.setattr(Path, "home", classmethod(lambda cls: tmp_path))
    fake_vast(monkeypatch, [INSTANCES[0]])
    clock = [0.0]
    monkeypatch.setattr(cli.time, "monotonic", lambda: clock[0])
    monkeypatch.setattr(cli.time, "sleep", lambda seconds: clock.__setitem__(0, clock[0] + seconds))
    monkeypatch.setattr(cli.subprocess, "run", lambda *a, **kw: subprocess.CompletedProcess(a, 255))
    monkeypatch.setattr(cli.subprocess, "Popen", lambda *a, **kw: pytest.fail("launched before SSH"))
    with pytest.raises(RuntimeError, match="Timed out waiting"):
        cli.cursor_instance("12", timeout=4)


def test_onstart_repo_and_untrusted_label():
    assert instances.repo_name({"label": "../bad", "onstart": "git clone https://github.com/o/good /workspace/good\n"}) == "good"
    assert instances.repo_name({"label": "no-repo"}) is None


@pytest.mark.parametrize("editor", ["code", "none"])
def test_up_offers_editor_on_success(monkeypatch, tmp_path, editor):
    monkeypatch.setattr(cli, "interactive_terminal", lambda: True)
    monkeypatch.setattr(cli, "history_path", lambda: tmp_path / "history.json")
    monkeypatch.setattr(cli, "github_repos", lambda owner: ["ThunderKittens"])
    monkeypatch.setattr(cli, "run", lambda command: json.dumps({"success": True, "new_contract": 12}))
    monkeypatch.setattr(cli, "pick", lambda screen: editor)
    monkeypatch.setattr(cli, "edit_instance", lambda instance_id, editor, **kw: selected.append((instance_id, editor)))
    monkeypatch.setattr(cli, "confirm", lambda prompt: True)
    monkeypatch.setattr("builtins.input", lambda prompt: "")
    monkeypatch.setattr(cli.LaunchWizard, "run", lambda self, picks: cli.Picks("ThunderKittens", "container", "H100", {"id": 9, "dph_total": 1}))
    selected = []
    args = argparse.Namespace(repo="ThunderKittens", mode="container", gpu="H100", num_gpus=1,
                              max_price=None, interruptible=False, offer=9, disk=None, yes=False,
                              dry_run=False, reliability=None, unverified=False)
    assert cli._launch(args, cli.Vast(), cli.DEFAULTS) == 0
    assert selected == ([("12", editor)] if editor != "none" else [])
