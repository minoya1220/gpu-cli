//! Terminal-free SSH and copy operations using the managed provider aliases.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::process::Cmd;
use crate::pyjson::Value;

pub fn wait(ctx: &Ctx, id: &str, timeout: f64) -> Result<String> {
    wait_with_setup(ctx, id, timeout, false)
}

pub fn wait_with_setup(ctx: &Ctx, id: &str, timeout: f64, wait_setup: bool) -> Result<String> {
    let started = ctx.clock.monotonic();
    ctx.result("setup_done", Value::Null);
    ctx.result("id", Value::str(id));
    ctx.result("ready", Value::Bool(false));
    let alias = crate::provider::parse(id)?.wait_ssh(ctx, timeout)?;
    ctx.result("ssh_alias", Value::str(&alias));
    ctx.result("ready", Value::Bool(true));
    ctx.progress(&format!("Instance {id} answers SSH ({alias})."))?;
    if ctx.json() || wait_setup {
        workspace_setup(
            ctx,
            &alias,
            timeout - (ctx.clock.monotonic() - started),
            wait_setup,
        )?;
    }
    Ok(alias)
}

pub(crate) fn after_up(
    ctx: &Ctx,
    launched: crate::launch::Launched,
    should_wait: bool,
    timeout: f64,
    wait_setup: bool,
) -> Result<()> {
    if let Some(alias) = &launched.ready_alias {
        if ctx.json() || should_wait {
            workspace_setup(ctx, alias, timeout, wait_setup)?;
        }
        return Ok(());
    }
    if should_wait {
        let id = launched.id.ok_or_else(|| {
            Error::msg("No created instance ID returned; cannot wait").next("gpu ls")
        })?;
        wait_with_setup(ctx, &id, timeout, wait_setup)?;
    }
    Ok(())
}

/// Old instances have no marker: report unknown, and only --wait-setup blocks.
fn workspace_setup(ctx: &Ctx, alias: &str, timeout: f64, required: bool) -> Result<()> {
    let poll = ctx.polling(timeout.max(0.0), 2.0);
    loop {
        let remaining = poll.remaining();
        if remaining <= 0.0 {
            break;
        }
        let result = ctx.run(
            &crate::ssh::command(
                &["ssh"],
                true,
                alias,
                "cat /workspace/.gpu-setup 2>/dev/null || true",
            )
            .timeout(Some(remaining.min(11.0))),
        );
        if let Ok(out) = result {
            if out.code == 0 {
                match out.stdout.trim() {
                    "done" => {
                        ctx.result("setup_done", Value::Bool(true));
                        ctx.result("setup_ok", Value::Bool(true));
                        return Ok(());
                    }
                    "failed" => {
                        ctx.result("setup_done", Value::Bool(true));
                        ctx.result("setup_ok", Value::Bool(false));
                        if required {
                            return Err(Error::msg("Workspace setup failed (installer or repository clone). Inspect the instance startup log; SSH remains \
                                available."));
                        }
                        return Ok(());
                    }
                    "pending" => {
                        ctx.result("setup_done", Value::Bool(false));
                    }
                    _ => {}
                }
            }
        }
        if !required {
            return Ok(());
        }
        poll.pause();
    }
    if required {
        Err(Error::status(
            crate::error::ExitCode::WaitTimeout,
            "Timed out waiting for workspace setup; instance is still running",
        )
        .next("gpu wait ID --wait-setup --timeout 120"))
    } else {
        Ok(())
    }
}

fn completed(ctx: &Ctx, argv: &[String]) -> Result<()> {
    // In plain mode inherit streams to preserve bytes and avoid buffering a
    // large job's output. stdin remains closed; SSH never allocates a TTY.
    let output = ctx
        .run(&Cmd::new(argv).capture(ctx.json()).raw_output().timeout(ctx.command_timeout()))
        .map_err(|e| match e {
            crate::process::RunError::TimedOut => Error::status(crate::error::ExitCode::CommandTimeout,
                "Remote command/transfer timed out; the remote command may still be running. Check: gpu ls"),
            other => Error::msg(other.describe(&argv[0])),
        })?;
    let code = if output.code < 0 {
        128 - output.code
    } else {
        output.code
    };
    ctx.result("exit_code", Value::Int(code.into()));
    if ctx.json() {
        ctx.result("stdout", Value::str(output.stdout));
        ctx.result("stderr", Value::str(output.stderr));
    }
    if code == 0 {
        Ok(())
    } else {
        Err(Error::Remote(code))
    }
}

pub fn exec(ctx: &Ctx, id: &str, command: &[String], timeout: f64) -> Result<()> {
    if id.starts_with("modal:") {
        return crate::modal::exec(ctx, id, command, timeout);
    }
    let alias = wait(ctx, id, timeout)?;
    // OpenSSH joins command arguments without escaping. Quote each argument so
    // spaces and shell metacharacters remain data; use `sh -c SCRIPT` for shell.
    let command = crate::pyfmt::shell_join(command);
    let mut argv = vec!["ssh".into(), "-T".into()];
    argv.extend(crate::ssh::options(true));
    argv.extend([
        "-o".into(),
        "ControlMaster=no".into(),
        // Vast's /etc/banner is an SSH authentication banner, not remote
        // command stderr. OpenSSH suppresses it at LogLevel=ERROR; never
        // strip banner-looking text from the actual command streams.
        "-o".into(),
        "LogLevel=ERROR".into(),
        alias,
        command,
    ]);
    completed(ctx, &argv)
}

pub fn validate_copy(source: &str, destination: &str) -> Result<()> {
    let remote = |s: &str| s.starts_with(":/");
    if remote(source) == remote(destination)
        || [source, destination].iter().any(|s| {
            s.is_empty() || s.chars().any(char::is_control) || (!remote(s) && s.contains(':'))
        })
    {
        return Err(Error::usage("Copy needs exactly one remote path prefixed with :/. Example: gpu cp ID ./file :/workspace/file"));
    }
    Ok(())
}

pub fn copy(
    ctx: &Ctx,
    id: &str,
    source: &str,
    destination: &str,
    recursive: bool,
    timeout: f64,
) -> Result<()> {
    validate_copy(source, destination)?;
    if id.starts_with("modal:") {
        return crate::modal::copy(ctx, id, source, destination, recursive, timeout);
    }
    let alias = wait(ctx, id, timeout)?;
    let path = |s: &str| {
        if s.starts_with(":/") {
            format!("{alias}{s}")
        } else if s.starts_with('/') {
            s.into()
        } else {
            format!("./{s}")
        }
    };
    let mut argv = vec!["scp".into(), "-s".into(), "-B".into()];
    argv.extend(crate::ssh::options(true));
    argv.extend([
        "-o".into(),
        "ControlMaster=no".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
        "-o".into(),
        "RequestTTY=no".into(),
    ]);
    if recursive {
        argv.push("-r".into());
    }
    argv.extend(["--".into(), path(source), path(destination)]);
    ctx.result("source", Value::str(source));
    ctx.result("destination", Value::str(destination));
    completed(ctx, &argv)
}
