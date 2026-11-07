//! `ls`, `ssh`, `stop`, `down`, `edit`, `cursor` and `expire`, plus the
//! polling shared by them.

use jiff::Timestamp;

use crate::ctx::Ctx;
use crate::editors::{check_editor, editor_label, editor_screen, launch_editor, EDITORS};
use crate::error::{Error, Result};
use crate::expiry::{expiry_summary, manage_expiry};
use crate::history::{recent_picks, record_picks};
use crate::instances::{repo_name, select_instance};
use crate::offers::offer_dicts;
use crate::picker::pick;
use crate::process::{run, Cmd, RunError};
use crate::prompt::confirm;
use crate::pyfmt::{general, py_str, truthy};
use crate::pyjson::{self, Dict, Value};
use crate::redact::{redact, redact_text};
use crate::ssh_config::{remove_host, update_host, HostError};
use crate::vast::{find_instance, list_instances};

pub fn ls(ctx: &Ctx, json: bool) -> Result<()> {
    let raw = run(ctx, &["vastai", "show", "instances", "--raw"], None)?;
    let value = crate::vast::parse_reply(&raw)?;
    if json {
        ctx.println(&pyjson::dumps(&redact(&value), Some(1)))
    } else {
        render_instances(ctx, &value)
    }
}

pub fn render_instances(ctx: &Ctx, instances: &Value) -> Result<()> {
    if !truthy(instances) {
        return ctx.println("No instances. Launch one: gpu up");
    }
    crate::instances::render_table(ctx, &offer_dicts(instances.clone())?, false)
}

/// The given ID, or one picked from the live instance list (`None` when
/// there is nothing to pick).
pub fn selected_id(ctx: &Ctx, id: Option<&str>, command: &str) -> Result<Option<String>> {
    match id {
        Some(id) => Ok(Some(id.to_string())),
        None if !ctx.interactive() => Err(crate::output::usage(format!(
            "Missing instance ID. Next: gpu {command} ID"
        ))),
        None => select_instance(ctx, &list_instances(ctx, None)?, command),
    }
}

pub fn last_editor(env: &crate::ctx::Env) -> Option<String> {
    recent_picks(env, "editor")
        .into_iter()
        .find(|e| EDITORS.iter().any(|(key, _)| key == e))
}

/// `vastai show instances --raw`, split so callers can tell a CLI failure
/// (Python's RuntimeError/OSError) from a malformed reply.
fn poll_instances(ctx: &Ctx, timeout: f64) -> std::result::Result<Vec<Dict>, (bool, Error)> {
    let raw = run(
        ctx,
        &["vastai", "show", "instances", "--raw"],
        Some(timeout),
    )
    .map_err(|e| (true, e))?;
    let value = crate::vast::parse_reply(&raw).map_err(|e| (false, e))?;
    offer_dicts(value).map_err(|e| (false, e))
}

/// Verify provider state, not just acceptance of a lifecycle request.
pub fn wait_for_lifecycle(ctx: &Ctx, instance_id: &str, action: &str, timeout: f64) -> Result<()> {
    let deadline = ctx.clock.monotonic() + timeout;
    loop {
        let remaining = deadline - ctx.clock.monotonic();
        if remaining <= 0.0 {
            let detail = if action == "destroy" {
                "still listed"
            } else {
                "stopped status not observed"
            };
            return Err(Error::msg(format!(
                "Instance {instance_id}: {action} not verified within {}s ({detail})",
                general(timeout)
            )));
        }
        let live = match poll_instances(ctx, remaining) {
            Ok(live) => live,
            Err((true, err)) => {
                return Err(Error::msg(format!(
                    "Instance {instance_id}: {action} could not be verified: {}",
                    err.text()
                )))
            }
            Err((false, err)) => return Err(err),
        };
        let instance = find_instance(&live, instance_id);
        if action == "destroy" && instance.is_none() {
            return Ok(());
        }
        if action == "stop"
            && instance.is_some_and(|i| i.get("actual_status") == Some(&Value::str("stopped")))
        {
            return Ok(());
        }
        let remaining = deadline - ctx.clock.monotonic();
        if remaining > 0.0 {
            ctx.clock.sleep(remaining.min(2.0));
        }
    }
}

/// Poll Vast and SSH until ready; shared by all editor launchers and expiry.
pub fn wait_for_ssh(ctx: &Ctx, instance_id: &str, timeout: f64) -> Result<(String, String)> {
    let deadline = ctx.clock.monotonic() + timeout;
    let mut reason = "instance not running".to_string();
    let timed_out = |reason: &str| {
        Error::status(crate::error::ExitCode::WaitTimeout, format!(
            "Timed out waiting for instance {instance_id} to run and answer SSH after {}s ({reason}). Next: gpu wait {instance_id} --timeout 120", general(timeout)
        ))
    };
    ctx.progress(&format!(
        "Waiting for instance {instance_id} and SSH (up to {}s)...",
        general(timeout)
    ))?;
    loop {
        let remaining = deadline - ctx.clock.monotonic();
        if remaining <= 0.0 {
            return Err(timed_out(&reason));
        }
        let live = match poll_instances(ctx, remaining) {
            Ok(live) => live,
            Err((true, err)) if err.text().contains("Timed out") => {
                return Err(timed_out("provider inventory request timed out"))
            }
            Err((_, err)) => return Err(err),
        };
        let instance =
            find_instance(&live, instance_id).ok_or_else(|| Error::not_found(instance_id))?;
        if let Some(Value::Str(state)) = instance.get("actual_status") {
            reason = format!("provider status: {}", redact_text(state));
            let stopping = matches!(
                instance.get("intended_status"),
                Some(Value::Str(intended)) if intended != "running"
            );
            if state == "destroyed" || (stopping && ["stopped", "exited"].contains(&state.as_str()))
            {
                return Err(Error::msg(format!(
                    "Instance {instance_id} is {state}; SSH cannot become ready. Next: gpu ls"
                )));
            }
        }
        if instance.get("actual_status") == Some(&Value::str("running")) {
            match update_host(&ctx.env, instance) {
                // SSH endpoint not provisioned yet.
                Err(HostError::NotReady(_)) => {
                    reason = "SSH endpoint not provisioned".into();
                }
                Err(HostError::Failed(err)) => return Err(err),
                Ok(alias) => {
                    let remaining = deadline - ctx.clock.monotonic();
                    if remaining > 0.0 {
                        let probe = Cmd::new(&[
                            "ssh",
                            "-o",
                            "BatchMode=yes",
                            "-o",
                            "ConnectTimeout=5",
                            &alias,
                            "true",
                        ])
                        .timeout(Some(remaining.min(6.0)));
                        match ctx.run(&probe) {
                            Ok(output) if output.code == 0 => {
                                let repo = repo_name(instance);
                                let directory = match &repo {
                                    Some(repo) => format!("/workspace/{repo}"),
                                    None => "/workspace".into(),
                                };
                                if repo.is_none() {
                                    ctx.progress(
                                        "No repository recorded on instance; opening /workspace.",
                                    )?;
                                }
                                return Ok((alias, directory));
                            }
                            Ok(output) => {
                                reason = format!("SSH probe exited with status {}", output.code);
                            }
                            Err(RunError::TimedOut) => {
                                reason = "SSH probe timed out".into();
                            }
                            Err(other) => return Err(Error::msg(other.python_text("ssh"))),
                        }
                    }
                }
            }
        }
        let remaining = deadline - ctx.clock.monotonic();
        if remaining <= 0.0 {
            return Err(timed_out(&reason));
        }
        ctx.clock.sleep(remaining.min(3.0));
    }
}

pub fn edit_instance(
    ctx: &Ctx,
    instance_id: &str,
    editor: Option<&str>,
    nvim_config: Option<&Value>,
    timeout: f64,
) -> Result<()> {
    let editor = match editor.map(String::from).or_else(|| last_editor(&ctx.env)) {
        Some(editor) => editor,
        None => {
            if !ctx.interactive() {
                return Err(crate::output::usage(
                    format!("No previous editor; specify --editor cursor|code|zed|nvim. Example: gpu edit {instance_id} --editor code"),
                ));
            }
            pick(ctx, editor_screen())?
        }
    };
    check_editor(&ctx.env, &editor)?;
    let (alias, directory) = wait_for_ssh(ctx, instance_id, timeout)?;
    ctx.result("ssh_alias", Value::str(&alias));
    ctx.result("directory", Value::str(&directory));
    ctx.result("editor", Value::str(&editor));
    let nvim_config = if editor == "nvim" {
        crate::config::nvim_config_text(nvim_config)?
    } else {
        None
    };
    launch_editor(ctx, &editor, &alias, &directory, nvim_config.as_deref())?;
    record_picks(&ctx.env, &[("editor", &editor)])?;
    ctx.println(&format!(
        "Opened {} at {directory} on {alias}.",
        editor_label(&editor).unwrap_or(&editor)
    ))
}

pub fn expire_instance(
    ctx: &Ctx,
    instance_id: &str,
    action: &str,
    deadline: Option<Timestamp>,
) -> Result<()> {
    let (alias, _) = wait_for_ssh(ctx, instance_id, ctx.ssh_timeout())?;
    let result = manage_expiry(ctx, &alias, instance_id, action, deadline)?;
    let deadlines = match result.get("deadline") {
        Some(Value::Int(seconds)) => Timestamp::from_second(*seconds)
            .ok()
            .map(|at| vec![Value::str(at.to_string())])
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    ctx.result(
        "expiry",
        Value::Dict(Dict::from_iter([
            (
                "status".into(),
                result.get("status").cloned().unwrap_or(Value::Null),
            ),
            ("deadlines".into(), Value::List(deadlines)),
        ])),
    );
    ctx.println(&expiry_summary(ctx, instance_id, &result)?)?;
    if result.get("status") == Some(&Value::str("inactive")) {
        return Err(Error::msg(format!(
            "Timer is NOT running; re-arm with gpu expire {instance_id} 2h."
        )));
    }
    Ok(())
}

pub fn ssh(ctx: &Ctx, instance_id: &str, exec: bool) -> Result<()> {
    let url = run(ctx, &["vastai", "ssh-url", instance_id], None)?;
    if exec {
        let words = shlex::split(&url).ok_or_else(|| Error::msg("No closing quotation"))?;
        let mut argv = vec!["ssh".to_string()];
        argv.extend(words);
        return Err(Error::msg(ctx.exec(&argv)));
    }
    ctx.result("ssh_command", Value::str(redact_text(&url)));
    ctx.println(&redact_text(&url))
}

/// `stop` keeps the instance; `down` destroys it after a typed confirmation.
pub fn lifecycle(ctx: &Ctx, instance_id: &str, destroy: bool, yes: bool) -> Result<()> {
    // Prove the requested ID belongs to this account before authorizing a
    // mutation. Disappearance only verifies a destroy after this preflight.
    let live = list_instances(ctx, Some(60.0))?;
    let instance =
        find_instance(&live, instance_id).ok_or_else(|| Error::not_found(instance_id))?;
    let gpu = instance
        .get("gpu_name")
        .filter(|name| truthy(name))
        .map(py_str)
        .unwrap_or_else(|| "unknown GPU".into());
    if destroy
        && !yes
        && !confirm(
            ctx,
            &format!("Destroy instance {instance_id}? Type 'yes' to confirm: "),
        )?
    {
        return ctx.println("Cancelled.");
    }
    let action = if destroy { "destroy" } else { "stop" };
    let mut command = vec!["vastai", action, "instance", instance_id];
    if destroy {
        command.push("--yes");
    }
    if let Err(err) = run(ctx, &command, Some(60.0)) {
        let message = format!(
            "Instance {instance_id}: {action} failed; completion not verified: {}",
            err.text()
        );
        return Err(match err {
            Error::Status(code, _) => Error::status(code, message),
            _ => Error::msg(message),
        });
    }
    ctx.progress(&format!(
        "Waiting for instance {instance_id} to {action}..."
    ))?;
    wait_for_lifecycle(ctx, instance_id, action, 60.0)?;
    if destroy {
        remove_host(&ctx.env, instance_id)?;
        ctx.result("state", Value::str("destroyed"));
        ctx.println(&format!(
            "Instance {instance_id} ({gpu}) destroyed; billing stopped."
        ))
    } else {
        ctx.result("state", Value::str("stopped"));
        ctx.println(&format!(
            "Instance {instance_id} ({gpu}) stopped; storage charges may continue."
        ))
    }
}
