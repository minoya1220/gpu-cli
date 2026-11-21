//! Editor selection and SSH launch. Blocking work runs only after the picker exits.

use std::path::PathBuf;

use crate::ctx::{Ctx, Env};
use crate::error::{Error, Result};
use crate::picker::{Next, Opt, Screen};
use crate::process::{Cmd, Input};
use crate::pyfmt::shell_quote;
use crate::pyjson::Value;

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

/// How long `gpu edit` waits for SSH before giving up.
pub(crate) const EDITOR_WAIT: f64 = 120.0;

/// Where an editor connects to an instance.
pub(crate) struct Target {
    pub alias: String,
    pub directory: String,
    /// How the closing message names the instance.
    pub shown: String,
}

/// The most recent editor picked on this machine.
pub fn last_editor(env: &Env) -> Option<String> {
    crate::history::recent_picks(env, "editor")
        .into_iter()
        .find(|e| EDITORS.iter().any(|(key, _)| key == e))
}

/// The one `gpu edit` flow: choose and check the editor, let the provider
/// wait for its SSH target, launch, and remember the choice.
pub(crate) fn open(
    ctx: &Ctx,
    id: &str,
    editor: Option<&str>,
    nvim_config: Option<&Value>,
    connect: impl FnOnce(&str) -> Result<Target>,
) -> Result<()> {
    let editor = match editor.map(String::from).or_else(|| last_editor(&ctx.env)) {
        Some(editor) => editor,
        None if ctx.interactive() => crate::picker::pick(ctx, editor_screen())?,
        None => {
            let keys = EDITORS.map(|(key, _)| key).join("|");
            return Err(
                Error::usage(format!("No previous editor; specify --editor {keys}"))
                    .next(format!("gpu edit {id} --editor code")),
            );
        }
    };
    check_editor(&ctx.env, &editor)?;
    let mut target = connect(&editor)?;
    // A directory linked to this instance is the project: open its twin.
    if let Some(directory) = crate::dirsync::linked_directory(&ctx.env, id) {
        target.directory = directory;
    }
    ctx.result("ssh_alias", Value::str(&target.alias));
    ctx.result("directory", Value::str(&target.directory));
    ctx.result("editor", Value::str(&editor));
    let nvim_config = if editor == "nvim" {
        crate::config::nvim_config_text(nvim_config)?
    } else {
        None
    };
    launch_editor(
        ctx,
        &editor,
        &target.alias,
        &target.directory,
        nvim_config.as_deref(),
    )?;
    crate::history::record_picks_best_effort(ctx, &[("editor", &editor)]);
    ctx.println(&format!(
        "Opened {} at {} on {}.",
        editor_label(&editor).unwrap_or(&editor),
        target.directory,
        target.shown
    ))
}

/// After an interactive launch, offer to open the new instance in an editor.
pub(crate) fn offer_after_launch(
    ctx: &Ctx,
    cfg: &crate::pyjson::Dict,
    instance: &dyn crate::provider::Instance,
) -> Result<()> {
    if !ctx.interactive() || ctx.editor_offer_deferred() {
        return Ok(());
    }
    let editor = crate::picker::pick(ctx, after_up_screen(last_editor(&ctx.env)))?;
    if editor == "none" {
        return Ok(());
    }
    instance.edit(
        ctx,
        Some(&editor),
        crate::config::nvim_config(cfg)?.as_ref(),
    )
}

pub fn editor_label(editor: &str) -> Option<&'static str> {
    EDITORS
        .iter()
        .find(|(key, _)| *key == editor)
        .map(|(_, label)| *label)
}

pub fn editor_screen() -> Screen<String, String> {
    Screen::new(
        "Editor",
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
        if labels.is_some() {
            options.push(Opt::new("default".to_string(), "Open editor"));
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

pub fn check_editor(env: &Env, editor: &str) -> Result<()> {
    if editor_label(editor).is_none() {
        return Err(Error::msg(format!("Unknown editor: {editor}")));
    }
    if editor != "nvim" && crate::process::which(env, editor).is_none() {
        let hint = match editor {
            "cursor" => {
                "In Cursor's command palette, run 'Shell Command: Install cursor command in PATH'."
            }
            "code" => {
                "In VS Code's command palette, run 'Shell Command: Install code command in PATH'."
            }
            _ => "In Zed's command palette, run 'cli: install'.",
        };
        return Err(Error::msg(format!(
            "Required editor CLI '{editor}' not found on PATH. {hint}"
        )));
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
        "export PATH=\"$HOME/.local/bin:$PATH\"; cd -- {} && if command -v tmux >/dev/null 2>&1; then exec tmux \
            new-session -A -s gpu-nvim nvim; else exec nvim; fi",
        shell_quote(directory)
    )
}

/// Run and report failure by exit code only: config URLs and git
/// diagnostics can contain credentials.
fn checked(ctx: &Ctx, cmd: Cmd) -> Result<()> {
    let program = cmd.program().to_string();
    let output = ctx
        .run(&cmd)
        .map_err(|e| Error::msg(e.describe(&program)))?;
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

/// Percent-encode an editor URI path while preserving slashes.
fn url_quote(text: &str) -> String {
    crate::url::encode(text, b"-._~/")
}

pub fn launch_editor(
    ctx: &Ctx,
    editor: &str,
    alias: &str,
    directory: &str,
    config: Option<&str>,
) -> Result<()> {
    if ctx.json() && editor == "nvim" {
        return Err(Error::usage(
            "Neovim needs a terminal; use --editor code or gpu exec ID -- COMMAND with --json.",
        ));
    }
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
            .run(&Cmd::new(&argv))
            .map_err(|e| Error::msg(e.describe(editor)))?;
        if output.code != 0 {
            let label = editor_label(editor).unwrap_or(editor);
            return Err(Error::msg(format!(
                "{label} failed to open the remote workspace"
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
                    "Neovim config must be a git URL or an existing local directory",
                ));
            };
            if crate::process::which(&ctx.env, "rsync").is_none() {
                return Err(Error::msg("Install rsync to upload a local Neovim config"));
            }
            local = Some(resolved);
        }
    }
    checked(
        ctx,
        crate::ssh::command(&["ssh"], false, alias, "bash -s")
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
