//! gcloud owns SSH key provisioning and OS Login. Editors reuse its resolved
//! OpenSSH command, not a guessed root user, IP, or copied cloud credential.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::gcp::{self, TpuId};
use crate::pyjson::{Dict, Value};

fn ssh_args(id: &TpuId) -> Vec<String> {
    let mut args = id.args("ssh");
    args.push("--worker=0".into());
    args
}
pub fn ssh(ctx: &Ctx, id: &TpuId, exec: bool) -> Result<()> {
    ctx.require_gcloud()?;
    let mut args = ssh_args(id);
    args.extend(crate::ssh::gcloud_options(false));
    if exec {
        return Err(Error::msg(ctx.exec(&args)));
    }
    ctx.result("ssh_command", Value::str(crate::pyfmt::shell_join(&args)));
    ctx.println(&crate::pyfmt::shell_join(&args))
}
pub fn alias(id: &TpuId) -> String {
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
                        return Err(
                            Error::msg("Unsupported SSH option; use a direct connection")
                                .next(format!("gpu ssh {} --exec", id.cli())),
                        );
                    }
                    let value = quote(val)?;
                    if !key.eq_ignore_ascii_case("ConnectTimeout") {
                        lines.push(format!("  {key} {value}\n"));
                    }
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
    result.push_str("  ServerAliveInterval 15\n  ServerAliveCountMax 4\n");
    result.push_str(&crate::ssh::config_timeout());
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
    let config = dir.join("config");
    let include = "Include ~/.ssh/gpu_gcp_config";
    crate::atomic_file::update(&config, |original| {
        let old = String::from_utf8_lossy(original);
        if old.lines().any(|l| l.trim().eq_ignore_ascii_case(include)) {
            Ok(original.to_vec())
        } else {
            Ok(format!("{include}\n{old}").into_bytes())
        }
    })?;
    let path = dir.join("gpu_gcp_config");
    crate::atomic_file::update(&path, |original| {
        let old = String::from_utf8_lossy(original);
        let wanted = format!("Host {}", alias(id));
        let block = old
            .split_inclusive('\n')
            .skip_while(|line| line.trim_end() != wanted)
            .enumerate()
            .take_while(|(n, line)| *n == 0 || !line.starts_with("Host "))
            .map(|(_, line)| line)
            .collect::<String>();
        if block == stanza {
            return Ok(original.to_vec());
        }
        Ok(format!("{}{stanza}", without(&old, id)).into_bytes())
    })?;
    Ok(())
}
pub fn has(ctx: &Ctx, id: &TpuId) -> Result<bool> {
    let path = ctx.env.home().join(".ssh/gpu_gcp_config");
    if !path.exists() {
        return Ok(false);
    }
    let wanted = format!("Host {}", alias(id));
    Ok(std::fs::read_to_string(&path)?
        .lines()
        .any(|line| line == wanted))
}
pub fn remove(ctx: &Ctx, id: &TpuId) -> Result<()> {
    let path = ctx.env.home().join(".ssh/gpu_gcp_config");
    if !path.exists() {
        return Ok(());
    }
    crate::atomic_file::update(&path, |original| {
        Ok(without(&String::from_utf8_lossy(original), id).into_bytes())
    })
}

fn probe(ctx: &Ctx, args: &[String], next: &str, timeout: f64) -> Result<bool> {
    let result = ctx.run(&crate::process::Cmd::new(args).timeout(Some(timeout)));
    match &result {
        Ok(out) if out.code == 0 => Ok(true),
        Err(crate::process::RunError::TimedOut) => Ok(false),
        Ok(out)
            if {
                // gcloud wraps OpenSSH's 255 in its own exit status. A newly
                // published key can also briefly fail public-key authentication.
                let detail = format!("{} {}", out.stdout, out.stderr).to_ascii_lowercase();
                out.code == 255
                    || [
                        "[255]",
                        "permission denied (publickey",
                        "connection refused",
                        "connection timed out",
                        "connection reset",
                        "no route to host",
                    ]
                    .iter()
                    .any(|s| detail.contains(s))
            } =>
        {
            Ok(false)
        }
        _ => crate::provider_errors::gcp(result, next).map(|_| true),
    }
}

/// Resolve gcloud's SSH alias after a bounded readiness probe.
pub fn ready_alias(ctx: &Ctx, id: &TpuId, timeout: f64) -> Result<String> {
    let poll = ctx.polling(timeout, 2.0);
    let next = format!(
        "gpu wait {} --timeout {}",
        id.cli(),
        crate::pyfmt::general(timeout)
    );
    wait_for_ssh(ctx, id, &next, timeout)?;
    let mut dry = ssh_args(id);
    dry.push("--dry-run".into());
    loop {
        let remaining = poll.remaining();
        if remaining <= 0.0 {
            return Err(Error::status(
                crate::error::ExitCode::WaitTimeout,
                format!("Timed out resolving SSH for {}", id.cli()),
            )
            .next(next));
        }
        match gcp::run_timeout(ctx, &dry, &next, remaining.min(15.0))
            .and_then(|command| stanza(id, &command))
        {
            Ok(stanza) => {
                save(ctx, id, &stanza)?;
                return Ok(alias(id));
            }
            Err(error) if error.retryable() => {
                poll.pause();
            }
            Err(error) => return Err(error),
        }
    }
}

/// Use the same bounded readiness/SSH wait as Vast, without changing node
/// lifecycle or its already armed expiry. All work runs after the picker exits.
fn wait_for_ssh(ctx: &Ctx, id: &TpuId, next: &str, timeout: f64) -> Result<Dict> {
    let poll = ctx.polling(timeout, 2.0);
    let timed_out = |reason: &str| {
        Error::status(
            crate::error::ExitCode::WaitTimeout,
            format!(
                "Timed out waiting for TPU {} and SSH after {}s ({reason}); expiry is unchanged",
                id.cli(),
                crate::pyfmt::general(timeout)
            ),
        )
        .next(next)
    };
    ctx.progress(&format!(
        "Waiting for TPU {} and SSH (up to {}s)...",
        id.cli(),
        crate::pyfmt::general(timeout)
    ))?;
    let mut previous = String::new();
    let mut reason = "provider readiness not observed".to_string();
    loop {
        let remaining = poll.remaining();
        if remaining <= 0.0 {
            return Err(timed_out(&reason));
        }
        let node = gcp::json_timeout(ctx, id.args("describe"), next, remaining.min(15.0))
            .and_then(|value| value.as_dict().cloned().ok_or_else(gcp::bad_reply));
        let node = match node {
            Ok(node) if gcp::text(&node, "state").is_some() => node,
            Ok(_) => {
                reason = gcp::bad_reply().text().into();
                poll.pause();
                continue;
            }
            Err(error) if error.retryable() => {
                reason = error.text().into();
                poll.pause();
                continue;
            }
            Err(error) => return Err(error),
        };
        let state = gcp::text(&node, "state").ok_or_else(gcp::bad_reply)?;
        if ![
            "READY",
            "CREATING",
            "STARTING",
            "RESTARTING",
            "REIMAGING",
            "REPAIRING",
            "STOPPED",
            "STOPPING",
            "DELETING",
            "PREEMPTED",
            "TERMINATED",
        ]
        .contains(&state)
        {
            reason = gcp::bad_reply().text().into();
            poll.pause();
            continue;
        }
        if state != previous {
            ctx.progress(&format!("TPU {}: {state}", id.cli()))?;
            previous = state.into();
        }
        reason = format!("provider status: {state}");
        match state {
            "READY" => {
                let mut ssh = ssh_args(id);
                ssh.push("--command=true".into());
                ssh.extend(crate::ssh::gcloud_options(true));
                ssh.push("--ssh-flag=-T".into());
                let remaining = poll.remaining();
                if remaining <= 0.0 {
                    return Err(timed_out(&reason));
                }
                let answering = match probe(ctx, &ssh, next, remaining.min(15.0)) {
                    Ok(answering) => answering,
                    Err(error) if error.retryable() => false,
                    Err(error) => return Err(error),
                };
                if answering {
                    return Ok(node);
                }
                reason = "TPU is READY but SSH is not answering".into();
                if poll.expired() {
                    return Err(timed_out(&reason));
                }
                ctx.progress("TPU is READY; waiting for SSH...")?;
            }
            "CREATING" | "STARTING" | "RESTARTING" | "REIMAGING" | "REPAIRING" => {}
            _ => {
                return Err(
                    Error::msg(format!("TPU {} is {state}; cannot answer SSH", id.cli()))
                        .next("gpu ls --provider gcp"),
                )
            }
        }
        let remaining = poll.remaining();
        if remaining > 0.0 {
            poll.pause();
        }
    }
}
fn workspace(node: &Dict) -> Result<&str> {
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
    Ok(directory)
}

/// Wait for SSH and write the managed alias an editor connects through.
pub(crate) fn editor_target(ctx: &Ctx, id: &TpuId, editor: &str) -> Result<crate::editors::Target> {
    let node = wait_for_ssh(
        ctx,
        id,
        &format!("gpu edit {} --editor {editor}", id.cli()),
        crate::editors::EDITOR_WAIT,
    )?;
    let directory = workspace(&node)?.to_string();
    let mut dry = ssh_args(id);
    dry.push("--dry-run".into());
    let command = gcp::run_timeout(ctx, &dry, &format!("gpu ssh {} --exec", id.cli()), 15.0)?;
    save(ctx, id, &stanza(id, &command)?)?;
    Ok(crate::editors::Target {
        alias: alias(id),
        directory,
        shown: id.cli(),
    })
}
