import argparse
import json
import subprocess
import sys

import pytest

from gpu.cli import (
    CONTAINER_ENV,
    DEFAULTS,
    Vast,
    _launch,
    choose,
    startup_script,
)
from gpu.offers import gpu_category

EXPECTED_CONTAINER_SCRIPT = """#!/bin/bash
source /etc/environment 2>/dev/null || true
export GH_TOKEN
apt update && apt install -y gh
touch /root/.no_auto_tmux
rm -rf /venv
mkdir -p /workspace
git config --global user.name minoya1220
git config --global user.email minoya1220@gmail.com
if [ -n "${GH_TOKEN:-}" ]; then gh auth setup-git; else echo "WARNING: GH_TOKEN is not set in Vast account environment; private GitHub repos may not clone." >&2; fi
[ -d /workspace/gpu-kernels ] || git clone https://github.com/minoya1220/gpu-kernels /workspace/gpu-kernels
if [ -f /workspace/gpu-kernels/pyproject.toml ]; then uv sync --directory /workspace/gpu-kernels; fi
sed -i 's|^cd \\${WORKSPACE}$|[ "$PWD" = "$HOME" ] \\&\\& cd "${WORKSPACE:-/workspace}"|' /root/.bashrc
touch /root/.works
entrypoint.sh
"""


def args(**overrides):
    values = {
        "mode": "container",
        "repo": "gpu-kernels",
        "disk": None,
        "interruptible": False,
        "unverified": False,
    }
    values.update(overrides)
    return argparse.Namespace(**values)


def test_container_script_uses_selected_repo_directory_only():
    script = startup_script("gpu-kernels", "container", DEFAULTS)
    assert script == EXPECTED_CONTAINER_SCRIPT
    assert (
        "git clone https://github.com/minoya1220/gpu-kernels /workspace/gpu-kernels"
        in script
    )
    assert "uv sync --directory /workspace/gpu-kernels" in script
    assert "/workspace/cute-kernels" not in script
    assert "gh auth setup-git" in script
    assert script.index("gh auth setup-git") < script.index("git clone")
    assert ".no_auto_tmux" in script
    assert script.splitlines()[-1] == "entrypoint.sh"
    assert "set -e" not in script
    assert "72299" not in script and "741641" not in script
    assert "72299" not in CONTAINER_ENV and "741641" not in CONTAINER_ENV


@pytest.mark.parametrize("mode", ["container", "vm"])
def test_generated_startup_script_is_valid_bash(mode):
    result = subprocess.run(
        ["bash", "-n"],
        input=startup_script("gpu-kernels", mode, DEFAULTS),
        text=True,
        capture_output=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr


def test_vm_script_preserves_toolchain_and_does_not_inject_secret():
    script = startup_script("gpu-kernels", "vm", DEFAULTS)
    assert "nsight-compute-2025.4.0" in script and "cuda-nvcc-12-8" in script
    assert "source /etc/environment" in script
    assert "GH_TOKEN" in script and "WARNING" in script
    assert "gh auth setup-git" in script
    assert "uv sync --directory /workspace/gpu-kernels" in script
    assert "vast_api_key" not in script
    assert "/root/.no_auto_tmux" not in script
    assert "set -e" not in script
    assert script.splitlines()[-1] == "touch /root/.works"


def test_launch_request_exact_image_env_script_and_offer():
    client = Vast()
    offer = {"id": 45669333, "dph_total": 0.46898148148148144}
    actual = client.request(offer, args(), DEFAULTS)
    assert actual == [
        "vastai",
        "create",
        "instance",
        "45669333",
        "--image",
        "vastai/base-image:@vastai-automatic-tag",
        "--disk",
        "20",
        "--onstart-cmd",
        EXPECTED_CONTAINER_SCRIPT,
        "--ssh",
        "--direct",
        "--label",
        "gpu-kernels",
        "--env",
        '-p 1111:1111 -p 6006:6006 -p 8080:8080 -p 8384:8384 -e OPEN_BUTTON_PORT=1111 -e OPEN_BUTTON_TOKEN=1 -e JUPYTER_DIR=/ -e DATA_DIRECTORY=/workspace/ -e PORTAL_CONFIG="localhost:1111:11111:/:Instance Portal|localhost:8080:18080:/:Jupyter|localhost:8080:8080:/terminals/1:Jupyter Terminal|localhost:8384:18384:/:Syncthing|localhost:6006:16006:/:Tensorboard"',
        "--raw",
    ]
    vm_offer = {"id": 123, "min_bid": 0.2}
    vm_args = args(mode="vm", interruptible=True, disk=130)
    request = client.request(vm_offer, vm_args, DEFAULTS)
    assert request[:8] == [
        "vastai",
        "create",
        "instance",
        "123",
        "--image",
        "docker.io/vastai/kvm",
        "--disk",
        "130",
    ]
    assert "--env" not in request
    assert request[-3:] == ["--bid_price", "0.2", "--raw"]
    assert "source /etc/environment" in request[request.index("--onstart-cmd") + 1]


def test_adapter_search_generates_exact_read_only_cli_call():
    calls = []

    def fake(argv):
        calls.append(argv)
        return json.dumps([])

    api = Vast(command=fake)
    assert api.offers("rentable=true", bid=True) == []
    assert calls == [
        [
            "vastai",
            "search",
            "offers",
            "rentable=true",
            "--no-default",
            "--limit",
            "1000",
            "--order",
            "min_bid",
            "--interruptible",
            "--raw",
        ]
    ]


def test_up_validates_before_network_calls(monkeypatch):
    launch_args = args()
    launch_args.num_gpus = 0
    launch_args.max_price = None
    launch_args.offer = None
    launch_args.reliability = None
    monkeypatch.setattr(
        "gpu.cli.github_repos",
        lambda owner: pytest.fail("validation should happen before GitHub access"),
    )
    with pytest.raises(ValueError, match="--num-gpus"):
        _launch(launch_args, Vast(), DEFAULTS)


def test_dry_run_launch_path_uses_mocked_api_and_exact_request(monkeypatch, capsys):
    offer = {
        "id": 42,
        "gpu_name": "RTX 4090",
        "num_gpus": 1,
        "dph_total": 0.4,
        "reliability": 0.99,
        "geolocation": "Test, XX",
        "verification": "verified",
    }
    expected_args = args()
    expected_args.gpu = "RTX 4090"
    expected_args.num_gpus = 1
    expected_args.max_price = 0.5
    expected_args.offer = 42
    expected_args.yes = False
    expected_args.dry_run = True
    expected_args.reliability = None

    class MockVast:
        def __init__(self):
            self.searches = []
            self.requests = []

        def offers(self, query, *, bid=False):
            self.searches.append((query, bid))
            return [offer]

        def request(self, selected, launch_args, cfg):
            request = Vast().request(selected, launch_args, cfg)
            self.requests.append(request)
            return request

    mock = MockVast()
    monkeypatch.setattr("gpu.cli.github_repos", lambda owner: ["gpu-kernels"])
    assert _launch(expected_args, mock, DEFAULTS) == 0
    assert mock.searches == [
        (
            "external=false rentable=true verified=true num_gpus=1 reliability>=0.95 disk_space>=20 gpu_name=RTX_4090 dph<=0.5",
            False,
        )
    ]
    assert mock.requests == [
        [
            "vastai",
            "create",
            "instance",
            "42",
            "--image",
            "vastai/base-image:@vastai-automatic-tag",
            "--disk",
            "20",
            "--onstart-cmd",
            EXPECTED_CONTAINER_SCRIPT,
            "--ssh",
            "--direct",
            "--label",
            "gpu-kernels",
            "--env",
            CONTAINER_ENV,
            "--raw",
        ]
    ]
    output = capsys.readouterr().out
    assert "DRY RUN: no instance created" in output
    assert "\N{EM DASH}" not in output
    assert '"create"' in output


def test_yes_without_offer_auto_selects_cheapest_without_prompt(monkeypatch):
    offers = [
        {
            "id": 20,
            "gpu_name": "Tesla T4",
            "num_gpus": 1,
            "dph_total": 0.20,
            "reliability": 0.99,
            "geolocation": "A",
            "verification": "verified",
        },
        {
            "id": 10,
            "gpu_name": "Tesla T4",
            "num_gpus": 1,
            "dph_total": 0.10,
            "reliability": 0.98,
            "geolocation": "B",
            "verification": "verified",
        },
    ]

    class MockVast:
        def __init__(self):
            self.selected = None

        def offers(self, query, *, bid=False):
            return offers

        def request(self, selected, launch_args, cfg):
            self.selected = selected
            return Vast().request(selected, launch_args, cfg)

    launch_args = args()
    launch_args.gpu = "Tesla T4"
    launch_args.num_gpus = 1
    launch_args.max_price = None
    launch_args.offer = None
    launch_args.yes = True
    launch_args.dry_run = True
    launch_args.reliability = None
    mock = MockVast()
    monkeypatch.setattr("gpu.cli.github_repos", lambda owner: ["gpu-kernels"])
    monkeypatch.setattr(
        "builtins.input", lambda prompt: pytest.fail(f"unexpected prompt: {prompt}")
    )

    assert _launch(launch_args, mock, DEFAULTS) == 0
    assert mock.selected["id"] == 10


def test_interactive_offer_selection_does_not_print_second_list(monkeypatch, capsys):
    offer = {
        "id": 42,
        "gpu_name": "Tesla T4",
        "num_gpus": 1,
        "dph_total": 0.1,
        "reliability": 0.99,
        "geolocation": "Test",
        "verification": "verified",
    }

    class MockVast:
        def offers(self, query, *, bid=False):
            return [offer]

        def request(self, selected, launch_args, cfg):
            return Vast().request(selected, launch_args, cfg)

    launch_args = args()
    launch_args.gpu = "Tesla T4"
    launch_args.num_gpus = 1
    launch_args.max_price = None
    launch_args.offer = None
    launch_args.yes = False
    launch_args.dry_run = True
    launch_args.reliability = None
    monkeypatch.setattr("gpu.cli.github_repos", lambda owner: ["gpu-kernels"])
    monkeypatch.setattr("builtins.input", lambda prompt: "1")

    assert _launch(launch_args, MockVast(), DEFAULTS) == 0
    output = capsys.readouterr().out
    assert output.count("1. id=42") == 1
    assert "\n1. 42\n" not in output


def test_ls_table_and_json_output(monkeypatch, capsys):
    from gpu import cli

    payload = [
        {
            "id": "123",
            "gpu_name": "RTX 4090",
            "actual_status": "running",
            "dph_total": 0.4,
            "duration": 7342.81,
        }
    ]
    monkeypatch.setattr(cli, "run", lambda command, **kwargs: json.dumps(payload))
    assert cli.main(["ls"]) == 0
    output = capsys.readouterr().out
    assert "ID" in output and "RTX 4090" in output
    assert "Uptime" in output and "2h 02m" in output
    assert "7342" not in output
    assert cli.main(["ls", "--json"]) == 0
    assert json.loads(capsys.readouterr().out) == payload
    monkeypatch.setattr(cli, "run", lambda command, **kwargs: "[]")
    assert cli.main(["ls"]) == 0
    assert capsys.readouterr().out.strip() == "No instances."


def test_gpu_cache_honors_xdg_and_rejects_invalid_names(monkeypatch, tmp_path):
    from gpu import offers

    cache_dir = tmp_path / "cache"
    monkeypatch.setenv("XDG_CACHE_HOME", str(cache_dir))
    cache_path = offers.gpu_cache_path()
    assert cache_path == cache_dir / "gpu" / "gpu_models.txt"
    cache_path.parent.mkdir(parents=True)
    cache_path.write_text(
        "Tesla T4\nRTX 4090\n\\n\n" + "x" * 101 + "\n" + "bad\tname\n"
    )
    assert offers.read_gpu_cache() == ["RTX 4090", "Tesla T4"]
    offers.update_gpu_cache(
        [
            {"gpu_name": "B200"},
            {"gpu_name": "line\nbreak"},
            {"gpu_name": "x" * 101},
            {"gpu_name": "bad\tname"},
        ]
    )
    assert offers.read_gpu_cache() == ["B200", "RTX 4090", "Tesla T4"]
    assert list(cache_path.parent.iterdir()) == [cache_path]
    assert {"B200", "RTX 4090", "Tesla T4"} <= set(offers.known_gpu_models())


def test_live_vast_gpu_fixture_has_few_other_models():
    from pathlib import Path

    names = (
        Path(__file__)
        .parent.joinpath("fixtures/vast_gpu_models.txt")
        .read_text()
        .splitlines()
    )
    assert len(names) >= 70
    assert sum(gpu_category(name) == "Other" for name in names) <= 2
    assert gpu_category("RTX 6000Ada") == "Workstation/Pro"
    assert gpu_category("Q RTX 8000") == "Workstation/Pro"
    assert gpu_category("A800 PCIE") == "Datacenter Ampere/Ada/older"
    assert gpu_category("Tesla P40") == "Datacenter Ampere/Ada/older"


def test_gpu_default_hint_only_appears_when_recent_pick_exists(monkeypatch, capsys):
    from gpu import cli

    monkeypatch.setattr(cli, "load_history", dict)
    prompts = []
    answers = iter(["1", "1"])
    monkeypatch.setattr(
        "builtins.input", lambda prompt: prompts.append(prompt) or next(answers)
    )
    assert cli.choose_gpu(["H100 SXM"], {"H100 SXM": (1.0, 1)}) == "H100 SXM"
    assert prompts[0] == "GPU: "
    assert "(1 offer)" in capsys.readouterr().out
    monkeypatch.setattr(cli, "load_history", lambda: {"gpu": ["H100 SXM"]})
    prompts.clear()
    monkeypatch.setattr("builtins.input", lambda prompt: prompts.append(prompt) or "")
    assert cli.choose_gpu(["H100 SXM"], {"H100 SXM": (1.0, 1)}) == "H100 SXM"
    assert prompts[0] == "GPU [Enter = most recent]: "


def test_picker_selects_unique_text_match_without_second_enter(monkeypatch, capsys):
    answers = iter(["gpu-k"])
    monkeypatch.setattr("builtins.input", lambda prompt: next(answers))
    assert choose(["gpu-kernels", "other"], "Repository") == "gpu-kernels"


def test_interactive_gpu_stats_and_offers_share_unverified_filter(monkeypatch, capsys):
    matches = [
        {
            "id": 1,
            "gpu_name": "Tesla T4",
            "num_gpus": 1,
            "dph_total": 0.149,
            "reliability": 0.99,
            "geolocation": "A",
            "verification": "verified",
        },
        {
            "id": 2,
            "gpu_name": "Tesla T4",
            "num_gpus": 1,
            "dph_total": 0.095,
            "reliability": 0.99,
            "geolocation": "B",
            "verification": "unverified",
        },
        {
            "id": 3,
            "gpu_name": "Tesla T4",
            "num_gpus": 1,
            "dph_total": 0.05,
            "reliability": 0.99,
            "geolocation": "C",
            "verification": "deverified",
        },
    ]

    class MockVast:
        query = None

        def offers(self, query, *, bid=False):
            self.query = query
            return matches

        def request(self, selected, launch_args, cfg):
            self.selected = selected
            return ["mock"]

    mock = MockVast()
    launch_args = args(
        gpu=None,
        num_gpus=1,
        max_price=None,
        offer=None,
        yes=False,
        dry_run=True,
        reliability=None,
    )
    monkeypatch.setattr("gpu.cli.github_repos", lambda owner: ["gpu-kernels"])
    answers = iter(["1", "1", "1"])
    monkeypatch.setattr("builtins.input", lambda prompt: next(answers))
    assert _launch(launch_args, mock, DEFAULTS) == 0
    output = capsys.readouterr().out
    assert "verified=true" not in mock.query
    assert "$0.095/hr (2 offers)" in output
    assert "verification=unverified" in output
    assert "deverified" not in output
    assert mock.selected["id"] == 2


def test_picker_filters_and_paginates(monkeypatch, capsys):
    answers = iter(["n", "GPU 1", "1"])
    monkeypatch.setattr("builtins.input", lambda prompt: next(answers))
    selected = choose(
        [f"GPU {i}" for i in range(25)], "GPU", labels=[f"GPU {i}" for i in range(25)]
    )
    assert selected == "GPU 1"
    output = capsys.readouterr().out
    assert "page 1/3" in output and "page 2/3" in output


def test_picker_recents_use_frequency_weighted_recency_and_latest_default(
    monkeypatch, tmp_path, capsys
):
    from gpu import cli

    monkeypatch.setattr(cli, "history_path", lambda: tmp_path / "history.json")
    cli.history_path().write_text(json.dumps({"repo": ["b", "a", "a", "a"]}))
    monkeypatch.setattr("builtins.input", lambda prompt: "")
    assert choose(["a", "b"], "Repository", step="repo") == "b"
    output = capsys.readouterr().out
    assert output.index("1. a") < output.index("2. b")


def test_history_is_written_only_after_successful_confirmed_launch(
    monkeypatch, tmp_path
):
    from gpu import cli

    monkeypatch.setattr(cli, "history_path", lambda: tmp_path / "history.json")
    launch_args = args(
        gpu="Tesla T4",
        num_gpus=1,
        max_price=None,
        offer=42,
        yes=False,
        dry_run=False,
        reliability=None,
    )
    offer = {
        "id": 42,
        "gpu_name": "Tesla T4",
        "num_gpus": 1,
        "dph_total": 0.1,
        "reliability": 0.99,
        "geolocation": "Test",
        "verification": "verified",
    }

    class MockVast:
        def offers(self, query, *, bid=False):
            return [offer]

        def request(self, selected, launch_args, cfg):
            return ["vastai", "create", "instance", "42"]

    monkeypatch.setattr(cli, "github_repos", lambda owner: ["gpu-kernels"])
    monkeypatch.setattr("builtins.input", lambda prompt: "yes")
    monkeypatch.setattr(cli, "run", lambda command, **kwargs: "created")
    assert _launch(launch_args, MockVast(), DEFAULTS) == 0
    assert json.loads(cli.history_path().read_text()) == {
        "mode": ["container"],
        "gpu": ["Tesla T4"],
    }


def test_unverified_flag_removes_verified_filter_and_lists_all_hosts(
    monkeypatch, capsys
):
    offers = [
        {
            "id": host_id,
            "gpu_name": "Tesla T4",
            "num_gpus": 1,
            "dph_total": price,
            "reliability": 0.99,
            "geolocation": "Test",
            "verification": verification,
        }
        for host_id, price, verification in [
            (1, 0.1, "deverified"),
            (2, 0.2, "verified"),
            (3, 0.3, "unverified"),
        ]
    ]

    class MockVast:
        def __init__(self):
            self.query = None

        def offers(self, query, *, bid=False):
            self.query = query
            return offers

        def request(self, selected, launch_args, cfg):
            return Vast().request(selected, launch_args, cfg)

    launch_args = args(unverified=True)
    launch_args.gpu = "Tesla T4"
    launch_args.num_gpus = 1
    launch_args.max_price = None
    launch_args.offer = None
    launch_args.yes = True
    launch_args.dry_run = True
    launch_args.reliability = None
    mock = MockVast()
    monkeypatch.setattr("gpu.cli.github_repos", lambda owner: ["gpu-kernels"])

    assert _launch(launch_args, mock, DEFAULTS) == 0
    assert "verified=" not in mock.query
    output = capsys.readouterr().out
    assert output.count("Tesla T4") == 3
    assert "verification=deverified" in output
    assert "verification=verified" in output
    assert "verification=unverified" in output


def test_ctrl_c_at_a_line_prompt_cancels_without_traceback(monkeypatch, capsys):
    from gpu import cli

    def interrupt(prompt):
        raise KeyboardInterrupt

    monkeypatch.setattr(cli, "github_repos", lambda owner: ["gpu-kernels"])
    monkeypatch.setattr("builtins.input", interrupt)
    assert cli.main(["up", "--dry-run"]) == 130
    output = capsys.readouterr()
    assert output.out.endswith("\nCancelled.\n")
    assert "Traceback" not in output.err


FAKE_INSTANCE_KEY = "fake-instance-key-8c1f0e"


def _confirmed_launch(monkeypatch, tmp_path, response):
    from gpu import cli

    monkeypatch.setattr(cli, "history_path", lambda: tmp_path / "history.json")
    monkeypatch.setattr(cli, "interactive_terminal", lambda: False)
    monkeypatch.setattr(cli, "github_repos", lambda owner: ["gpu-kernels"])
    offer = {"id": 42, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.1}

    class MockVast:
        def offers(self, query, *, bid=False):
            return [offer]

        def request(self, selected, launch_args, cfg):
            return ["vastai", "create", "instance", "42", "--raw"]

    def fake_subprocess_run(argv, **kwargs):
        if isinstance(response, Exception):
            raise response
        return subprocess.CompletedProcess(argv, 0, stdout=response, stderr="")

    monkeypatch.setattr(cli.subprocess, "run", fake_subprocess_run)
    launch_args = args(
        gpu="Tesla T4", num_gpus=1, max_price=None, offer=42, yes=True, dry_run=False, reliability=None
    )
    try:
        return _launch(launch_args, MockVast(), DEFAULTS)
    except RuntimeError as exc:
        print(f"gpu: {exc}", file=sys.stderr)
        return 1


@pytest.mark.parametrize(
    "response",
    [
        json.dumps(
            {"success": True, "new_contract": 31337, "instance_api_key": FAKE_INSTANCE_KEY}
        ),
        f"Started. {{'success': True, 'new_contract': 31337, 'instance_api_key': '{FAKE_INSTANCE_KEY}'}}",
    ],
    ids=["raw-json", "unparseable"],
)
def test_up_never_prints_instance_api_key(monkeypatch, tmp_path, capsys, response):
    assert _confirmed_launch(monkeypatch, tmp_path, response) == 0
    captured = capsys.readouterr()
    assert FAKE_INSTANCE_KEY not in captured.out + captured.err
    if response.startswith("{"):
        assert "Created instance 31337" in captured.out
        assert "gpu ssh 31337" in captured.out
    else:
        assert "gpu ls" in captured.out


@pytest.mark.parametrize(
    "response",
    [
        json.dumps({"success": False, "msg": f"bad api_key={FAKE_INSTANCE_KEY}"}),
        subprocess.CalledProcessError(
            1, "vastai", stderr=f'{{"error": "boom", "instance_api_key": "{FAKE_INSTANCE_KEY}"}}'
        ),
    ],
    ids=["unsuccessful", "command-failed"],
)
def test_up_errors_never_print_instance_api_key(monkeypatch, tmp_path, capsys, response):
    assert _confirmed_launch(monkeypatch, tmp_path, response) == 1
    captured = capsys.readouterr()
    assert FAKE_INSTANCE_KEY not in captured.out + captured.err
    assert not (tmp_path / "history.json").exists()


def test_run_redacts_secrets_from_failed_command_output(monkeypatch):
    from gpu import cli

    def failing(*a, **kw):
        raise subprocess.CalledProcessError(
            1, "vastai", stderr=f"'instance_api_key': '{FAKE_INSTANCE_KEY}', id 7"
        )

    monkeypatch.setattr(cli.subprocess, "run", failing)
    with pytest.raises(RuntimeError) as exc:
        cli.run(["vastai", "create", "instance", "7"])
    assert FAKE_INSTANCE_KEY not in str(exc.value)
    assert "id 7" in str(exc.value)


def test_ls_ssh_and_stop_redact_secrets(monkeypatch, capsys):
    from gpu import cli

    instances = [
        {
            "id": 31337,
            "actual_status": "stopped",
            "gpu_name": "RTX 4090",
            "instance_api_key": FAKE_INSTANCE_KEY,
            "jupyter_token": FAKE_INSTANCE_KEY,
            "extra_env": [["VAST_API_KEY", FAKE_INSTANCE_KEY]],
        }
    ]
    outputs = {
        "show": json.dumps(instances),
        "ssh-url": f"ssh://root@31337.example:22?token={FAKE_INSTANCE_KEY}",
        "stop": f"stopping instance 31337. api_key: {FAKE_INSTANCE_KEY}",
    }
    monkeypatch.setattr(cli, "run", lambda command, **kwargs: outputs[command[1]])
    for argv in (["ls"], ["ls", "--json"], ["ssh", "31337"], ["stop", "31337"]):
        assert cli.main(argv) == 0
        captured = capsys.readouterr()
        assert FAKE_INSTANCE_KEY not in captured.out + captured.err
        assert "31337" in captured.out


def test_dry_run_request_contains_no_credentials():
    request = Vast().request({"id": 42, "dph_total": 0.1}, args(), DEFAULTS)
    assert not any(cli_secret in part for part in request for cli_secret in ("api_key", "ghp_", "gho_"))
