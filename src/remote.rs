//! Terminal-free SSH and copy operations using the managed provider aliases.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::process::Cmd;
use crate::provider::InstanceId;
use crate::pyjson::Value;

pub fn wait(ctx: &Ctx, id: &str, timeout: f64) -> Result<String> {
    ctx.result("id", Value::str(id));
    ctx.result("ready", Value::Bool(false));
    let alias = match InstanceId::parse(id)? {
        InstanceId::Vast(id) => crate::commands::wait_for_ssh(ctx, &id, timeout)?.0,
        InstanceId::Gcp(id) => crate::gcp_ssh::ready_alias(ctx, &id, timeout)?,
    };
    ctx.result("ssh_alias", Value::str(&alias));
    ctx.result("ready", Value::Bool(true));
    ctx.progress(&format!("Instance {id} answers SSH ({alias})."))?;
    Ok(alias)
}

pub(crate) fn after_up(ctx: &Ctx, should_wait: bool, timeout: f64) -> Result<()> {
    if should_wait {
        if ctx.result_snapshot().get("ready") == Some(&Value::Bool(true)) {
            return Ok(());
        }
        let id = ctx
            .result_snapshot()
            .get("id")
            .and_then(|v| match v {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .ok_or_else(|| {
                Error::msg("No created instance ID returned; cannot wait. Next: gpu ls")
            })?;
        wait(ctx, &id, timeout)?;
    }
    Ok(())
}

fn completed(ctx: &Ctx, argv: &[String]) -> Result<()> {
    // In plain mode inherit streams to preserve bytes and avoid buffering a
    // large job's output. stdin remains closed; SSH never allocates a TTY.
    let output = ctx
        .run(&Cmd::new(argv).capture(ctx.json()).raw_output())
        .map_err(|e| Error::msg(e.python_text(&argv[0])))?;
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
    let alias = wait(ctx, id, timeout)?;
    // OpenSSH joins command arguments without escaping. Quote each argument so
    // spaces and shell metacharacters remain data; use `sh -c SCRIPT` for shell.
    let command = crate::pyfmt::shell_join(command);
    completed(
        ctx,
        &[
            "ssh".into(),
            "-T".into(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=5".into(),
            // Vast's /etc/banner is an SSH authentication banner, not remote
            // command stderr. OpenSSH suppresses it at LogLevel=ERROR; never
            // strip banner-looking text from the actual command streams.
            "-o".into(),
            "LogLevel=ERROR".into(),
            alias,
            command,
        ],
    )
}

pub fn validate_copy(source: &str, destination: &str) -> Result<()> {
    let remote = |s: &str| s.starts_with(":/");
    if remote(source) == remote(destination)
        || [source, destination].iter().any(|s| {
            s.is_empty() || s.chars().any(char::is_control) || (!remote(s) && s.contains(':'))
        })
    {
        return Err(crate::output::usage("Copy needs exactly one remote path prefixed with :/. Example: gpu cp ID ./file :/workspace/file"));
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
    let mut argv = vec![
        "scp".into(),
        // Force SFTP even on versions whose default was the remote-shell SCP
        // protocol: filenames must never be interpreted as shell programs.
        "-s".into(),
        "-B".into(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=5".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
        "-o".into(),
        "RequestTTY=no".into(),
    ];
    if recursive {
        argv.push("-r".into());
    }
    argv.extend(["--".into(), path(source), path(destination)]);
    ctx.result("source", Value::str(source));
    ctx.result("destination", Value::str(destination));
    completed(ctx, &argv)
}
