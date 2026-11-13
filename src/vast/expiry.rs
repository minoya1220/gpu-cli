//! The Vast expiry transport: the remote worker and boot guard, sent over
//! SSH stdin; no account credentials cross SSH.
use jiff::Timestamp;

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::expiry::{deadline_seconds, local_time};
use crate::process::{Cmd, Input, RunError};
use crate::pyfmt::{is_digits, py_float, shell_join, shell_quote};
use crate::pyjson::{self, Dict, Value};

/// The remote controller, sent over SSH stdin and run with the instance's bash.
pub const REMOTE_SOURCE: &str = include_str!("../../remote/expiry_remote.sh");

/// The boot-time guard the onstart script starts (`crate::vast::startup::expiry_guard`).
pub const GUARD_SOURCE: &str = include_str!("../../remote/expiry_guard.sh");

/// The `ssh` argv; the remote command line holds only validated words.
pub fn remote_command(
    alias: &str,
    instance_id: &str,
    action: &str,
    deadline: Option<Timestamp>,
) -> Result<Vec<String>> {
    if !is_digits(instance_id) || alias != format!("vast-{instance_id}") {
        return Err(Error::msg("Invalid managed instance SSH alias or ID."));
    }
    if !matches!(action, "arm" | "show" | "cancel") {
        return Err(Error::msg("Invalid expiry action."));
    }
    let mut args = vec![
        "bash".to_string(),
        "-s".into(),
        "--".into(),
        action.into(),
        instance_id.into(),
    ];
    if action == "arm" {
        let deadline = deadline.ok_or_else(|| Error::msg("Arming requires a deadline."))?;
        args.push(deadline_seconds(deadline).to_string());
    }
    Ok(crate::ssh::command(&["ssh"], false, alias, &shell_join(&args)).argv)
}

/// The script sent on stdin: the source as a variable, then the source.
pub fn transport_payload() -> String {
    format!(
        "GPU_EXPIRE_SOURCE={}\n{REMOTE_SOURCE}",
        shell_quote(REMOTE_SOURCE)
    )
}

pub fn manage_expiry(
    ctx: &Ctx,
    alias: &str,
    instance_id: &str,
    action: &str,
    deadline: Option<Timestamp>,
) -> Result<Dict> {
    let command = remote_command(alias, instance_id, action, deadline)?;
    let cmd = Cmd::new(&command)
        .stdin(Input::Text(transport_payload()))
        .timeout(Some(45.0));
    let result = match ctx.run(&cmd) {
        Ok(result) => result,
        Err(RunError::TimedOut) => {
            let next = format!("gpu expire {instance_id} show");
            return Err(Error::msg(format!(
                "Expiry SSH timed out; arming/cancellation is unverified. Run {next}."
            ))
            .recovers_with(next));
        }
        Err(other) => return Err(Error::msg(other.describe("ssh"))),
    };
    // Never echo SSH stderr or arbitrary remote text: it may contain secrets.
    let reply = pyjson::loads(&result.stdout).unwrap_or(Value::Dict(Dict::new()));
    let show = format!("gpu expire {instance_id} show");
    let message = |code: &str| {
        match code {
        "credentials" => Error::msg(
            "Instance-scoped CONTAINER_ID/CONTAINER_API_KEY missing or mismatched; expiry NOT armed.",
        ),
        "past" => {
            let rearm = format!("gpu expire {instance_id} 2h");
            Error::msg(format!(
                "Deadline passed before SSH was ready; expiry NOT armed. Run {rearm}."
            ))
            .recovers_with(rearm)
        }
        "verification" => Error::msg(format!(
            "Timer startup verification failed; new expiry NOT armed (previous deadline unchanged). Run {show}."
        ))
        .recovers_with(&show),
        _ => Error::msg(format!(
            "Remote expiry operation failed; check bash, curl, flock and writable ~/.local/state/gpu-expire; run {show}."
        ))
        .recovers_with(&show),
    }
    };
    let error = reply.get("error").filter(|e| crate::pyfmt::truthy(e));
    let Value::Dict(reply) = reply.clone() else {
        return Err(message("remote"));
    };
    if result.code != 0 || error.is_some() {
        let code = error.and_then(Value::as_str).unwrap_or("remote");
        return Err(message(code));
    }
    let status = reply.get("status").and_then(Value::as_str).unwrap_or("");
    if !matches!(
        status,
        "armed" | "retrying" | "inactive" | "none" | "cancelled" | "destroy-request-accepted"
    ) {
        return Err(Error::msg(format!(
            "Invalid expiry response; completion unverified. Run {show}."
        ))
        .recovers_with(show));
    }
    if action == "arm" && status != "armed" {
        return Err(message("verification"));
    }
    Ok(reply)
}

pub fn expiry_summary(ctx: &Ctx, instance_id: &str, reply: &Dict) -> Result<String> {
    let status = crate::pyfmt::py_str(reply.get("status").ok_or_else(|| Error::key("status"))?);
    if status == "none" || status == "cancelled" {
        let suffix = if status == "cancelled" {
            " (cancelled)."
        } else {
            "."
        };
        return Ok(format!(
            "Instance {instance_id}: no auto-destroy deadline{suffix}"
        ));
    }
    let deadline = reply
        .get("deadline")
        .ok_or_else(|| Error::key("deadline"))?;
    let seconds = py_float(deadline).map_err(|_| {
        let show = format!("gpu expire {instance_id} show");
        Error::msg(format!(
            "Expiry deadline must be a number, got {}. Run {show}.",
            deadline.kind()
        ))
        .recovers_with(show)
    })?;
    let micros = (seconds * 1e6).round_ties_even();
    let deadline = Timestamp::from_microsecond(micros as i64)
        .map_err(|_| Error::msg("year is out of range"))?;
    let text = local_time(ctx, deadline);
    let remaining = deadline.as_microsecond() - ctx.clock.now().as_microsecond();
    let total = if remaining <= 0 {
        0
    } else {
        (remaining + 999_999) / 1_000_000
    };
    let (hours, rest) = (total / 3600, total % 3600);
    let (minutes, secs) = (rest / 60, rest % 60);
    Ok(format!(
        "Instance {instance_id}: auto-DESTROY at {text} (in {hours}h {minutes}m {secs}s); timer {status}. Disk \
            will be deleted; all billing stops on destruction."
    ))
}
