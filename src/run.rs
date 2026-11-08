//! Attached jobs: detached remote execution, resumable logs, download, verified cleanup.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use jiff::Timestamp;

use crate::cli::RunFlags;
use crate::ctx::Ctx;
use crate::error::{Error, ExitCode, Result};
use crate::launch::UpArgs;
use crate::offers::{offer_price, OfferSearch};
use crate::picker::Job;
use crate::process::{Cmd, Input, Output};
use crate::pyfmt::{shell_join, shell_quote};
use crate::pyjson::{self, Dict, Value};
use crate::run_files;
use crate::vast::{self, Vast};

pub const REMOTE_SOURCE: &str = include_str!("../remote/run_remote.sh");
const DOWNLOAD_RESERVE: i64 = 600;
const CHUNK: u64 = 131072;

fn phase(ctx: &Ctx, phase: &str) {
    ctx.result("phase", Value::str(phase));
}

fn checked(ctx: &Ctx, cmd: &Cmd, message: &str) -> Result<Output> {
    match ctx.run(cmd) {
        Ok(out) if out.code == 0 => Ok(out),
        _ => Err(Error::msg(message)), // never leak forwarded credentials or SSH diagnostics
    }
}

fn checked_transfer(ctx: &Ctx, cmd: &Cmd, threshold: usize, message: &str) -> Result<Output> {
    match ctx.transfer(cmd, threshold) {
        Ok(out) if out.code == 0 => Ok(out),
        _ => Err(Error::msg(message)),
    }
}

fn ssh(alias: &str, command: &str, timeout: f64) -> Cmd {
    Cmd::new(&[
        "ssh",
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=4",
        alias,
        command,
    ])
    .timeout(Some(timeout.max(0.1)))
    .raw_output()
}

fn control(root: &str, token: &str, action: &str) -> String {
    format!(
        "bash {} {action} {} {}",
        shell_quote(&format!("{root}/runner.sh")),
        shell_quote(root),
        shell_quote(token)
    )
}

fn setup(flags: &RunFlags, cwd: &Path) -> String {
    if flags.no_setup {
        return String::new();
    }
    if let Some(cmd) = &flags.setup {
        return cmd.clone();
    }
    if cwd.join("uv.lock").is_file() {
        "uv sync --frozen".into()
    } else if cwd.join("pyproject.toml").is_file() {
        "uv sync".into()
    } else if cwd.join("requirements.txt").is_file() {
        "uv venv --system-site-packages .venv && uv pip install -r requirements.txt".into()
    } else {
        String::new()
    }
}

fn duration(text: &str, now: Timestamp) -> Result<i64> {
    crate::expiry::parse_deadline_duration(text, now)
        .map(|at| crate::expiry::deadline_seconds(at) - now.as_second())
        .map_err(Error::Usage)
}

fn args(flags: &RunFlags) -> UpArgs {
    UpArgs {
        repo: Some(crate::repos::Repo::NoRepo),
        mode: Some("container".into()),
        gpu: flags.gpu.clone(),
        num_gpus: flags.num_gpus,
        max_price: flags.max_price,
        offer: flags.offer,
        disk: flags.disk,
        yes: flags.yes || flags.dry_run,
        dry_run: flags.dry_run,
        expire: flags.expire.clone(),
        reliability: flags.reliability,
        unverified: flags.unverified,
        interruptible: false,
    }
}

fn choose(ctx: &Ctx, flags: &RunFlags, cfg: &Dict, args: &UpArgs) -> Result<Dict> {
    let search = OfferSearch::new(Vast::new(ctx), args, cfg)?;
    if ctx.interactive() {
        let wizard = crate::wizard::LaunchWizard::new(
            &ctx.env,
            crate::wizard::WizardOptions {
                repos: Job::new(|| Ok(Vec::new())),
                search: Some(Arc::new(search.clone())),
                models: crate::offers::known_gpu_models(&ctx.env),
                recent_gpus: crate::history::recent_picks(&ctx.env, "gpu"),
                choose_offer: !flags.yes && flags.offer.is_none(),
                owner: String::new(),
                tpu_setup_error: Some("gpu run phase 1 supports Vast GPUs only.".into()),
                tpu_transition: None,
                tpu_prices: None,
                tpu_cached_prices: None,
                tpu_types: Vec::new(),
                tpu_catalog: None,
            },
        );
        let picks = wizard.run(
            ctx,
            crate::wizard::Picks::new(
                Some(crate::repos::Repo::NoRepo),
                Some("container"),
                flags.gpu.as_deref(),
            ),
        )?;
        if picks.tpu.is_some() {
            return Err(Error::Usage("gpu run supports Vast GPUs only.".into()));
        }
        if let Some(offer) = picks.offer {
            return Ok(offer);
        }
        return crate::launch::select_offer(
            ctx,
            search.offers("container", picks.gpu.as_deref())?,
            args,
        );
    }
    crate::launch::select_offer(ctx, search.offers("container", flags.gpu.as_deref())?, args)
}

/// `cwd` is explicit so all file tests are isolated without process-wide chdir.
pub fn run(ctx: &Ctx, flags: &RunFlags, cfg: &Dict, cwd: &Path) -> Result<()> {
    let outcome = run_inner(ctx, flags, cfg, cwd);
    ctx.end_run();
    outcome
}

fn run_inner(ctx: &Ctx, flags: &RunFlags, cfg: &Dict, cwd: &Path) -> Result<()> {
    let started = ctx.clock.monotonic();
    let now = ctx.clock.now();
    let expire = flags
        .expire
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.get("default_expire").and_then(Value::as_str))
        .ok_or_else(|| {
            Error::Usage(
                "gpu run requires --expire DURATION or default_expire in config.json.".into(),
            )
        })?;
    let seconds = duration(expire, now)?;
    if seconds <= DOWNLOAD_RESERVE {
        return Err(Error::Usage(
            "gpu run needs --expire longer than 10m to reserve time for downloading results."
                .into(),
        ));
    }
    let deadline = Timestamp::from_second(now.as_second() + seconds)
        .map_err(|_| Error::Usage("Expiry out of range.".into()))?;
    let linger = duration(&flags.linger, now)?;
    if !run_files::relative(Path::new(&flags.outputs)) {
        return Err(Error::Usage(
            "--outputs must be a relative directory without . or .. components.".into(),
        ));
    }
    if !ctx.interactive() && flags.gpu.is_none() {
        return Err(Error::Usage(
            "Non-terminal gpu run requires --gpu MODEL.".into(),
        ));
    }
    if !ctx.interactive() && !flags.yes && !flags.dry_run {
        return Err(Error::Usage(
            "Non-terminal gpu run requires --yes; use --dry-run to inspect without renting.".into(),
        ));
    }
    if flags.image.as_deref().is_some_and(str::is_empty) {
        return Err(Error::Usage("--image must not be empty.".into()));
    }
    let mut env = String::new();
    for key in &flags.env {
        if key.is_empty()
            || !key
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
        {
            return Err(Error::Usage(format!(
                "Invalid --env name: {key}. Pass a variable NAME, not NAME=value."
            )));
        }
        let value = ctx
            .env
            .get(key)
            .ok_or_else(|| Error::Usage(format!("Local variable {key} is not set.")))?;
        if value.contains('\0') {
            return Err(Error::Usage(format!(
                "Local variable {key} contains a NUL."
            )));
        }
        env.push_str(&format!("export {key}={}\n", shell_quote(value)));
    }
    let args = args(flags);
    crate::launch::validate_launch_args(&args).map_err(|e| Error::Usage(e.text().into()))?;
    let upload = run_files::prepare(ctx, cwd, run_files::max_bytes(&flags.max_upload)?)?;
    let setup = setup(flags, &upload.root.join(&upload.relative));
    let mut cfg = cfg.clone();
    if let Some(image) = &flags.image {
        cfg.insert("container_image", Value::str(image));
    }
    // Validate generated startup and config before even searching offers.
    crate::startup::run_script(deadline)?;
    if !matches!(cfg.get("container_image"), Some(Value::Str(image)) if !image.is_empty()) {
        return Err(Error::Usage(
            "container_image must be a non-empty string.".into(),
        ));
    }
    phase(ctx, "launch");
    ctx.result("provider", Value::str("vast"));
    ctx.result("dry_run", Value::Bool(flags.dry_run));
    ctx.result("created", Value::Bool(false));
    ctx.result("destroyed", Value::Bool(false));
    ctx.result("expiry_armed", Value::Bool(false));
    for key in [
        "id",
        "run_id",
        "exit_code",
        "outputs_dir",
        "log_path",
        "log_tail",
        "downloaded_bytes",
    ] {
        ctx.result(key, Value::Null);
    }
    ctx.result("expires_at", Value::str(deadline.to_string()));
    ctx.result("uploaded_files", Value::Int(upload.files.len() as i64));
    ctx.result("uploaded_bytes", Value::Int(upload.bytes as i64));
    ctx.result(
        "setup",
        if setup.is_empty() {
            Value::Null
        } else {
            Value::str(&setup)
        },
    );
    let offer = choose(ctx, flags, &cfg, &args)?;
    let rate = offer_price(&offer)?;
    if !rate.is_finite() || rate < 0.0 {
        return Err(Error::msg(
            "Offer has no finite nonnegative hourly price; no instance created.",
        ));
    }
    crate::guardrails::spending(ctx, &cfg, Some(rate), Some(deadline))?;
    let estimate = rate * seconds as f64 / 3600.0;
    ctx.result("estimated_spend", Value::Float(estimate));
    ctx.result("price_per_hour", Value::Float(rate));
    ctx.result("gpu", offer.get("gpu_name").cloned().unwrap_or(Value::Null));
    let label = upload
        .root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let request = vast::run_request(&offer, flags.disk, &cfg, deadline, label)?;
    vast::argv(&request)?;
    if flags.dry_run {
        ctx.result("request", crate::redact::redact(&Value::List(request)));
        ctx.println(&format!("DRY RUN: {} files, {} bytes; setup: {}; maximum compute estimate ${}. No instance created.", upload.files.len(), upload.bytes, setup, crate::pyfmt::fixed(estimate, 3)))?;
        return Ok(());
    }
    if !flags.yes
        && !crate::prompt::confirm(
            ctx,
            &format!(
                "Run {} at ${}/hr, at most {expire} (up to ${})? Type 'yes': ",
                shell_join(&flags.command),
                crate::pyfmt::fixed(rate, 3),
                crate::pyfmt::fixed(estimate, 3)
            ),
        )?
    {
        return Err(Error::Cancelled(130));
    }
    if ctx.clock.now().as_second() >= deadline.as_second() - DOWNLOAD_RESERVE {
        return Err(Error::Usage(
            "Not enough time remains before expiry; choose a longer --expire.".into(),
        ));
    }
    // Before create, Ctrl-C cancels at once: nothing has been rented yet.
    ctx.begin_run();
    ctx.set_interrupt_notice(Some(format!(
        "Vast may have created an instance; boot expiry is {deadline}. Check: gpu ls"
    )));
    ctx.result("created", Value::Null);
    let reply = Vast::new(ctx).create(&request).inspect_err(|_| {
        if let Some(notice) = ctx.interrupt_notice() {
            let _ = ctx.eprintln(&notice);
        }
    })?;
    crate::launch::created_summary(reply.as_ref()).inspect_err(|_| {
        ctx.result("created", Value::Bool(false));
        ctx.set_interrupt_notice(None);
    })?;
    let id = reply.as_ref().and_then(crate::launch::created_id).ok_or_else(|| Error::status(ExitCode::CleanupUnverified, "Create response has no instance ID; cleanup cannot be verified. Check: gpu ls, then gpu down ID --yes"))?;
    ctx.result("id", Value::str(&id));
    ctx.result("created", Value::Bool(true));
    ctx.set_interrupt_notice(Some(format!("Instance {id} may still be running; it self-destroys at {deadline}. Check: gpu ls, gpu down {id} --yes")));
    // Everything after create is inside this scope: even download/log/disk errors
    // must reach destroy and provider-state verification.
    let run_id = format!("{}-{id}", now.strftime("%Y%m%dT%H%M%SZ"));
    let local = cwd.join("gpu-runs").join(&run_id);
    let mut paths = None;
    let job = (|| {
        if !cwd.join("gpu-runs").exists() {
            ctx.eprintln("Results go in gpu-runs/; add gpu-runs/ to .gitignore.")?;
        }
        fs::create_dir_all(&local)?;
        ctx.result("run_id", Value::str(&run_id));
        ctx.result(
            "log_path",
            Value::str(local.join("log.txt").to_string_lossy()),
        );
        fs::write(local.join("log.txt"), b"")?;
        ctx.check_cancelled()?;
        let remaining =
            (deadline.as_second() - DOWNLOAD_RESERVE - ctx.clock.now().as_second()) as f64;
        let wait_timeout = flags.timeout.min(remaining.max(0.0));
        let cap = if remaining < flags.timeout {
            let message = format!("SSH wait limited to {}s by --expire (requested --timeout {}s; 10m reserved for results). Use a longer --expire to allow more boot time.", crate::pyfmt::general(wait_timeout), crate::pyfmt::general(flags.timeout));
            ctx.eprintln(&message)?;
            message + " "
        } else {
            String::new()
        };
        let hint = format!(
            "{cap}Next: retry gpu run with a longer --expire/--timeout or another --offer."
        );
        let (alias, _) = crate::commands::wait_for_ssh_with_hint(ctx, &id, wait_timeout, &hint)?;
        crate::expiry::manage_expiry(ctx, &alias, &id, "arm", Some(deadline))?;
        ctx.result("expiry_armed", Value::Bool(true));
        let target = format!("/workspace/gpu-run-{run_id}");
        let jobdir = if upload.relative.as_os_str().is_empty() {
            target.clone()
        } else {
            format!("{target}/{}", upload.relative.to_string_lossy())
        };
        let root = format!("/root/.local/state/gpu-run/{run_id}");
        paths = Some((alias.clone(), root.clone(), jobdir.clone()));
        ctx.check_cancelled()?;
        phase(ctx, "upload");
        let command = format!(
            "mkdir -p {} && tar -xzf - --no-same-owner -C {}",
            shell_quote(&target),
            shell_quote(&target)
        );
        let remaining =
            (deadline.as_second() - DOWNLOAD_RESERVE - ctx.clock.now().as_second()).max(1) as f64;
        checked_transfer(
            ctx,
            &ssh(&alias, &command, remaining).stdin(Input::File(upload.archive.path().into())),
            1,
            "Upload failed; instance will be destroyed.",
        )?;
        ctx.check_cancelled()?;
        let mut config = format!("FORWARDED={}\n", shell_quote(&env));
        for (key, value) in [
            ("JOBDIR", jobdir),
            ("OUTPUTS", flags.outputs.clone()),
            ("SETUP", setup.clone()),
            ("COMMAND", shell_join(&flags.command)),
            ("DEADLINE", deadline.as_second().to_string()),
            (
                "JOB_DEADLINE",
                (deadline.as_second() - DOWNLOAD_RESERVE).to_string(),
            ),
            ("LINGER", linger.to_string()),
        ] {
            config.push_str(&format!("{key}={}\n", shell_quote(&value)));
        }
        let hook = format!("#!/bin/bash\nroot=\"${{HOME:-/root}}/.local/state/gpu-expire\"\nGPU_EXPIRE_SOURCE=$(cat \"$root/runner.sh\")\nexport GPU_EXPIRE_SOURCE\nexec bash \"$root/runner.sh\" arm {id} \"$1\"\n");
        let payload = format!(
            "GPU_RUN_SOURCE={}\nGPU_RUN_CONFIG={}\nGPU_RUN_HOOK={}\n{REMOTE_SOURCE}",
            shell_quote(REMOTE_SOURCE),
            shell_quote(&config),
            shell_quote(&hook)
        );
        let start = format!(
            "bash -s -- start {} {}",
            shell_quote(&root),
            shell_quote(&run_id)
        );
        checked(
            ctx,
            &ssh(&alias, &start, 30.0).stdin(Input::Text(payload)),
            "Job startup unverified; instance will be destroyed.",
        )?;
        follow(ctx, &alias, &id, &root, &run_id, deadline, &local)
    })();
    let mut outcome = job;
    let mut failure_phase = ctx
        .result_snapshot()
        .get("phase")
        .cloned()
        .unwrap_or(Value::str("launch"));
    // Cancellation is cooperative, including a setup/start SSH error caused by SIGINT.
    if ctx.interrupts() > 0 {
        outcome = Err(Error::Cancelled(130));
    }
    if let Some((alias, root, jobdir)) = paths {
        let status = || {
            checked(
                ctx,
                &ssh(&alias, &control(&root, &run_id, "status"), 5.0),
                "Status unavailable.",
            )
            .ok()
            .and_then(|out| parse_status(&out.stdout))
        };
        let mut last = None;
        if outcome.is_err() {
            let _ = checked(
                ctx,
                &ssh(&alias, &control(&root, &run_id, "stop"), 10.0),
                "Stop unverified.",
            );
            // The supervisor runs the linger hook before publishing the final
            // status; wait through transient SSH failures until it is seen.
            let until = ctx.clock.monotonic() + 35.0;
            while ctx.interrupts() < 2 && ctx.clock.monotonic() < until {
                if let Some(s) = status().filter(|s| s.exit.is_some()) {
                    last = Some(s);
                    break;
                }
                ctx.clock.sleep(1.0);
            }
        }
        if ctx.interrupts() < 2 {
            // Fetch bytes written during cancellation/TERM grace, too.
            if let Some(status) = last.or_else(status) {
                let _ = finish_log(ctx, &alias, &root, &local, status.size);
            }
            // The finished job shortened expiry to linger; an attached
            // download keeps the original deadline.
            if crate::expiry::manage_expiry(ctx, &alias, &id, "arm", Some(deadline)).is_err() {
                let _ = ctx.eprintln(&format!(
                    "Could not restore expiry to {deadline}; downloading anyway."
                ));
            }
            phase(ctx, "download");
            let downloaded = download(ctx, &alias, &jobdir, &flags.outputs, &local, deadline);
            if let Err(error) = downloaded {
                let _ = ctx.eprintln(&format!("Could not fetch results: {}", error.text()));
                if outcome.is_ok()
                    || matches!(
                        outcome,
                        Err(Error::Remote(_)
                            | Error::Status(ExitCode::JobTimeout | ExitCode::InstanceLost, _))
                    )
                {
                    outcome = Err(error);
                    failure_phase = Value::str("download");
                }
            }
        }
    }
    phase(ctx, "cleanup");
    let cleaned = destroy(ctx, &id);
    if let Err(error) = cleaned {
        outcome = Err(error);
        failure_phase = Value::str("cleanup");
    } else {
        ctx.result("destroyed", Value::Bool(true));
        ctx.set_interrupt_notice(None);
        if let Err(Error::Status(ExitCode::WaitTimeout, message)) = &mut outcome {
            *message = format!("Instance {id} was destroyed. {message}");
        }
        if ctx.interrupts() > 0 {
            outcome = Err(Error::Cancelled(130));
        }
    }
    ctx.result(
        "duration_seconds",
        Value::Float(ctx.clock.monotonic() - started),
    );
    ctx.result(
        "log_tail",
        run_files::tail(&local.join("log.txt"))
            .map(Value::Str)
            .unwrap_or(Value::Null),
    );
    if outcome.is_ok() {
        phase(ctx, "done");
    } else {
        ctx.result("phase", failure_phase);
    }
    let object = crate::output::envelope(ctx, "run", &outcome);
    if local.exists() {
        if let Err(error) = fs::write(local.join("result.json"), pyjson::dumps(&object, None)) {
            if outcome.is_ok() {
                outcome = Err(error.into());
            }
        }
        let _ = ctx.eprintln(&format!("Results: {}", local.display()));
    }
    outcome
}

struct Status {
    phase: String,
    exit: Option<i32>,
    timed: bool,
    size: u64,
}
fn parse_status(text: &str) -> Option<Status> {
    let fields: Vec<_> = text.trim().split('\t').collect();
    if fields.len() != 4 || !["setup", "job"].contains(&fields[0]) {
        return None;
    }
    Some(Status {
        phase: fields[0].into(),
        exit: if fields[1] == "-" {
            None
        } else {
            Some(fields[1].parse().ok()?)
        },
        timed: fields[2] == "1",
        size: fields[3].parse().ok()?,
    })
}

fn follow(
    ctx: &Ctx,
    alias: &str,
    id: &str,
    root: &str,
    token: &str,
    deadline: Timestamp,
    local: &Path,
) -> Result<()> {
    let mut offset = 0u64;
    let mut retry = 1.0f64;
    let chunk = tempfile::NamedTempFile::new()?;
    let mut log = OpenOptions::new()
        .append(true)
        .open(local.join("log.txt"))?;
    loop {
        ctx.check_cancelled()?;
        if ctx.clock.now() >= deadline {
            return Err(Error::status(
                ExitCode::JobTimeout,
                "Job lifetime deadline reached.",
            ));
        }
        let response = checked(
            ctx,
            &ssh(alias, &control(root, token, "status"), 15.0),
            "Job status unavailable.",
        )
        .ok();
        if response.as_ref().is_some_and(|o| o.stdout.trim() == "lost") {
            return Err(Error::status(
                ExitCode::InstanceLost,
                "Remote job supervisor vanished (instance may have restarted).",
            ));
        }
        let status = response.and_then(|o| parse_status(&o.stdout));
        let Some(status) = status else {
            let live = vast::list_instances(ctx, Some(15.0)).ok();
            if live.as_ref().is_some_and(|live| {
                vast::find_instance(live, id).is_none_or(|i| {
                    matches!(
                        i.get("actual_status").and_then(Value::as_str),
                        Some("stopped" | "exited" | "destroyed")
                    )
                })
            }) {
                return Err(Error::status(
                    ExitCode::InstanceLost,
                    "Instance stopped or vanished while the job was running.",
                ));
            }
            if retry == 1.0 {
                ctx.eprintln("Connection lost; job keeps running on the instance; reconnecting.")?;
            }
            ctx.clock.sleep(retry);
            retry = (retry * 2.0).min(15.0);
            continue;
        };
        phase(ctx, &status.phase);
        let amount = status.size.saturating_sub(offset).min(CHUNK);
        if amount > 0 {
            let command = format!(
                "dd if={} bs=64K iflag=skip_bytes,count_bytes skip={offset} count={amount} status=none",
                shell_quote(&format!("{root}/log"))
            );
            if checked(
                ctx,
                &ssh(alias, &command, 15.0).stdout_file(chunk.path()),
                "Log connection interrupted.",
            )
            .is_err()
            {
                ctx.clock.sleep(1.0);
                continue;
            }
            let bytes = fs::read(chunk.path())?;
            if bytes.len() as u64 != amount {
                return Err(Error::msg(
                    "Remote log truncated; refusing to lose or duplicate bytes.",
                ));
            }
            log.write_all(&bytes)?;
            if ctx.json() {
                ctx.console.write_err_bytes(&bytes)?;
            } else {
                ctx.console.write_out_bytes(&bytes)?;
            }
            offset += amount;
        }
        retry = 1.0;
        if offset >= status.size {
            if let Some(exit) = status.exit {
                ctx.result("exit_code", Value::Int(exit.into()));
                if status.timed {
                    return Err(Error::status(
                        ExitCode::JobTimeout,
                        "Job reached its deadline; fetching partial results.",
                    ));
                }
                return if exit == 0 {
                    Ok(())
                } else {
                    Err(Error::Remote(exit))
                };
            }
            ctx.clock.sleep(1.0);
        }
    }
}

fn finish_log(ctx: &Ctx, alias: &str, root: &str, local: &Path, size: u64) -> Result<()> {
    let path = local.join("log.txt");
    let mut offset = fs::metadata(&path)?.len();
    let mut log = OpenOptions::new().append(true).open(path)?;
    let chunk = tempfile::NamedTempFile::new()?;
    while offset < size && ctx.interrupts() < 2 {
        let amount = (size - offset).min(CHUNK);
        let command = format!(
            "dd if={} bs=64K iflag=skip_bytes,count_bytes skip={offset} count={amount} status=none",
            shell_quote(&format!("{root}/log"))
        );
        checked_transfer(
            ctx,
            &ssh(alias, &command, 15.0).stdout_file(chunk.path()),
            2,
            "Final log fetch interrupted.",
        )?;
        let bytes = fs::read(chunk.path())?;
        if bytes.len() as u64 != amount {
            return Err(Error::msg("Remote log truncated."));
        }
        log.write_all(&bytes)?;
        if ctx.json() {
            ctx.console.write_err_bytes(&bytes)?;
        } else {
            ctx.console.write_out_bytes(&bytes)?;
        }
        offset += amount;
    }
    Ok(())
}

fn download(
    ctx: &Ctx,
    alias: &str,
    jobdir: &str,
    outputs: &str,
    local: &Path,
    deadline: Timestamp,
) -> Result<()> {
    let archive = tempfile::NamedTempFile::new()?;
    let seconds = (deadline.as_second() - ctx.clock.now().as_second()).max(1) as f64;
    let command = format!(
        "tar -C {} -cf - -- {}",
        shell_quote(jobdir),
        shell_quote(outputs)
    );
    checked_transfer(
        ctx,
        &ssh(alias, &command, seconds).stdout_file(archive.path()),
        2,
        "Results download failed; instance will still be destroyed.",
    )?;
    let bytes = run_files::unpack(ctx, archive.path(), local, Path::new(outputs))?;
    ctx.result("downloaded_bytes", Value::Int(bytes as i64));
    ctx.result(
        "outputs_dir",
        Value::str(local.join(outputs).to_string_lossy()),
    );
    Ok(())
}

/// Only ever called with the ID this invocation just created. A successful
/// mutation is not enough: verify disappearance before removing the SSH alias.
fn destroy(ctx: &Ctx, id: &str) -> Result<()> {
    let cleanup = || -> Result<()> {
        let live = vast::list_instances(ctx, Some(15.0))?;
        if vast::find_instance(&live, id).is_some() {
            crate::process::run(
                ctx,
                &["vastai", "destroy", "instance", id, "--yes"],
                Some(30.0),
            )?;
            crate::commands::wait_for_lifecycle(ctx, id, "destroy", 60.0)?;
        }
        crate::ssh_config::remove_host(&ctx.env, id)?;
        Ok(())
    };
    cleanup().map_err(|_| Error::status(ExitCode::CleanupUnverified, format!("Instance {id}: destruction unverified; billing may continue. Next: gpu down {id} --yes")))
}
