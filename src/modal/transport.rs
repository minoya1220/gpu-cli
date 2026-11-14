//! Public exec/copy/terminal operations. No SSH daemon or local SSH config.
use super::{sandbox_id, status, volume_name, Helper};
use crate::ctx::Ctx;
use crate::error::{Error, ExitCode, Result};
use crate::process::{Cmd, Input, RunError};
use crate::pyjson::{self, Value};

fn ready(ctx: &Ctx, id: &str, timeout: f64) -> Result<Helper> {
    let raw = sandbox_id(id)?;
    ctx.result("id", Value::str(id));
    ctx.result("ssh_alias", Value::Null);
    ctx.result("setup_done", Value::Null);
    ctx.result("ready", Value::Bool(false));
    let helper = Helper::new()?;
    let mut cmd = helper.command("status").timeout(Some(timeout.max(0.1)));
    cmd.argv.push(raw.into());
    let (reply, _) = helper.call_command(ctx, &cmd, false)?;
    if reply.get("running") != Some(&Value::Bool(true)) {
        return Err(
            Error::status(ExitCode::InstanceLost, "Modal sandbox is no longer running")
                .next("gpu ls --provider modal"),
        );
    }
    ctx.result("ready", Value::Bool(true));
    Ok(helper)
}

pub(super) fn exec_error(output: &crate::process::Output) -> Option<Error> {
    if output.code != 255 {
        return None;
    }
    let reply = pyjson::loads(&output.stderr).ok()?;
    let error = reply.get("error")?;
    let kind = error.get("kind")?.as_str()?;
    // Only helper-owned diagnostics reach this path. Callers redact as well.
    Some(crate::provider_errors::modal(
        kind,
        error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Modal exec failed."),
    ))
}

fn completed(ctx: &Ctx, cmd: Cmd) -> Result<()> {
    let output = ctx.run(&cmd.capture(ctx.json()).raw_output().timeout(ctx.command_timeout()))
        .map_err(|e| match e {
            RunError::NotFound => Error::status(ExitCode::NotSetup, "Install uv").next("gpu setup --provider modal"),
            RunError::TimedOut => Error::status(ExitCode::CommandTimeout, "Remote command/transfer timed out; the remote command may still be running. Check: gpu ls --provider modal"),
            other => Error::msg(ctx.safe_diagnostic(&other.describe("uv"))),
        })?;
    let code = if output.code < 0 {
        128 - output.code
    } else {
        output.code
    };
    ctx.result("exit_code", Value::Int(code.into()));
    let mapped = exec_error(&output);
    if ctx.json() {
        ctx.result("stdout", Value::str(&output.stdout));
        ctx.result(
            "stderr",
            Value::str(if mapped.is_some() {
                ctx.safe_diagnostic(&output.stderr)
            } else {
                output.stderr.clone()
            }),
        );
    }
    if let Some(error) = mapped {
        let text = ctx.safe_diagnostic(error.text());
        return Err(error.with_text(text));
    }
    if code == 0 {
        Ok(())
    } else {
        Err(Error::Remote(code))
    }
}

pub fn exec(ctx: &Ctx, id: &str, command: &[String], timeout: f64) -> Result<()> {
    let helper = ready(ctx, id, timeout)?;
    let mut cmd = helper.command("exec");
    cmd.argv.extend([sandbox_id(id)?.into(), "--".into()]);
    cmd.argv.extend_from_slice(command);
    completed(ctx, cmd)
}

pub fn copy(
    ctx: &Ctx,
    id: &str,
    source: &str,
    destination: &str,
    recursive: bool,
    timeout: f64,
) -> Result<()> {
    crate::remote::validate_copy(source, destination)?;
    ctx.result("source", Value::str(source));
    ctx.result("destination", Value::str(destination));
    if id.starts_with("modal:volume:") {
        let name = volume_name(id)?;
        if source != ":/" || !recursive {
            return Err(Error::usage("Recover a leftover Volume with gpu cp modal:volume:NAME :/ ./destination --recursive"));
        }
        let helper = Helper::new()?;
        let mut cmd = helper.command("volume-copy");
        cmd.argv.extend([name.into(), destination.into()]);
        return completed(ctx, cmd);
    }
    let helper = ready(ctx, id, timeout)?;
    let mut cmd = helper.command("exec");
    cmd.argv.extend([
        sandbox_id(id)?.into(),
        "--copy".into(),
        if recursive { "recursive" } else { "file" }.into(),
        source.into(),
        destination.into(),
    ]);
    completed(ctx, cmd)
}

pub fn ssh(ctx: &Ctx, id: &str, connect: bool) -> Result<()> {
    sandbox_id(id)?;
    ctx.result("id", Value::str(id));
    if !connect {
        // A public command remains usable after this helper's temp file is gone.
        status(ctx, id)?;
        let command = format!("gpu ssh {id} --exec");
        ctx.result("ssh_command", Value::str(&command));
        return ctx.println(&command);
    }
    let helper = ready(ctx, id, 60.0)?;
    let mut cmd = helper.command("exec");
    cmd.argv.extend([
        sandbox_id(id)?.into(),
        "--pty".into(),
        "--".into(),
        "bash".into(),
        "-l".into(),
    ]);
    completed(ctx, cmd.stdin(Input::Inherit))
}
