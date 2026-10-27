"""Editor selection and SSH launch. Blocking work runs only after the picker exits."""
from __future__ import annotations

import shlex
import shutil
import subprocess
from pathlib import Path
from urllib.parse import quote

from gpu.picker import Option, Screen, pick

EDITORS = {"cursor": "Cursor", "code": "VS Code", "zed": "Zed", "nvim": "Neovim"}
NVIM_VERSION = "v0.11.5"
# Official release asset digests: GitHub /repos/neovim/neovim/releases/tags/v0.11.5.
NVIM_SHA256 = {
    "x86_64": "b2f91117be5b5ea39edd7297156dc2a4a8df4add6c95a90809a8df19e7ab6f52",
    "arm64": "ea4f9a31b11cc1477ff014aebb7b207684e7280f94ffa97abdab6cacd9b98519",
}


def editor_screen() -> Screen:
    return Screen("Editor", options=lambda: [Option(key, label) for key, label in EDITORS.items()],
                  select=lambda value: value)


def select_editor() -> str:
    return pick(editor_screen())


def after_up_screen(last: str | None) -> Screen:
    def select(value):
        if value == "select" or (value == "default" and not last):
            return editor_screen()
        return last if value == "default" else "none"

    return Screen("Open workspace", options=lambda: [
        *([Option("default", f"Open in {EDITORS[last]}")] if last else []),
        Option("select", "Select editor..."), Option("none", "Don't open")], select=select)


def check_editor(editor: str) -> None:
    if editor not in EDITORS:
        raise ValueError(f"Unknown editor: {editor}")
    if editor != "nvim" and not shutil.which(editor):
        hints = {
            "cursor": "In Cursor's command palette, run 'Shell Command: Install cursor command in PATH'.",
            "code": "In VS Code's command palette, run 'Shell Command: Install code command in PATH'.",
            "zed": "In Zed's command palette, run 'cli: install'.",
        }
        raise RuntimeError(f"Required editor CLI '{editor}' not found on PATH. {hints[editor]}")


def nvim_bootstrap(config_url: str | None = None) -> str:
    script = f'''set -eu
export PATH="$HOME/.local/bin:$PATH"
if ! command -v nvim >/dev/null 2>&1; then
    case "$(uname -m)" in
        x86_64|amd64) arch=x86_64; digest={NVIM_SHA256['x86_64']} ;;
        aarch64|arm64) arch=arm64; digest={NVIM_SHA256['arm64']} ;;
        *) echo 'Unsupported Neovim CPU architecture' >&2; exit 1 ;;
    esac
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    asset="nvim-linux-$arch.tar.gz"
    base="https://github.com/neovim/neovim/releases/download/{NVIM_VERSION}"
    curl -fsSL "$base/$asset" -o "$tmp/$asset"
    (cd "$tmp"; printf '%s  %s\\n' "$digest" "$asset" > checksum; sha256sum -c checksum)
    tar -xzf "$tmp/$asset" -C "$tmp"
    mkdir -p "$HOME/.local"
    cp -R "$tmp/nvim-linux-$arch/." "$HOME/.local/"
fi
'''
    if config_url:
        script += f'''mkdir -p "$HOME/.config"
if [ -d "$HOME/.config/nvim/.git" ]; then
    git -C "$HOME/.config/nvim" remote set-url origin {shlex.quote(config_url)}
    git -C "$HOME/.config/nvim" pull --ff-only
else
    git clone -- {shlex.quote(config_url)} "$HOME/.config/nvim"
fi
'''
    return script


def nvim_session(directory: str) -> str:
    return (f'export PATH="$HOME/.local/bin:$PATH"; cd -- {shlex.quote(directory)} && '
            'if command -v tmux >/dev/null 2>&1; then '
            'exec tmux new-session -A -s gpu-nvim nvim; else exec nvim; fi')


def _checked(argv: list[str], **kwargs) -> None:
    # Never echo remote output: config URLs and git diagnostics can contain credentials.
    result = subprocess.run(argv, check=False, **kwargs)
    if result.returncode:
        raise RuntimeError(f"{argv[0]} failed while preparing or running Neovim (exit {result.returncode})")


def launch_editor(editor: str, alias: str, directory: str, config: str | None = None) -> None:
    check_editor(editor)
    if editor != "nvim":
        argv = (["zed", f"ssh://{alias}/{quote(directory.lstrip('/'), safe='/')}"] if editor == "zed"
                else [editor, "--remote", f"ssh-remote+{alias}", directory])
        # Wait for the CLI handoff, not the editor window; a failed handoff must not enter history.
        result = subprocess.run(argv, stdin=subprocess.DEVNULL, capture_output=True, check=False)
        if result.returncode:
            raise RuntimeError(f"{EDITORS[editor]} failed to open the remote workspace")
        return
    url = None
    local = None
    if config:
        if config.startswith(("https://", "http://", "ssh://", "git://", "git@")):
            url = config
        else:
            local = Path(config).expanduser().resolve()
            if not local.is_dir():
                raise ValueError("Neovim config must be a git URL or an existing local directory")
            if not shutil.which("rsync"):
                raise RuntimeError("Install rsync to upload a local Neovim config")
    _checked(["ssh", "-o", "BatchMode=yes", alias, "bash -s"],
             input=nvim_bootstrap(url), text=True, capture_output=True)
    if local:
        _checked(["ssh", alias, 'mkdir -p "$HOME/.config/nvim"'], capture_output=True)
        _checked(["rsync", "-az", "--", str(local) + "/", f"{alias}:.config/nvim/"],
                 capture_output=True)
    _checked(["ssh", "-t", alias, nvim_session(directory)])
