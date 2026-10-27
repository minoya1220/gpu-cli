import json
import os
import shlex
import subprocess
import time
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timedelta, timezone
from pathlib import Path
from threading import Event
from types import SimpleNamespace
from unittest.mock import Mock
from zoneinfo import ZoneInfo

import pytest

from gpu import cli, expiry
from gpu import expiry_remote as remote

UTC = timezone.utc
NOW = datetime(2026, 1, 1, 12, tzinfo=UTC)


@pytest.fixture(autouse=True)
def no_live_calls(monkeypatch):
    def forbidden(*args, **kwargs):
        pytest.fail("Unmocked external call in expiry test")
    monkeypatch.setattr(cli, "run", forbidden)
    monkeypatch.setattr(cli, "list_instances", forbidden)
    monkeypatch.setattr(remote.urllib.request.OpenerDirector, "open", forbidden)


@pytest.mark.parametrize("text,seconds", [("30m", 1800), ("2h", 7200), ("1h30m", 5400),
                                          ("1d2h3m4s", 93784), ("45m", 2700), ("1s", 1)])
def test_duration(text, seconds):
    assert expiry.parse_duration(text) == seconds
    assert expiry.parse_when(text, now=NOW) == NOW + timedelta(seconds=seconds)


@pytest.mark.parametrize("text", ["", "0s", "-2h", "1.5h", "2", "1m2h", "1h1h", "2h;id", "infinity"])
def test_invalid_duration(text):
    with pytest.raises(ValueError):
        expiry.parse_duration(text)


@pytest.mark.parametrize("text,expected", [
    ("18:30", "2026-01-01T18:30:00+00:00"),
    ("6:30PM", "2026-01-01T18:30:00+00:00"),
    ("6:30pm", "2026-01-01T18:30:00+00:00"),
    ("6pm", "2026-01-01T18:00:00+00:00"),
    ("12am", "2026-01-02T00:00:00+00:00"),
    ("12pm", "2026-01-02T12:00:00+00:00"),
    ("11:00", "2026-01-02T11:00:00+00:00"),
    ("2026-01-02T09:00:00-05:00", "2026-01-02T14:00:00+00:00"),
    ("2026-01-02T09:00Z", "2026-01-02T09:00:00+00:00"),
    ("2026-01-02 09:00", "2026-01-02T09:00:00+00:00"),
])
def test_times(text, expected):
    assert expiry.parse_when(text, now=NOW, zone=UTC).isoformat() == expected


@pytest.mark.parametrize("text", ["24:00", "13pm", "0am", "6:70pm", "18", "2026-01-01", "2025-01-01T18:30Z",
                                  "2026-01-01T12:00Z", "6:3pm", "6:30pm; rm -rf /", "2h 30m"])
def test_invalid_times(text):
    with pytest.raises(ValueError):
        expiry.parse_when(text, now=NOW, zone=UTC)


def test_dst_elapsed_duration_is_not_wall_time():
    now = datetime(2026, 3, 8, 6, 30, tzinfo=UTC)  # 01:30 EST before spring-forward
    deadline = expiry.parse_when("2h", now=now, zone=ZoneInfo("America/New_York"))
    assert deadline == now + timedelta(seconds=7200)
    assert deadline.astimezone(ZoneInfo("America/New_York")).hour == 4


def test_dst_gap_and_fold():
    zone = ZoneInfo("America/New_York")
    with pytest.raises(ValueError, match="nonexistent or ambiguous"):
        expiry.parse_when("2026-03-08T02:30", now=NOW, zone=zone)
    with pytest.raises(ValueError, match="nonexistent or ambiguous"):
        expiry.parse_when("2026-11-01T01:30", now=NOW, zone=zone)
    assert expiry.parse_when("2026-11-01T01:30-05:00", now=NOW, zone=zone).hour == 6
    # A clock time skips a nonexistent occurrence; fall-back picks the next fold.
    assert expiry.parse_when("2:30am", now=datetime(2026, 3, 8, 6, tzinfo=UTC), zone=zone) == datetime(2026, 3, 9, 6, 30, tzinfo=UTC)
    assert expiry.parse_when("1:30am", now=datetime(2026, 11, 1, 5, 45, tzinfo=UTC), zone=zone) == datetime(2026, 11, 1, 6, 30, tzinfo=UTC)


def test_system_local_timezone_uses_future_rules(monkeypatch):
    original = os.environ.get("TZ")
    try:
        monkeypatch.setenv("TZ", "America/New_York")
        time.tzset()
        assert expiry.parse_when("2026-07-01T12:00", now=NOW) == datetime(2026, 7, 1, 16, tzinfo=UTC)
        with pytest.raises(ValueError, match="DST"):
            expiry.parse_when("2026-03-08T02:30", now=NOW)
        assert expiry.parse_when("1:30am", now=datetime(2026, 11, 1, 5, 45, tzinfo=UTC)) == datetime(2026, 11, 1, 6, 30, tzinfo=UTC)
    finally:
        if original is None:
            monkeypatch.delenv("TZ", raising=False)
        else:
            monkeypatch.setenv("TZ", original)
        time.tzset()


@pytest.mark.parametrize("values,expected", [(["2h"], (None, "2h")), (["123", "6pm"], ("123", "6pm")),
                                             (["cancel"], (None, "cancel")), (["123", "show"], ("123", "show"))])
def test_argument_shape(values, expected):
    assert expiry.parse_expire_arguments(values) == expected


@pytest.mark.parametrize("values", [[], ["123"], ["123", "456"], ["2h", "123"], ["123", "2h", "3h"]])
def test_ambiguous_arguments(values):
    with pytest.raises(ValueError, match="Bare numbers are IDs"):
        expiry.parse_expire_arguments(values)


def test_command_has_no_user_shell_input():
    command = expiry.remote_command("vast-123", "123", "arm", NOW)
    assert command[:-1] == ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "vast-123"]
    assert shlex.split(command[-1]) == ["python3", "-", "arm", "123", str(int(NOW.timestamp()))]
    for alias, instance in [("vast-1;id", "1;id"), ("-oProxyCommand=bad", "123"), ("vast-456", "123")]:
        with pytest.raises(ValueError):
            expiry.remote_command(alias, instance, "show", None)


def test_transport_embeds_standalone_source_without_secrets(monkeypatch):
    def run(argv, **kwargs):
        assert argv[-1].startswith("python3 - arm 123 ")
        source = kwargs["input"]
        namespace = {"__name__": "not_main"}
        exec(compile(source, "<transport>", "exec"), namespace)  # noqa: S102 - our own standalone source
        assert namespace["SOURCE"] == Path(remote.__file__).read_text()
        assert "personal-key-sentinel" not in source
        assert kwargs["timeout"] == 45
        return SimpleNamespace(returncode=0, stdout=json.dumps({"status": "armed", "deadline": NOW.timestamp()}))
    monkeypatch.setenv("VAST_API_KEY", "personal-key-sentinel")
    monkeypatch.setattr(expiry.subprocess, "run", run)
    assert expiry.manage_expiry("vast-123", "123", "arm", NOW)["status"] == "armed"


@pytest.mark.parametrize("reply,match", [({"error": "verification"}, "verification failed"),
                                        ({"error": "credentials"}, "scoped"),
                                        ({"status": "inactive"}, "verification failed"),
                                        ({"error": "personal-key-sentinel"}, "Remote expiry")])
def test_transport_errors_do_not_leak(monkeypatch, reply, match):
    monkeypatch.setattr(expiry.subprocess, "run", lambda *a, **kw: SimpleNamespace(
        returncode=0, stdout=json.dumps(reply), stderr="personal-key-sentinel"))
    with pytest.raises(RuntimeError, match=match) as error:
        expiry.manage_expiry("vast-123", "123", "arm", NOW)
    assert "personal-key-sentinel" not in str(error.value)


def test_timeout_marks_arming_unverified(monkeypatch):
    def run(*a, **kw):
        raise subprocess.TimeoutExpired("ssh", 45, stderr="private-key")
    monkeypatch.setattr(expiry.subprocess, "run", run)
    with pytest.raises(RuntimeError, match="unverified"):
        expiry.manage_expiry("vast-123", "123", "arm", NOW)


def test_confirmation_local_timezone_and_remaining():
    message = expiry.expiry_summary("123", {"status": "armed", "deadline": time.time() + 7200})
    assert "auto-DESTROY" in message and "Disk will be deleted" in message
    assert "in 2h" in message
    assert datetime.now().astimezone().strftime("%Z") in message


@pytest.fixture
def timer(tmp_path, monkeypatch):
    monkeypatch.setattr(remote, "credentials", lambda instance: "scoped-key")
    monkeypatch.setattr(remote, "running", lambda state: True)
    process = Mock(pid=456)
    process.poll.return_value = None
    monkeypatch.setattr(remote, "start_worker", lambda *a: process)
    # control deliberately sets the remote process's umask; restore the test runner's.
    old_umask = os.umask(0o077)
    yield tmp_path
    os.umask(old_umask)


def arm(root, deadline=None):
    return remote.control(root, "arm", "123", deadline or int(time.time() + 7200), "# fake runner")


def test_arm_show_replace_cancel(timer, monkeypatch):
    assert remote.control(timer, "show", "123") == {"status": "none"}
    deadline = int(time.time() + 7200)
    assert arm(timer, deadline) == {"status": "armed", "deadline": deadline}
    old = remote.load_state(timer)
    assert "scoped-key" not in (timer / "deadline.json").read_text()
    assert (timer / "deadline.json").stat().st_mode & 0o777 == 0o600
    assert remote.control(timer, "show", "123") == {"status": "armed", "deadline": deadline}
    assert arm(timer, deadline + 3600)["deadline"] == deadline + 3600
    assert remote.load_state(timer)["token"] != old["token"]
    destroy = Mock(side_effect=AssertionError("Must not destroy superseded timers"))
    monkeypatch.setattr(remote, "destroy", destroy)
    assert remote.tick(timer, old["token"], "123", "scoped-key", now=deadline + 9999) is None
    new = remote.load_state(timer)
    assert remote.control(timer, "cancel", "123") == {"status": "cancelled"}
    assert remote.tick(timer, new["token"], "123", "scoped-key", now=deadline + 9999) is None
    assert remote.control(timer, "cancel", "123") == {"status": "none"}
    destroy.assert_not_called()


def test_tick_only_destroys_at_deadline_and_retries(timer, monkeypatch):
    arm(timer)
    state = remote.load_state(timer)
    destroy = Mock(side_effect=[OSError("secret must not be saved"), False, True])
    monkeypatch.setattr(remote, "destroy", destroy)
    assert remote.tick(timer, state["token"], "123", "scoped-key", now=state["deadline"] - 1) == 1
    destroy.assert_not_called()
    for _ in range(2):
        assert remote.tick(timer, state["token"], "123", "scoped-key", now=state["deadline"]) == 30
        assert remote.load_state(timer)["status"] == "retrying"
    assert remote.tick(timer, state["token"], "123", "scoped-key", now=state["deadline"]) is None
    assert remote.load_state(timer)["status"] == "destroy-request-accepted"
    destroy.assert_called_with("123", "scoped-key")
    assert "secret" not in (timer / "deadline.json").read_text()


def test_failed_rearm_preserves_existing_deadline(timer, monkeypatch):
    arm(timer)
    original = remote.load_state(timer)
    def fail(*args):
        raise remote.ExpiryError("verification")
    monkeypatch.setattr(remote, "start_worker", fail)
    with pytest.raises(remote.ExpiryError, match="verification"):
        arm(timer)
    assert remote.load_state(timer) == original
    with pytest.raises(remote.ExpiryError, match="past"):
        arm(timer, time.time() - 10)
    assert remote.load_state(timer) == original


@pytest.mark.parametrize("existing", [False, True])
def test_post_arm_verification_failure_rolls_back(timer, monkeypatch, existing):
    if existing:
        arm(timer)
    original = remote.load_state(timer)
    monkeypatch.setattr(remote, "running", lambda state: False)
    with pytest.raises(remote.ExpiryError, match="verification"):
        arm(timer)
    assert remote.load_state(timer) == original


def test_cancel_serializes_with_inflight_destroy(timer, monkeypatch):
    arm(timer)
    state = remote.load_state(timer)
    started, release, cancelled = Event(), Event(), Event()
    def destroy(*args):
        started.set()
        assert release.wait(5)
        return False
    def cancel():
        result = remote.control(timer, "cancel", "123")
        cancelled.set()
        return result
    monkeypatch.setattr(remote, "destroy", destroy)
    with ThreadPoolExecutor(max_workers=2) as pool:
        tick = pool.submit(remote.tick, timer, state["token"], "123", "scoped", state["deadline"])
        assert started.wait(5)
        cancellation = pool.submit(cancel)
        try:
            assert not cancelled.wait(0.05)
        finally:
            release.set()
        assert tick.result(timeout=5) == 30
        assert cancellation.result(timeout=5)["status"] == "cancelled"
    assert remote.load_state(timer) is None


def test_show_detects_dead_worker(timer, monkeypatch):
    arm(timer)
    monkeypatch.setattr(remote, "running", lambda state: False)
    assert remote.control(timer, "show", "123")["status"] == "inactive"


def test_worker_is_detached_and_startup_verified(tmp_path, monkeypatch):
    process = Mock(pid=123)
    process.poll.return_value = None
    popen = Mock(return_value=process)
    monkeypatch.setattr(remote.subprocess, "Popen", popen)
    monkeypatch.setattr(remote.select, "select", lambda *a: ([1], [], []))
    monkeypatch.setattr(remote.os, "read", lambda *a: b"ready")
    monkeypatch.setattr(remote, "running", lambda state: True)
    assert remote.start_worker(tmp_path, "generation", "123", "scoped-key") is process
    args, kwargs = popen.call_args
    assert args[0][2:6] == ["worker", str(tmp_path), "generation", "123"]
    assert "scoped-key" not in str(args)
    assert kwargs["start_new_session"] is True
    assert kwargs["stdin"] == kwargs["stdout"] == kwargs["stderr"] == subprocess.DEVNULL
    assert kwargs["env"]["CONTAINER_API_KEY"] == "scoped-key"
    assert "VAST_API_KEY" not in kwargs["env"]
    monkeypatch.setattr(remote.select, "select", lambda *a: ([], [], []))
    with pytest.raises(remote.ExpiryError, match="verification"):
        remote.start_worker(tmp_path, "generation", "123", "scoped-key")
    process.terminate.assert_called_once()
    process.wait.assert_called_once()


def test_process_identity_check(monkeypatch):
    monkeypatch.setattr(Path, "read_bytes", lambda self: b"python3\0runner.py\0worker\0state\0token\0")
    assert remote.running({"pid": 123, "token": "token"})
    assert not remote.running({"pid": 123, "token": "another-token"})


@pytest.mark.parametrize("environment", ["container", "vm", "pid1"])
def test_scoped_credentials_sources(monkeypatch, environment):
    monkeypatch.delenv("CONTAINER_ID", raising=False)
    monkeypatch.delenv("CONTAINER_API_KEY", raising=False)
    monkeypatch.setenv("VAST_API_KEY", "personal-key-must-never-be-used")
    monkeypatch.setattr(Path, "read_bytes", lambda p: b"CONTAINER_ID=123\0CONTAINER_API_KEY=scoped\0" if environment == "pid1" else b"")
    monkeypatch.setattr(Path, "read_text", lambda p: 'CONTAINER_ID="123"\nCONTAINER_API_KEY="scoped"\n' if environment == "vm" else "")
    if environment == "container":
        monkeypatch.setenv("CONTAINER_ID", "123")
        monkeypatch.setenv("CONTAINER_API_KEY", "scoped")
    assert remote.credentials("123") == "scoped"
    with pytest.raises(remote.ExpiryError, match="credentials"):
        remote.credentials("456")


def test_missing_scoped_credentials_never_falls_back_to_account(monkeypatch):
    monkeypatch.delenv("CONTAINER_ID", raising=False)
    monkeypatch.delenv("CONTAINER_API_KEY", raising=False)
    monkeypatch.setenv("VAST_API_KEY", "personal-key")
    monkeypatch.setattr(Path, "read_bytes", lambda p: b"")
    monkeypatch.setattr(Path, "read_text", lambda p: "")
    with pytest.raises(remote.ExpiryError, match="credentials"):
        remote.credentials("123")


def test_exact_destroy_request_mocked(monkeypatch):
    response = Mock()
    response.read.return_value = b'{"success": true}'
    opener = Mock()
    opener.open.return_value.__enter__ = Mock(return_value=response)
    opener.open.return_value.__exit__ = Mock(return_value=False)
    build = Mock(return_value=opener)
    monkeypatch.setattr(remote.urllib.request, "build_opener", build)
    assert remote.destroy("123", "scoped-key") is True
    request = opener.open.call_args.args[0]
    assert request.full_url == "https://console.vast.ai/api/v0/instances/123/"
    assert request.method == "DELETE"
    assert request.get_header("Authorization") == "Bearer scoped-key"
    assert opener.open.call_args.kwargs["timeout"] == 15
    assert isinstance(build.call_args.args[1], remote.NoRedirect)


@pytest.mark.parametrize("args,instance,action", [(["123", "2h"], "123", "arm"),
                                                 (["6pm"], "456", "arm"),
                                                 (["123", "cancel"], "123", "cancel"),
                                                 (["show"], "456", "show")])
def test_cli_dispatch(args, instance, action, monkeypatch):
    monkeypatch.setattr(cli, "load_config", lambda: cli.DEFAULTS)
    monkeypatch.setattr(cli, "list_instances", lambda: [{"id": 456}])
    picker = Mock(return_value="456")
    monkeypatch.setattr(cli, "select_instance", picker)
    expire = Mock()
    monkeypatch.setattr(cli, "expire_instance", expire)
    assert cli.main(["expire", *args]) == 0
    assert expire.call_args.args[:2] == (instance, action)
    assert (expire.call_args.args[2] is not None) == (action == "arm")
    assert picker.called == (instance == "456")


@pytest.mark.parametrize("args", [[], ["123"], ["123", "123"], ["2h", "45m"], ["--in", "2h"], ["--at", "18:30"], ["--cancel"], ["6:80pm"]])
def test_cli_usage_errors_before_external_calls(args):
    with pytest.raises(SystemExit) as error:
        cli.main(["expire", *args])
    assert error.value.code == 2


def test_cli_show_inactive_is_nonzero(monkeypatch, capsys):
    monkeypatch.setattr(cli, "load_config", lambda: cli.DEFAULTS)
    monkeypatch.setattr(cli, "wait_for_ssh", lambda id: ("vast-123", "/workspace"))
    monkeypatch.setattr(cli, "manage_expiry", lambda *a: {"status": "inactive", "deadline": time.time() + 7200})
    assert cli.main(["expire", "123", "show"]) == 1
    assert "NOT running" in capsys.readouterr().err


def test_up_expire_invalid_fails_before_launch():
    with pytest.raises(SystemExit) as error:
        cli.main(["up", "--expire", "0m"])
    assert error.value.code == 2
