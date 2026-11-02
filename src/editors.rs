//! Editor selection and SSH launch. Blocking work runs only after the picker exits.

use std::path::{Path, PathBuf};

use crate::ctx::{Ctx, Env};
use crate::error::{Error, Result};
use crate::picker::{Next, Opt, Screen};
use crate::process::{Cmd, Input};
use crate::pyfmt::shell_quote;

pub const EDITORS: [(&str, &str); 4] = [
    ("cursor", "Cursor"),
    ("code", "VS Code"),
    ("zed", "Zed"),
    ("nvim", "Neovim"),
];
pub const NVIM_VERSION: &str = "v0.11.5";
// Official release asset digests: GitHub /repos/neovim/neovim/releases/tags/v0.11.5.
pub const NVIM_SHA256_X86_64: &str =
    "b2f91117be5b5ea39edd7297156dc2a4a8df4add6c95a90809a8df19e7ab6f52";
pub const NVIM_SHA256_ARM64: &str =
    "ea4f9a31b11cc1477ff014aebb7b207684e7280f94ffa97abdab6cacd9b98519";

pub fn editor_label(editor: &str) -> Option<&'static str> {
    EDITORS
        .iter()
        .find(|(key, _)| *key == editor)
        .map(|(_, label)| *label)
}

pub fn editor_screen() -> Screen<String, String> {
    Screen::new(
        "Choose editor",
        || {
            EDITORS
                .iter()
                .map(|(key, label)| Opt::new(key.to_string(), *label))
                .collect()
        },
        Next::Done,
    )
}

pub fn after_up_screen(last: Option<String>) -> Screen<String, String> {
    let labels = last.clone();
    let options = move || {
        let mut options = Vec::new();
        if let Some(last) = &labels {
            options.push(Opt::new(
                "default".to_string(),
                format!("Open in {}", editor_label(last).unwrap_or(last)),
            ));
        }
        options.push(Opt::new("select".to_string(), "Select editor..."));
        options.push(Opt::new("none".to_string(), "Don't open"));
        options
    };
    let select = move |value: String| {
        if value == "select" || (value == "default" && last.is_none()) {
            return Next::Screen(editor_screen());
        }
        match (&*value, &last) {
            ("default", Some(last)) => Next::Done(last.clone()),
            _ => Next::Done("none".into()),
        }
    };
    Screen::new("Open workspace", options, select)
}

/// `shutil.which`: the first executable `name` on `PATH`.
pub fn which(env: &Env, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let executable = |path: &Path| {
        std::fs::metadata(path).is_ok_and(|m| !m.is_dir() && m.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        let path = PathBuf::from(name);
        return executable(&path).then_some(path);
    }
    let search = env.get("PATH").unwrap_or("/bin:/usr/bin");
    search
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|path| executable(path))
}

pub fn check_editor(env: &Env, editor: &str) -> Result<()> {
    if editor_label(editor).is_none() {
        return Err(Error::msg(format!(
            "Unknown editor: {editor}. Use cursor, code, zed, or nvim."
        )));
    }
    if editor != "nvim" && which(env, editor).is_none() {
        let hint = match editor {
            "cursor" => {
                "In Cursor's command palette, run 'Shell Command: Install cursor command in PATH'."
            }
            "code" => {
                "In VS Code's command palette, run 'Shell Command: Install code command in PATH'."
            }
            _ => "In Zed's command palette, run 'cli: install'.",
        };
        return Err(Error::msg(format!("'{editor}' is not on PATH. {hint}")));
    }
    Ok(())
}

pub fn nvim_bootstrap(config_url: Option<&str>) -> String {
    nvim_bootstrap_with(config_url, NVIM_SHA256_X86_64, NVIM_SHA256_ARM64)
}

/// `nvim_bootstrap` with other release digests (tests use a local fixture).
pub fn nvim_bootstrap_with(
    config_url: Option<&str>,
    x86_64_digest: &str,
    arm64_digest: &str,
) -> String {
    let mut script = format!(
        r#"set -eu
export PATH="$HOME/.local/bin:$PATH"
if ! command -v nvim >/dev/null 2>&1; then
    case "$(uname -m)" in
        x86_64|amd64) arch=x86_64; digest={x86_64_digest} ;;
        aarch64|arm64) arch=arm64; digest={arm64_digest} ;;
        *) echo 'Unsupported Neovim CPU architecture' >&2; exit 1 ;;
    esac
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    asset="nvim-linux-$arch.tar.gz"
    base="https://github.com/neovim/neovim/releases/download/{NVIM_VERSION}"
    curl -fsSL "$base/$asset" -o "$tmp/$asset"
    (cd "$tmp"; printf '%s  %s\n' "$digest" "$asset" > checksum; sha256sum -c checksum)
    tar -xzf "$tmp/$asset" -C "$tmp"
    mkdir -p "$HOME/.local"
    cp -R "$tmp/nvim-linux-$arch/." "$HOME/.local/"
fi
"#
    );
    if let Some(url) = config_url.filter(|u| !u.is_empty()) {
        let url = shell_quote(url);
        script.push_str(&format!(
            r#"mkdir -p "$HOME/.config"
if [ -d "$HOME/.config/nvim/.git" ]; then
    git -C "$HOME/.config/nvim" remote set-url origin {url}
    git -C "$HOME/.config/nvim" pull --ff-only
else
    git clone -- {url} "$HOME/.config/nvim"
fi
"#
        ));
    }
    script
}

pub fn nvim_session(directory: &str) -> String {
    format!(
        "export PATH=\"$HOME/.local/bin:$PATH\"; cd -- {} && if command -v tmux >/dev/null 2>&1; then exec tmux new-session -A -s gpu-nvim nvim; else exec nvim; fi",
        shell_quote(directory)
    )
}

/// Run and report failure by exit code only: config URLs and git
/// diagnostics can contain credentials.
fn checked(ctx: &Ctx, cmd: Cmd) -> Result<()> {
    let program = cmd.program().to_string();
    let output = ctx
        .runner
        .run(&cmd)
        .map_err(|e| Error::msg(e.python_text(&program)))?;
    if output.code != 0 {
        return Err(Error::msg(format!(
            "{program} failed while preparing or running Neovim (exit {})",
            output.code
        )));
    }
    Ok(())
}

/// `os.path.expanduser` for `~` and `~/...`.
fn expand_user(env: &Env, path: &str) -> PathBuf {
    if path == "~" {
        env.home()
    } else if let Some(rest) = path.strip_prefix("~/") {
        env.home().join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// `urllib.parse.quote(text, safe='/')`.
fn url_quote(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~/".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn launch_editor(
    ctx: &Ctx,
    editor: &str,
    alias: &str,
    directory: &str,
    config: Option<&str>,
) -> Result<()> {
    check_editor(&ctx.env, editor)?;
    if editor != "nvim" {
        let argv: Vec<String> = if editor == "zed" {
            vec![
                "zed".into(),
                format!(
                    "ssh://{alias}/{}",
                    url_quote(directory.trim_start_matches('/'))
                ),
            ]
        } else {
            vec![
                editor.into(),
                "--remote".into(),
                format!("ssh-remote+{alias}"),
                directory.into(),
            ]
        };
        // Wait for the CLI handoff, not the editor window; a failed handoff must not enter history.
        let output = ctx
            .runner
            .run(&Cmd::new(&argv))
            .map_err(|e| Error::msg(e.python_text(editor)))?;
        if output.code != 0 {
            let label = editor_label(editor).unwrap_or(editor);
            return Err(Error::msg(format!(
                "{label} could not open {directory} on {alias}. Check the editor's Remote SSH setup; test SSH with: ssh {alias}"
            )));
        }
        return Ok(());
    }
    let mut url = None;
    let mut local = None;
    if let Some(config) = config.filter(|c| !c.is_empty()) {
        if ["https://", "http://", "ssh://", "git://", "git@"]
            .iter()
            .any(|p| config.starts_with(p))
        {
            url = Some(config);
        } else {
            let resolved = std::fs::canonicalize(expand_user(&ctx.env, config))
                .ok()
                .filter(|p| p.is_dir());
            let Some(resolved) = resolved else {
                return Err(Error::msg(
                    "Neovim config path is not a directory. Set --nvim-config to an existing directory or a git URL.",
                ));
            };
            if which(&ctx.env, "rsync").is_none() {
                return Err(Error::msg("Install rsync to upload a local Neovim config"));
            }
            local = Some(resolved);
        }
    }
    checked(
        ctx,
        Cmd::new(&["ssh", "-o", "BatchMode=yes", alias, "bash -s"])
            .stdin(Input::Text(nvim_bootstrap(url))),
    )?;
    if let Some(local) = local {
        checked(
            ctx,
            Cmd::new(&["ssh", alias, "mkdir -p \"$HOME/.config/nvim\""]).stdin(Input::Inherit),
        )?;
        let source = format!("{}/", local.display());
        let target = format!("{alias}:.config/nvim/");
        checked(
            ctx,
            Cmd::new(&["rsync", "-az", "--", &source, &target]).stdin(Input::Inherit),
        )?;
    }
    // The final ssh owns the terminal.
    checked(
        ctx,
        Cmd::new(&["ssh", "-t", alias, &nvim_session(directory)])
            .stdin(Input::Inherit)
            .capture(false),
    )
}
