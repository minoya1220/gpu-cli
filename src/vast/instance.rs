//! Vast instance commands: SSH readiness, `ssh`, `stop`, `down` and
//! `expire`, plus the polling shared by them.

use jiff::Timestamp;

use crate::ctx::Ctx;
use crate::error::{Error, ExitCode, Result};
use crate::instances::repo_name;
use crate::process::RunError;
use crate::prompt::confirm;
use crate::pyfmt::{general, py_str, truthy};
use crate::pyjson::{Dict, Value};
use crate::redact::redact_text;
use crate::vast::expiry::{expiry_summary, manage_expiry};
use crate::vast::run;
use crate::vast::ssh_config::{has_host, remove_host, update_host, HostError};
use crate::vast::{find_instance, list_instances};

/// Bounded provider read with typed retryability.
fn poll_instances(ctx: &Ctx, timeout: f64) -> Result<Vec<Dict>> {
    list_instances(ctx, Some(timeout.min(15.0)))
}

/// Verify provider state, not just acceptance of a lifecycle request.
pub fn wait_for_lifecycle(ctx: &Ctx, instance_id: &str, action: &str, timeout: f64) -> Result<()> {
    let poll = ctx.polling(timeout, 2.0);
    loop {
        let remaining = poll.remaining();
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
            Err(err) if err.retryable() => {
                poll.pause();
                continue;
            }
            Err(err) => return Err(err),
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
        let remaining = poll.remaining();
        if remaining > 0.0 {
            poll.pause();
        }
    }
}

/// Poll Vast and SSH until ready; shared by all editor launchers and expiry.
pub fn wait_for_ssh(ctx: &Ctx, instance_id: &str, timeout: f64) -> Result<(String, String)> {
    wait_for_ssh_with_hint(
        ctx,
        instance_id,
        timeout,
        &format!("Next: gpu wait {instance_id} --timeout 120"),
    )
}

/// Callers that own a temporary instance must not suggest reconnecting after
/// their cleanup has destroyed it. Keep the readiness diagnostic shared.
pub(crate) fn wait_for_ssh_with_hint(
    ctx: &Ctx,
    instance_id: &str,
    timeout: f64,
    hint: &str,
) -> Result<(String, String)> {
    let poll = ctx.polling(timeout, 3.0);
    let mut reason = "instance not running".to_string();
    let timed_out = |reason: &str| {
        Error::status(crate::error::ExitCode::WaitTimeout, format!(
            "Timed out waiting for instance {instance_id} to run and answer SSH after {}s ({reason}). {hint}", general(timeout)
        ))
    };
    ctx.progress(&format!(
        "Waiting for instance {instance_id} and SSH (up to {}s)...",
        general(timeout)
    ))?;
    loop {
        poll.check_cancelled()?;
        let remaining = poll.remaining();
        if remaining <= 0.0 {
            return Err(timed_out(&reason));
        }
        let live = match poll_instances(ctx, remaining) {
            Ok(live) => live,
            Err(err) if err.retryable() => {
                reason = err.text().into();
                poll.pause();
                continue;
            }
            Err(err) => return Err(err),
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
                    "Instance {instance_id} is {state}; SSH cannot become ready"
                ))
                .next("gpu ls"));
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
                    let remaining = poll.remaining();
                    if remaining > 0.0 {
                        let probe = crate::ssh::command(&["ssh"], false, &alias, "true")
                            .timeout(Some(remaining.min(11.0)));
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
                            Err(other) => return Err(Error::msg(other.describe("ssh"))),
                        }
                    }
                }
            }
        }
        let remaining = poll.remaining();
        if remaining <= 0.0 {
            return Err(timed_out(&reason));
        }
        poll.pause();
    }
}

pub fn expire_instance(
    ctx: &Ctx,
    instance_id: &str,
    action: &str,
    deadline: Option<Timestamp>,
) -> Result<String> {
    let (alias, _) = wait_for_ssh(ctx, instance_id, ctx.ssh_timeout())?;
    let result = manage_expiry(ctx, &alias, instance_id, action, deadline)?;
    let deadlines = match result.get("deadline") {
        Some(Value::Int(seconds)) => Timestamp::from_second(*seconds)
            .ok()
            .map(|at| vec![Value::str(at.to_string())])
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    crate::expiry::report(
        ctx,
        result.get("status").cloned().unwrap_or(Value::Null),
        deadlines,
    );
    ctx.println(&expiry_summary(ctx, instance_id, &result)?)?;
    if result.get("status") == Some(&Value::str("inactive")) {
        let rearm = format!("gpu expire {instance_id} 2h");
        return Err(
            Error::msg(format!("Timer is NOT running; re-arm with {rearm}.")).recovers_with(rearm),
        );
    }
    Ok(alias)
}

pub fn ssh(ctx: &Ctx, instance_id: &str, exec: bool) -> Result<()> {
    let url = run(ctx, &["vastai", "ssh-url", instance_id], None)?;
    if exec {
        let words = shlex::split(&url).ok_or_else(|| Error::msg("No closing quotation"))?;
        let mut argv = vec!["ssh".to_string()];
        argv.extend(crate::ssh::connect_options());
        argv.extend(words);
        return Err(Error::msg(ctx.exec(&argv)));
    }
    ctx.result("ssh_command", Value::str(redact_text(&url)));
    ctx.println(&redact_text(&url))
}

/// Ask Vast to `destroy` or `stop` an instance; completion is not verified.
fn request(ctx: &Ctx, instance_id: &str, action: &str, timeout: f64) -> Result<String> {
    let mut command = vec!["vastai", action, "instance", instance_id];
    if action == "destroy" {
        command.push("--yes");
    }
    run(ctx, &command, Some(timeout))
}

const DESTROY_ATTEMPTS: usize = 3;
const DESTROY_VERIFY_SECONDS: f64 = 60.0;

/// Only ever called with the ID this invocation just created. A successful
/// mutation is not enough: verify disappearance. Always remove the local alias.
pub(crate) fn destroy_created(ctx: &Ctx, id: &str) -> Result<()> {
    let cleanup = || -> Result<()> {
        // This invocation owns the freshly created ID. A lagging or broken
        // inventory must never suppress the paid-resource cleanup request.
        let mut verified = Ok(());
        for _ in 0..DESTROY_ATTEMPTS {
            let sent = request(ctx, id, "destroy", 30.0);
            if let Err(error) = sent {
                // A failed/timeout reply does not prove the mutation was rejected.
                // A definite absence after the request still verifies completion.
                let _ = ctx.eprintln(&format!(
                    "Destroy request completion uncertain: {}",
                    ctx.safe_diagnostic(error.text())
                ));
            }
            verified = wait_for_lifecycle(
                ctx,
                id,
                "destroy",
                DESTROY_VERIFY_SECONDS / DESTROY_ATTEMPTS as f64,
            );
            if verified.is_ok() {
                break;
            }
        }
        verified
    };
    let outcome = cleanup();
    let _ = remove_host(&ctx.env, id);
    outcome.map_err(|_| {
        Error::status(
            ExitCode::CleanupUnverified,
            format!("Instance {id}: destruction unverified; billing may continue"),
        )
        .next(format!("gpu down {id} --yes"))
    })
}

/// `stop` keeps the instance; `down` destroys it after a typed confirmation.
pub fn lifecycle(ctx: &Ctx, instance_id: &str, destroy: bool, yes: bool) -> Result<()> {
    // Prove the requested ID belongs to this account before authorizing a
    // mutation. Disappearance only verifies a destroy after this preflight.
    let live = list_instances(ctx, Some(60.0))?;
    let Some(instance) = find_instance(&live, instance_id) else {
        if destroy && has_host(&ctx.env, instance_id)? {
            remove_host(&ctx.env, instance_id)?;
            ctx.result("state", Value::str("already_absent"));
            return ctx.println(&format!("Instance {instance_id} is already absent."));
        }
        return Err(Error::not_found(instance_id));
    };
    let gpu = instance
        .get("gpu_name")
        .filter(|name| truthy(name))
        .map(py_str)
        .unwrap_or_else(|| "unknown GPU".into());
    if destroy && !yes && !confirm(ctx, Some(&format!("Destroy instance {instance_id}?")))? {
        return ctx.println("Cancelled.");
    }
    let action = if destroy { "destroy" } else { "stop" };
    let billed = destroy
        .then(|| crate::vast::billing::estimate(instance, ctx.clock.now()))
        .flatten();
    if let Err(err) = request(ctx, instance_id, action, 60.0) {
        let message = format!(
            "Instance {instance_id}: {action} failed; completion not verified: {}",
            err.text()
        );
        return Err(err.with_text(message));
    }
    ctx.progress(&format!(
        "Waiting for instance {instance_id} to {action}..."
    ))?;
    wait_for_lifecycle(ctx, instance_id, action, 60.0)?;
    if destroy {
        remove_host(&ctx.env, instance_id)?;
        ctx.result("state", Value::str("destroyed"));
        let Some(billed) = billed else {
            return ctx.println(&format!(
                "Instance {instance_id} ({gpu}) destroyed; billing stopped."
            ));
        };
        crate::vast::billing::record(ctx, &billed);
        ctx.println(&format!(
            "Instance {instance_id} ({gpu}) destroyed{}.",
            crate::vast::billing::describe(&billed)
        ))
    } else {
        ctx.result("state", Value::str("stopped"));
        ctx.println(&format!(
            "Instance {instance_id} ({gpu}) stopped; storage charges may continue."
        ))
    }
}
