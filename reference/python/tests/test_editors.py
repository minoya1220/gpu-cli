"""Editor tests never contact Vast, GitHub, or an SSH server."""
import hashlib
import io
import shlex
import shutil
import subprocess
import sys
import tarfile

import pytest

from gpu import cli, editors


@pytest.fixture
def commands(monkeypatch):
    calls = []
    monkeypatch.setattr(editors.shutil, "which", lambda name: f"/bin/{name}")

    def run(argv, **kwargs):
        calls.append((argv, kwargs))
        return subprocess.CompletedProcess(argv, 0)

    monkeypatch.setattr(editors.subprocess, "run", run)
    return calls


@pytest.mark.parametrize("editor,expected", [
    ("cursor", ["cursor", "--remote", "ssh-remote+vast-12", "/workspace/repo"]),
    ("code", ["code", "--remote", "ssh-remote+vast-12", "/workspace/repo"]),
    ("zed", ["zed", "ssh://vast-12/workspace/repo"]),
])
def test_gui_commands(commands, editor, expected):
    editors.launch_editor(editor, "vast-12", "/workspace/repo")
    assert commands[0][0] == expected


@pytest.mark.parametrize("editor", ["cursor", "code", "zed"])
def test_missing_cli(monkeypatch, editor):
    monkeypatch.setattr(editors.shutil, "which", lambda name: None)
    with pytest.raises(RuntimeError, match="not found on PATH.*[Ii]nstall"):
        editors.launch_editor(editor, "vast-12", "/workspace")


@pytest.mark.parametrize("source", [None, "https://example.com/config.git", "git@example.com:config.git", "local"])
def test_nvim_commands(commands, source, tmp_path):
    config = tmp_path / "my config"
    config.mkdir()
    editors.launch_editor("nvim", "vast-12", "/workspace/repo", str(config) if source == "local" else source)
    assert commands[0][0] == ["ssh", "-o", "BatchMode=yes", "vast-12", "bash -s"]
    script = commands[0][1]["input"]
    assert "sha256sum -c checksum" in script
    assert editors.NVIM_VERSION in script
    assert "arm64" in script and "x86_64" in script
    if source and source != "local":
        assert f"git clone -- {shlex.quote(source)}" in script
        assert "pull --ff-only" in script
    else:
        assert "git clone" not in script
    if source == "local":
        assert commands[2][0] == ["rsync", "-az", "--", str(config) + "/", "vast-12:.config/nvim/"]
    assert commands[-1][0] == ["ssh", "-t", "vast-12", editors.nvim_session("/workspace/repo")]
    assert "capture_output" not in commands[-1][1]  # Own the terminal, not a background process.


def test_safe_config_and_directory_quoting(commands):
    url = "https://example.com/it's;$(touch BAD).git"
    editors.launch_editor("nvim", "vast-12", "/workspace/a'; touch BAD; '", url)
    assert shlex.quote(url) in commands[0][1]["input"]
    assert shlex.quote("/workspace/a'; touch BAD; '") in commands[-1][0][-1]


@pytest.mark.parametrize("success", [True, False])
def test_history_only_after_success(monkeypatch, commands, success):
    monkeypatch.setattr(cli, "wait_for_ssh", lambda *a, **kw: ("vast-12", "/workspace"))
    if not success:
        monkeypatch.setattr(editors.subprocess, "run", lambda *a, **kw: subprocess.CompletedProcess(a, 1))
        with pytest.raises(RuntimeError):
            cli.edit_instance("12", "code")
        assert cli.last_editor() is None
    else:
        cli.edit_instance("12", "code")
        assert cli.last_editor() == "code"
        cli.edit_instance("12")
        assert cli.recent_picks("editor") == ["code", "code"]


def test_no_history_noninteractive(monkeypatch):
    monkeypatch.setattr(cli, "interactive_terminal", lambda: False)
    with pytest.raises(RuntimeError, match="specify --editor"):
        cli.edit_instance("12")


@pytest.mark.parametrize("last,choice,expected", [
    ("zed", 0, "zed"), ("code", 1, "cursor"), ("code", 2, "none"),
    (None, 0, "cursor"), (None, 1, "none"),
])
def test_after_up_real_keys(drive, last, choice, expected):
    async def select(keys):
        await keys.wait(keys.on("Open workspace"))
        assert keys.picker.labels == (
            [f"Open in {editors.EDITORS[last]}", "Select editor...", "Don't open"]
            if last else ["Select editor...", "Don't open"]
        )
        for _ in range(choice):
            await keys.press("\x1b[B")
        await keys.press("\r")
        if (last and choice == 1) or (choice == 0 and last is None):
            await keys.wait(keys.on("Editor"))
            await keys.press("\r")
    assert drive(editors.after_up_screen(last), select) == expected


@pytest.mark.parametrize("index,expected", list(enumerate(editors.EDITORS)))
def test_editor_real_keys(drive, index, expected):
    async def select(keys):
        await keys.wait(keys.on("Editor"))
        for _ in range(index):
            await keys.press("\x1b[B")
        await keys.press("\r")
    assert drive(editors.editor_screen(), select) == expected


def test_edit_flag_config_override_and_alias(monkeypatch):
    calls = []
    monkeypatch.setattr(cli, "load_config", lambda: {"editor": {"nvim_config": "stored"}})
    monkeypatch.setattr(cli, "edit_instance", lambda *a, **kw: calls.append((a, kw)))
    assert cli.main(["edit", "12", "--editor", "nvim", "--nvim-config", "override"]) == 0
    assert calls[-1] == (("12", "nvim"), {"nvim_config": "override"})
    assert cli.main(["edit", "12", "--editor", "nvim"]) == 0
    assert calls[-1][1] == {"nvim_config": "stored"}
    assert cli.main(["cursor", "12"]) == 0
    assert calls[-1][0] == ("12", "cursor")


@pytest.mark.parametrize("tmux", [True, False])
def test_nvim_session_shell(tmp_path, tmux):
    # Execute the generated shell with only fake editor binaries on PATH.
    bin_dir = tmp_path / ".local/bin"
    bin_dir.mkdir(parents=True)
    nvim = bin_dir / "nvim"
    nvim.write_text("#!/bin/sh\necho direct\n")
    nvim.chmod(0o755)
    if tmux:
        binary = bin_dir / "tmux"
        binary.write_text('#!/bin/sh\nprintf "%s\\n" "$*"\n')
        binary.chmod(0o755)
    result = subprocess.run(["/bin/sh", "-c", editors.nvim_session(str(tmp_path))],
                            env={"HOME": str(tmp_path), "PATH": str(bin_dir)},
                            capture_output=True, text=True, check=True)
    assert result.stdout.strip() == ("new-session -A -s gpu-nvim nvim" if tmux else "direct")


def test_existing_nvim_skips_install(tmp_path):
    bin_dir = tmp_path / ".local/bin"
    bin_dir.mkdir(parents=True)
    binary = bin_dir / "nvim"
    binary.write_text("#!/bin/sh\nexit 0\n")
    binary.chmod(0o755)
    subprocess.run(["/bin/bash", "-s"], input=editors.nvim_bootstrap(), text=True,
                   env={"HOME": str(tmp_path), "PATH": str(bin_dir)}, check=True)


@pytest.mark.parametrize("cpu,arch,valid", [
    ("x86_64", "x86_64", True), ("aarch64", "arm64", True),
    ("x86_64", "x86_64", False),
])
def test_missing_nvim_installs_only_verified_archive(tmp_path, monkeypatch, cpu, arch, valid):
    # Real extraction/copy with a tiny local release fixture; no network or remote host.
    archive = tmp_path / "release.tar.gz"
    with tarfile.open(archive, "w:gz") as tar:
        data = b"#!/bin/sh\nexit 0\n"
        info = tarfile.TarInfo(f"nvim-linux-{arch}/bin/nvim")
        info.mode, info.size = 0o755, len(data)
        tar.addfile(info, io.BytesIO(data))
    digest = hashlib.sha256(archive.read_bytes()).hexdigest() if valid else "0" * 64
    monkeypatch.setitem(editors.NVIM_SHA256, arch, digest)
    tools = tmp_path / "tools"
    tools.mkdir()
    for tool in ("mktemp", "rm", "tar", "mkdir", "cp"):
        (tools / tool).symlink_to(shutil.which(tool))

    def executable(name, body):
        path = tools / name
        path.write_text(body)
        path.chmod(0o755)

    executable("uname", f"#!/bin/sh\necho {cpu}\n")
    executable("curl", f'#!/bin/sh\ncp {shlex.quote(str(archive))} "$4"\n')
    executable("sha256sum", f"#!{sys.executable}\n" +
               "import hashlib, pathlib, sys\n" +
               "digest, name = pathlib.Path('checksum').read_text().split()\n" +
               "sys.exit(0 if hashlib.sha256(pathlib.Path(name).read_bytes()).hexdigest() == digest else 1)\n")
    result = subprocess.run(["/bin/bash", "-s"], input=editors.nvim_bootstrap(), text=True,
                            env={"HOME": str(tmp_path), "PATH": str(tools)}, capture_output=True, check=False)
    assert (result.returncode == 0) == valid, result.stderr
    assert (tmp_path / ".local/bin/nvim").exists() == valid


@pytest.mark.parametrize("existing", [True, False])
def test_git_config_shell_is_quoted_and_updates(tmp_path, existing):
    tools = tmp_path / ".local/bin"
    tools.mkdir(parents=True)
    for name, body in {
        "nvim": "#!/bin/sh\nexit 0\n",
        "git": f"#!{sys.executable}\nimport json, os, sys\n"
               "with open(os.environ['HOME'] + '/git-calls', 'a') as f: f.write(json.dumps(sys.argv[1:]) + '\\n')\n",
    }.items():
        binary = tools / name
        binary.write_text(body)
        binary.chmod(0o755)
    (tools / "mkdir").symlink_to(shutil.which("mkdir"))
    if existing:
        (tmp_path / ".config/nvim/.git").mkdir(parents=True)
    source = "https://example.com/it's;$(touch BAD).git"
    subprocess.run(["/bin/bash", "-s"], input=editors.nvim_bootstrap(source), text=True,
                   env={"HOME": str(tmp_path), "PATH": str(tools)}, check=True)
    import json
    calls = [json.loads(line) for line in (tmp_path / "git-calls").read_text().splitlines()]
    config = str(tmp_path / ".config/nvim")
    assert calls == ([["-C", config, "remote", "set-url", "origin", source],
                      ["-C", config, "pull", "--ff-only"]] if existing else
                     [["clone", "--", source, config]])


def test_invalid_local_config_does_not_run_ssh(commands, tmp_path):
    with pytest.raises(ValueError, match="existing local directory"):
        editors.launch_editor("nvim", "vast-12", "/workspace", str(tmp_path / "missing"))
    assert commands == []
