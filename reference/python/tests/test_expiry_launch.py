"""All launch/SSH operations in these tests are mocks; never rent an instance."""
import json
from datetime import datetime, timezone
from unittest.mock import Mock

import pytest

from gpu import cli


@pytest.fixture
def launch(monkeypatch, tmp_path):
    monkeypatch.setattr(cli, "history_path", lambda: tmp_path / "history.json")
    monkeypatch.setattr(cli, "load_config", lambda: cli.DEFAULTS)
    monkeypatch.setattr(cli, "interactive_terminal", lambda: False)
    offer = {"id": 42, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.1}
    monkeypatch.setattr(cli.Vast, "offers", lambda *a, **kw: [offer])
    run = Mock(return_value=json.dumps({"success": True, "new_contract": 123, "instance_api_key": "private-sentinel"}))
    monkeypatch.setattr(cli, "run", run)
    expire = Mock()
    monkeypatch.setattr(cli, "expire_instance", expire)
    argv = ["up", "--no-repo", "--mode", "container", "--gpu", "Tesla T4", "--offer", "42", "--yes", "--expire", "2h"]
    return argv, run, expire


def test_up_arms_after_creation(launch, capsys):
    argv, run, expire = launch
    now = datetime.now(timezone.utc)
    assert cli.main(argv) == 0
    assert run.call_args.args[0][:3] == ["vastai", "create", "instance"]
    assert expire.call_args.args[:2] == ("123", "arm")
    assert 7199 <= (expire.call_args.args[2] - now).total_seconds() <= 7201
    assert "private-sentinel" not in capsys.readouterr().out


def test_up_dry_run_does_not_arm(launch, capsys):
    argv, run, expire = launch
    assert cli.main([*argv, "--dry-run"]) == 0
    run.assert_not_called()
    expire.assert_not_called()
    assert "no timer installed" in capsys.readouterr().out


def test_up_arming_failure_leaves_instance_and_warns(launch, capsys):
    argv, run, expire = launch
    expire.side_effect = RuntimeError("verification failed")
    assert cli.main(argv) == 1
    assert run.call_count == 1  # no attempted rollback / lifecycle call
    error = capsys.readouterr().err
    assert "123 created but expiry NOT verified" in error
    assert "billing continues" in error
    assert "gpu expire 123 show" in error


def test_up_without_returned_id_cannot_claim_expiry(launch, capsys):
    argv, run, expire = launch
    run.return_value = '{"success": true}'
    assert cli.main(argv) == 1
    expire.assert_not_called()
    assert "expiry NOT armed" in capsys.readouterr().err
