//! gcloud owns SSH key provisioning and OS Login. Editors reuse its resolved
//! OpenSSH command, not a guessed root user, IP, or copied cloud credential.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::gcp::{self, TpuId};
use crate::pyjson::{Dict, Value};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

fn ssh_args(id: &TpuId) -> Vec<String> {
    let mut args = id.args("ssh");
    args.push("--worker=0".into());
    args
}
pub fn ssh(ctx: &Ctx, id: &TpuId, exec: bool) -> Result<()> {
    ctx.require_gcloud()?;
    let args = ssh_args(id);
    if exec {
        return Err(Error::msg(ctx.exec(&args)));
    }
    ctx.println(&crate::pyfmt::shell_join(&args))
}
fn alias(id: &TpuId) -> String {
    format!("gcp_{}_{}_{}", id.project, id.zone, id.name)
}
fn quote(value: &str) -> Result<String> {
    if value.chars().any(char::is_control) {
        return Err(gcp::bad_reply());
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// Translate only OpenSSH options used by gcloud. Reject unfamiliar switches
/// instead of turning tool output into arbitrary SSH config directives.
pub fn stanza(id: &TpuId, command: &str) -> Result<String> {
    let words = shlex::split(command).ok_or_else(gcp::bad_reply)?;
    if !words
        .first()
        .is_some_and(|w| w.rsplit('/').next() == Some("ssh"))
    {
        return Err(gcp::bad_reply());
    }
    let mut lines = Vec::new();
    let mut destination = None;
    let mut i = 1;
    while i < words.len() {
        let word = &words[i];
        match word.as_str() {
            "-t" | "-T" | "--" => {}
            "-i" | "-p" | "-l" | "-o" => {
                i += 1;
                let value = words.get(i).ok_or_else(gcp::bad_reply)?;
                if word == "-o" {
                    let (key, val) = value.split_once('=').ok_or_else(gcp::bad_reply)?;
                    if ![
                        "CheckHostIP",
                        "HashKnownHosts",
                        "HostKeyAlias",
                        "IdentitiesOnly",
                        "StrictHostKeyChecking",
                        "UserKnownHostsFile",
                        "LogLevel",
                        "ConnectTimeout",
                    ]
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(key))
                    {
                        return Err(Error::msg(format!("Unsupported SSH option; use a direct connection. Next: gpu ssh {} --exec", id.cli())));
                    }
                    lines.push(format!("  {key} {}\n", quote(val)?));
                } else {
                    let key = match word.as_str() {
                        "-i" => "IdentityFile",
                        "-p" => "Port",
                        _ => "User",
                    };
                    lines.push(format!("  {key} {}\n", quote(value)?));
                }
            }
            _ if !word.starts_with('-') && destination.is_none() => {
                destination = Some(word.clone());
            }
            _ => return Err(gcp::bad_reply()),
        }
        i += 1;
    }
    let destination = destination.ok_or_else(gcp::bad_reply)?;
    let (user, host) = destination
        .split_once('@')
        .map_or((None, destination.as_str()), |(u, h)| (Some(u), h));
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".:-[]".contains(&b))
    {
        return Err(gcp::bad_reply());
    }
    let mut result = format!("Host {}\n  HostName {}\n", alias(id), quote(host)?);
    if let Some(user) = user {
        result.push_str(&format!("  User {}\n", quote(user)?));
    }
    for line in lines {
        result.push_str(&line);
    }
    Ok(result)
}

fn without(content: &str, id: &TpuId) -> String {
    let wanted = format!("Host {}", alias(id));
    let mut keep = true;
    let mut out = String::new();
    for line in content.lines() {
        if line.starts_with("Host ") {
            keep = line != wanted;
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}
fn save(ctx: &Ctx, id: &TpuId, stanza: &str) -> Result<()> {
    let dir = ctx.env.home().join(".ssh");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let config = dir.join("config");
    let old = if config.exists() {
        std::fs::read_to_string(&config)?
    } else {
        String::new()
    };
    let include = "Include ~/.ssh/gpu_gcp_config";
    if !old.lines().any(|l| l.trim().eq_ignore_ascii_case(include)) {
        std::fs::write(&config, format!("{include}\n{old}"))?;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600))?;
    }
    let path = dir.join("gpu_gcp_config");
    let old = if path.exists() {
        std::fs::read_to_string(&path)?
    } else {
        String::new()
    };
    std::fs::write(&path, format!("{}{stanza}", without(&old, id)))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
pub fn remove(ctx: &Ctx, id: &TpuId) -> Result<()> {
    let path = ctx.env.home().join(".ssh/gpu_gcp_config");
    if path.exists() {
        std::fs::write(&path, without(&std::fs::read_to_string(&path)?, id))?;
    }
    Ok(())
}

pub fn edit(
    ctx: &Ctx,
    cfg: &Dict,
    id: &TpuId,
    editor: Option<&str>,
    nvim_config: Option<&Value>,
) -> Result<()> {
    ctx.require_gcloud()?;
    let editor = match editor
        .map(String::from)
        .or_else(|| crate::commands::last_editor(&ctx.env))
    {
        Some(e) => e,
        None if ctx.interactive() => crate::picker::pick(ctx, crate::editors::editor_screen())?,
        None => {
            return Err(Error::msg(format!(
                "Choose an editor. Next: gpu edit {} --editor code",
                id.cli()
            )))
        }
    };
    crate::editors::check_editor(&ctx.env, &editor)?;
    let node = gcp::describe(ctx, id)?;
    if gcp::text(&node, "state") != Some("READY") {
        return Err(Error::msg(
            "Wait for the TPU to become READY. Next: gpu ls --provider gcp",
        ));
    }
    let directory = node
        .get("metadata")
        .and_then(Value::as_dict)
        .and_then(|m| gcp::text(m, "gpu-workspace"))
        .unwrap_or("/workspace");
    if directory != "/workspace"
        && !directory.strip_prefix("/workspace/").is_some_and(|s| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
    {
        return Err(gcp::bad_reply());
    }
    let mut probe = ssh_args(id);
    probe.push("--command=true".into());
    gcp::run(ctx, &probe, &format!("gpu ssh {} --exec", id.cli()))?;
    let mut dry = ssh_args(id);
    dry.push("--dry-run".into());
    let command = gcp::run(ctx, &dry, &format!("gpu ssh {} --exec", id.cli()))?;
    save(ctx, id, &stanza(id, &command)?)?;
    let configured = crate::config::nvim_config(cfg)?;
    let nvim = if editor == "nvim" {
        crate::config::nvim_config_text(nvim_config.or(configured.as_ref()))?
    } else {
        None
    };
    crate::editors::launch_editor(ctx, &editor, &alias(id), directory, nvim.as_deref())?;
    crate::history::record_picks(&ctx.env, &[("editor", &editor)])?;
    ctx.println(&format!(
        "Opened {} at {directory} on {}.",
        crate::editors::editor_label(&editor).unwrap_or(&editor),
        id.cli()
    ))
}
