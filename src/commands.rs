//! `ls`, `ssh`, `stop`, `down`, `edit`, `cursor` and `expire`, plus the
//! polling shared by them.

use jiff::Timestamp;

use crate::ctx::Ctx;
use crate::editors::{check_editor, editor_label, editor_screen, launch_editor, EDITORS};
use crate::error::{Error, Result};
use crate::expiry::{expiry_summary, manage_expiry};
use crate::history::{recent_picks, record_picks};
use crate::instances::{format_uptime, repo_name, select_instance};
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
    let value = pyjson::loads(&raw)?;
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
    let items = offer_dicts(instances.clone())?;
    let columns = [
        ("ID", "id"),
        ("Label", "label"),
        ("GPU", "gpu_name"),
        ("Status", "actual_status"),
        ("$/hr", "dph_total"),
        ("Uptime", "duration"),
    ];
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|item| {
            columns
                .iter()
                .map(|(_, key)| match *key {
                    "duration" => format_uptime(item.get(key)),
                    _ => item.get(key).map(py_str).unwrap_or_else(|| "-".into()),
                })
                .collect()
        })
        .collect();
    let widths: Vec<usize> = columns
        .iter()
        .enumerate()
        .map(|(i, (title, _))| {
            rows.iter()
                .map(|r| r[i].chars().count())
                .chain([title.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<String>| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<w$}", w = widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
    };
    ctx.println(&line(columns.iter().map(|(t, _)| t.to_string()).collect()))?;
    ctx.println(&line(widths.iter().map(|w| "-".repeat(*w)).collect()))?;
    for row in rows {
        ctx.println(&line(row))?;
    }
    Ok(())
}

/// The given ID, or one picked from the live instance list (`None` when
/// there is nothing to pick).
pub fn selected_id(ctx: &Ctx, id: Option<&str>, command: &str) -> Result<Option<String>> {
    match id {
        Some(id) => Ok(Some(id.to_string())),
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
    let value = pyjson::loads(&raw).map_err(|e| (false, Error::Msg(e)))?;
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
                "Instance {instance_id}: {action} not verified after {} seconds ({detail}). Check: gpu ls",
                general(timeout)
            )));
        }
        let live = match poll_instances(ctx, remaining) {
            Ok(live) => live,
            Err((true, err)) => {
                return Err(Error::msg(format!(
                    "Instance {instance_id}: {action} not verified: {}. Check: gpu ls",
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
    let timed_out = || {
        Error::msg(format!(
            "Instance {instance_id}: SSH not ready after {} seconds. Check: gpu ls",
            general(timeout)
        ))
    };
    ctx.eprintln(&format!(
        "Instance {instance_id}: waiting for running state and SSH, up to {} seconds.",
        general(timeout)
    ))?;
    loop {
        let remaining = deadline - ctx.clock.monotonic();
        if remaining <= 0.0 {
            return Err(timed_out());
        }
        let live = match poll_instances(ctx, remaining) {
            Ok(live) => live,
            Err((true, err)) if err.text().contains("Timed out") => return Err(timed_out()),
            Err((_, err)) => return Err(err),
        };
        let instance = find_instance(&live, instance_id).ok_or_else(|| {
            Error::msg(format!("Instance {instance_id} not found. Check: gpu ls"))
        })?;
        if instance.get("actual_status") == Some(&Value::str("running")) {
            match update_host(&ctx.env, instance) {
                // SSH endpoint not provisioned yet.
                Err(HostError::NotReady(_)) => {}
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
                        match ctx.runner.run(&probe) {
                            Ok(output) if output.code == 0 => {
                                let repo = repo_name(instance);
                                let directory = match &repo {
                                    Some(repo) => format!("/workspace/{repo}"),
                                    None => "/workspace".into(),
                                };
                                if repo.is_none() {
                                    ctx.println("Workspace: /workspace (no repository recorded).")?;
                                }
                                return Ok((alias, directory));
                            }
                            Ok(_) | Err(RunError::TimedOut) => {}
                            Err(other) => return Err(Error::msg(other.python_text("ssh"))),
                        }
                    }
                }
            }
        }
        let remaining = deadline - ctx.clock.monotonic();
        if remaining <= 0.0 {
            return Err(timed_out());
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
                return Err(Error::msg(
                    format!("No saved editor (cursor, code, zed, nvim). Run: gpu edit {instance_id} --editor code"),
                ));
            }
            pick(ctx, editor_screen())?
        }
    };
    check_editor(&ctx.env, &editor)?;
    let (alias, directory) = wait_for_ssh(ctx, instance_id, timeout)?;
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
    let (alias, _) = wait_for_ssh(ctx, instance_id, 120.0)?;
    let result = manage_expiry(ctx, &alias, instance_id, action, deadline)?;
    ctx.println(&expiry_summary(ctx, instance_id, &result)?)?;
    if result.get("status") == Some(&Value::str("inactive")) {
        return Err(Error::msg(format!(
            "Instance {instance_id}: expiry worker is inactive. Re-arm: gpu expire {instance_id} 2h"
        )));
    }
    Ok(())
}

pub fn ssh(ctx: &Ctx, instance_id: &str, exec: bool) -> Result<()> {
    let url = run(ctx, &["vastai", "ssh-url", instance_id], None)?;
    if exec {
        let words = shlex::split(&url).ok_or_else(|| {
            Error::msg(format!(
                "Vast returned an SSH URL with an unclosed quote. Retry: gpu ssh {instance_id}"
            ))
        })?;
        let mut argv = vec!["ssh".to_string()];
        argv.extend(words);
        return Err(Error::msg(ctx.runner.exec(&argv)));
    }
    ctx.println(&redact_text(&url))
}

/// `stop` keeps the instance; `down` destroys it after a typed confirmation.
pub fn lifecycle(ctx: &Ctx, instance_id: &str, destroy: bool, yes: bool) -> Result<()> {
    if destroy
        && !yes
        && !confirm(
            ctx,
            &format!("Delete instance {instance_id} and its disk? Type 'yes' to destroy: "),
        )?
    {
        return ctx.println("Cancelled.");
    }
    let live = list_instances(ctx, Some(60.0))?;
    let gpu = match find_instance(&live, instance_id) {
        Some(instance) => match instance.get("gpu_name") {
            Some(name) if truthy(name) => py_str(name),
            _ => "unknown GPU".into(),
        },
        None => "unknown GPU".into(),
    };
    let action = if destroy { "destroy" } else { "stop" };
    let mut command = vec!["vastai", action, "instance", instance_id];
    if destroy {
        command.push("--yes");
    }
    if let Err(err) = run(ctx, &command, Some(60.0)) {
        return Err(Error::msg(format!(
            "Instance {instance_id}: {action} failed; completion unverified: {}. Check: gpu ls",
            err.text()
        )));
    }
    ctx.eprintln(&format!(
        "Instance {instance_id}: verifying {action}, up to 60 seconds."
    ))?;
    wait_for_lifecycle(ctx, instance_id, action, 60.0)?;
    if destroy {
        remove_host(&ctx.env, instance_id)?;
        ctx.println(&format!(
            "Instance {instance_id} ({gpu}) destroyed; billing stopped."
        ))
    } else {
        ctx.println(&format!(
            "Instance {instance_id} ({gpu}) stopped; storage charges may continue."
        ))
    }
}
