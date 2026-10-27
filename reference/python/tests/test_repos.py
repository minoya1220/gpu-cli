"""Optional/arbitrary repositories: no network, credentials, or paid instances."""

import json
import subprocess
import threading

import pytest

from gpu import cli
from gpu.picker import Job, PickerCancelled
from gpu.repos import (
    NO_REPO,
    cache_path,
    load_cached_repos,
    resolve_repo,
    save_cached_repos,
)
from gpu.wizard import LaunchWizard, Picks


@pytest.mark.parametrize(("repo", "url", "name"), [
    ("gpu-kernels", "https://github.com/minoya1220/gpu-kernels", "gpu-kernels"),
    ("sgl-project/sglang", "https://github.com/sgl-project/sglang", "sglang"),
    ("https://github.com/sgl-project/sglang.git", "https://github.com/sgl-project/sglang.git", "sglang"),
    ("https://git.example.org:8443/team/sub/repo.git/", "https://git.example.org:8443/team/sub/repo.git/", "repo"),
    ("git@git.example.org:team/sub/repo.git", "git@git.example.org:team/sub/repo.git", "repo"),
    ("owner/.dotfiles", "https://github.com/owner/.dotfiles", ".dotfiles"),
    (NO_REPO, "", "no-repo"),
])
def test_resolve_repo(repo, url, name):
    assert resolve_repo(repo, "minoya1220") == (url, name)


@pytest.mark.parametrize("repo", [
    "--upload-pack=bad", "../escape", "owner/..", "owner/repo;id", "$(id)",
    "file:///tmp/repo", "http://host/repo", "ssh://git@host/repo", "git@host:../../repo",
    "https://host/repo;id", "https://host/$(id)", "https://host/repo'", "https://host/repo\n",
    "https://user:secret@host/repo", "https://host/repo?token=secret", "https://host/repo#branch",
    "https://host/.git", "git@host:repo`id`", "https://host/../repo",
])
def test_unsafe_or_credential_bearing_repos_are_rejected(repo):
    with pytest.raises(ValueError):
        resolve_repo(repo, "minoya1220")


@pytest.mark.parametrize("mode", ["container", "vm"])
@pytest.mark.parametrize("repo", [NO_REPO, "sgl-project/sglang", "https://example.org/team/other.git", "git@github.com:sgl-project/sglang.git"])
def test_generated_script_new_choices(mode, repo):
    script = cli.startup_script(repo, mode, cli.DEFAULTS)
    assert "git config --global user.name" in script
    assert 'if [ -n "${GH_TOKEN:-}" ]; then gh auth setup-git' in script
    assert "mkdir -p /workspace" in script
    assert script.splitlines()[-1] == ("entrypoint.sh" if mode == "container" else "touch /root/.works")
    if repo == NO_REPO:
        assert "git clone" not in script and "uv sync" not in script
        assert "pyproject.toml" not in script and "/workspace/no-repo" not in script
    else:
        url, name = resolve_repo(repo, "minoya1220")
        assert f"git clone {url} /workspace/{name}" in script
        assert f"if [ -f /workspace/{name}/pyproject.toml ]; then uv sync --directory /workspace/{name}; fi" in script
    result = subprocess.run(["bash", "-n"], input=script, text=True, capture_output=True, check=False)
    assert result.returncode == 0, result.stderr


@pytest.mark.parametrize("flags", [["--no-repo"], ["--repo", "sgl-project/sglang"],
                                   ["--repo", "https://example.org/group/repo.git"],
                                   ["--repo", "git@github.com:sgl-project/sglang.git"],
                                   ["--repo", "gpu-kernels"]])
def test_cli_dry_run_skips_github_and_prompts(flags, monkeypatch, capsys):
    calls = []

    def fake_run(command):
        calls.append(command)
        assert command[:3] == ["vastai", "search", "offers"], "only read-only search allowed"
        return json.dumps([{"id": 42, "gpu_name": "Tesla T4", "num_gpus": 1,
                            "dph_total": 0.1, "reliability": 0.99, "verification": "verified"}])

    monkeypatch.setattr(cli, "run", fake_run)
    monkeypatch.setattr(cli, "interactive_terminal", lambda: False)
    monkeypatch.setattr("builtins.input", lambda _: pytest.fail("unexpected prompt"))
    assert cli.main(["up", *flags, "--mode", "container", "--gpu", "Tesla T4", "--yes", "--dry-run"]) == 0
    output = capsys.readouterr().out
    request = json.loads(output.split("DRY RUN: no instance created\n")[1])
    repo = NO_REPO if flags == ["--no-repo"] else flags[1]
    assert request[request.index("--label") + 1] == resolve_repo(repo, "minoya1220")[1]
    assert request[request.index("--onstart-cmd") + 1] == cli.startup_script(repo, "container", cli.DEFAULTS)
    assert len(calls) == 1
    assert not cli.history_path().exists()


def test_flag_conflict_and_invalid_repo_fail_before_network(monkeypatch, capsys):
    monkeypatch.setattr(cli, "run", lambda _: pytest.fail("unexpected network"))
    with pytest.raises(SystemExit) as error:
        cli.main(["up", "--no-repo", "--repo", "owner/repo", "--dry-run"])
    assert error.value.code == 2
    assert cli.main(["up", "--repo", "https://host/repo;id", "--dry-run"]) == 1
    assert "Use a bare repo name" in capsys.readouterr().err


def flow(repos=None):
    return LaunchWizard(repos=repos or Job(lambda: ["own-repo"]), search=None,
                        models=[], recent_gpus=[], choose_offer=False)


def test_no_repo_usable_while_github_is_blocked(drive):
    save_cached_repos("minoya1220", ["own-repo"])
    gate = threading.Event()
    job = Job(lambda: gate.wait(5) and ["own-repo"])

    async def script(keys):
        await keys.wait(lambda: job.state == "running")
        await keys.type("No repository")
        latency = await keys.press("\r", until=keys.on("Mode"))
        assert latency < 0.5
        await keys.press("2", until=keys.on("Checking repositories"))
        gate.set()

    try:
        assert drive(flow(job).next(Picks(gpu="Tesla T4")), script).repo == NO_REPO
    finally:
        gate.set()


@pytest.mark.parametrize("repo", ["https://github.com/sgl-project/sglang.git", "git@github.com:sgl-project/sglang.git", "sgl-project/sglang"])
def test_url_entered_inline_and_back_preserves_choices(drive, repo):
    async def script(keys):
        await keys.type("Clone from")
        await keys.press("\r", until=keys.on("Clone from URL"))
        await keys.press("\r")  # empty is not an implicit no-repo choice
        assert keys.picker.title == "Clone from URL"
        await keys.type("https://host/repo;id")
        await keys.press("\r")
        assert keys.picker.title == "Clone from URL"
        await keys.press("`")  # clear invalid input
        assert keys.picker.filter == ""
        await keys.press("`", until=keys.on("Repository"))
        await keys.press("\r", until=keys.on("Clone from URL"))
        await keys.type(repo + "x")
        await keys.press("\x7f")
        await keys.press("\r", until=keys.on("Mode"))
        await keys.press("`", until=keys.on("Clone from URL"))
        assert keys.picker.filter == repo
        await keys.press("\r", until=keys.on("Mode"))
        await keys.press("1")

    assert drive(flow().next(Picks(gpu="Tesla T4")), script) == Picks(repo, "container", "Tesla T4")


def test_github_failure_does_not_hide_static_choices(drive):
    def fail():
        raise RuntimeError("GitHub unavailable")

    job = Job(fail)

    async def script(keys):
        await keys.wait(lambda: job.state == "failed")
        assert "No repository" in keys.picker.labels
        assert "Clone from URL..." in keys.picker.labels
        await keys.type("Clone from")
        await keys.press("\r", until=keys.on("Clone from URL"))
        await keys.press("`", until=keys.on("Repository"))
        await keys.press("`")  # clear repository filter
        await keys.press("`")
        await keys.press("`")  # quit: twice on the first screen

    with pytest.raises(PickerCancelled):
        drive(flow(job).next(Picks()), script)


def test_cache_contains_only_repo_names_and_no_recent_rows(drive):
    save_cached_repos("minoya1220", ["own-repo", "https://user:token@host/repo"])
    assert load_cached_repos("minoya1220") == ["own-repo"]
    assert "token" not in cache_path("minoya1220").read_text()

    async def script(keys):
        assert keys.picker.labels == ["No repository", "Clone from URL...", "own-repo"]
        await keys.press("1")

    assert drive(flow().next(Picks(mode="vm", gpu="B200")), script).repo == NO_REPO


def test_paste_url_in_same_application(drive):
    repo = "https://github.com/sgl-project/sglang.git"

    async def script(keys):
        await keys.type("Clone from")
        await keys.press("\r", until=keys.on("Clone from URL"))
        await keys.press("\x1b[200~" + repo + "\x1b[201~")
        assert keys.picker.filter == repo
        await keys.press("\r")

    assert drive(flow().next(Picks(mode="container", gpu="B200")), script).repo == repo


@pytest.mark.parametrize(("answers", "repo"), [
    (["1"], NO_REPO),
    (["2", "sgl-project/sglang"], "sgl-project/sglang"),
])
def test_line_picker_new_choices_without_github(answers, repo, monkeypatch):
    def unavailable(_):
        raise RuntimeError("offline")

    class Search:
        def offers(self, mode, gpu):
            return [{"id": 42, "gpu_name": gpu}]

    monkeypatch.setattr(cli, "github_repos", unavailable)
    replies = iter(answers)
    monkeypatch.setattr("builtins.input", lambda _: next(replies))
    picks, _ = cli.prompt_for_picks(Picks(mode="vm", gpu="B200"), Search(), "minoya1220")
    assert picks.repo == repo


def test_optional_repository_load_can_retry(drive):
    attempts = []

    def load():
        attempts.append(True)
        if len(attempts) == 1:
            raise RuntimeError("offline")
        return ["own-repo"]

    job = Job(load)

    async def script(keys):
        await keys.wait(lambda: job.state == "failed")
        await keys.press("\x12")  # Ctrl-R
        await keys.wait(lambda: "own-repo" in keys.picker.labels)
        await keys.type("own-repo")
        await keys.press("\r")


    assert drive(flow(job).next(Picks(mode="vm", gpu="B200")), script).repo == "own-repo"
    assert len(attempts) == 2
