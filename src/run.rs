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

#[cfg(test)]
mod tests;

pub const REMOTE_SOURCE: &str = include_str!("../remote/run_remote.sh");
const DOWNLOAD_RESERVE: i64 = 600;
const CHUNK: u64 = 131072;

fn phase(ctx: &Ctx, phase: &str) {
    ctx.result("phase", Value::str(phase));
}

pub(crate) fn step_result(
    ctx: &Ctx,
    result: std::result::Result<Output, crate::process::RunError>,
    message: &str,
) -> Result<Output> {
    match result {
        Ok(out) if out.code == 0 => Ok(out),
        Ok(out) => {
            // Mask exact forwarded values before taking a bounded stderr tail.
            let detail = ctx.safe_diagnostic(&out.stderr);
            let tail: String = detail
                .chars()
                .rev()
                .take(2048)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            Err(Error::msg(format!(
                "{message} Exit status {}. {}",
                out.code,
                tail.trim()
            )))
        }
        Err(error) => Err(Error::msg(format!(
            "{message} {}",
            ctx.safe_diagnostic(&error.python_text("ssh"))
        ))),
    }
}

fn checked(ctx: &Ctx, backend: &dyn RunBackend, cmd: &Cmd, message: &str) -> Result<Output> {
    backend.remote_result(ctx, ctx.run(cmd), message)
}

fn checked_transfer(
    ctx: &Ctx,
    backend: &dyn RunBackend,
    cmd: &Cmd,
    threshold: usize,
    message: &str,
) -> Result<Output> {
    backend.remote_result(ctx, ctx.transfer(cmd, threshold), message)
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

/// The shared runner owns uploads, supervision, logs and downloads. A backend
/// owns provider planning, paid-resource lifecycle and remote command transport.
#[derive(Debug)]
pub(crate) struct RunPlan {
    pub(crate) gpu: Value,
    pub(crate) rate: f64,
    // Report request errors after publishing pricing, preserving partial JSON.
    pub(crate) request: Result<Value>,
}

pub(crate) trait RunBackend {
    fn provider(&self) -> &'static str;
    fn plan(
        &self,
        ctx: &Ctx,
        flags: &RunFlags,
        cfg: &Dict,
        deadline: Timestamp,
        label: String,
    ) -> Result<RunPlan>;
    /// Call `created` as soon as the ID is known, before any fallible readiness
    /// work, so the shared runner can always clean up a partially launched host.
    fn launch(
        &mut self,
        ctx: &Ctx,
        request: &Value,
        deadline: Timestamp,
        timeout: f64,
        created: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<String>;
    fn remote(&self, command: &str, timeout: f64) -> Cmd;
    fn remote_result(
        &self,
        ctx: &Ctx,
        result: std::result::Result<Output, crate::process::RunError>,
        message: &str,
    ) -> Result<Output> {
        step_result(ctx, result, message)
    }
    fn lost(&self, ctx: &Ctx, id: &str) -> bool;
    fn restore_expiry(&self, ctx: &Ctx, id: &str, deadline: Timestamp) -> Result<()>;
    fn hook(&self, id: &str) -> String;
    fn start_prelude(&self) -> String {
        String::new()
    }
    fn wait_for_startup(&self) -> bool;
    fn destroy(&self, ctx: &Ctx, id: &str) -> Result<()>;
}

#[derive(Default)]
struct VastBackend {
    alias: String,
    offer: Option<Dict>,
}

impl RunBackend for VastBackend {
    fn provider(&self) -> &'static str {
        "vast"
    }

    fn plan(
        &self,
        _ctx: &Ctx,
        flags: &RunFlags,
        cfg: &Dict,
        deadline: Timestamp,
        label: String,
    ) -> Result<RunPlan> {
        let offer = self
            .offer
            .clone()
            .ok_or_else(|| Error::msg("No Vast offer selected."))?;
        let rate = offer_price(&offer)?;
        if !rate.is_finite() || rate < 0.0 {
            return Err(Error::msg(
                "Offer has no finite nonnegative hourly price; no instance created.",
            ));
        }
        let request = vast::run_request(&offer, flags.disk, cfg, deadline, label)
            .and_then(|request| vast::argv(&request).map(|_| Value::List(request)));
        Ok(RunPlan {
            gpu: offer.get("gpu_name").cloned().unwrap_or(Value::Null),
            rate,
            request,
        })
    }

    fn launch(
        &mut self,
        ctx: &Ctx,
        request: &Value,
        deadline: Timestamp,
        timeout: f64,
        created: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<String> {
        ctx.set_interrupt_notice(Some(format!(
            "Vast may have created an instance; boot expiry is {deadline}. Check: gpu ls"
        )));
        ctx.result("created", Value::Null);
        let request = request
            .as_list()
            .ok_or_else(|| Error::msg("Invalid Vast run request."))?;
        let reply = Vast::new(ctx).create(request).inspect_err(|_| {
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
        created(&id)?;
        ctx.check_cancelled()?;
        let remaining =
            (deadline.as_second() - DOWNLOAD_RESERVE - ctx.clock.now().as_second()) as f64;
        let wait_timeout = timeout.min(remaining.max(0.0));
        let cap = if remaining < timeout {
            let message = format!("SSH wait limited to {}s by --expire (requested --timeout {}s; 10m reserved for results). Use a longer \
                --expire to allow more boot time.", crate::pyfmt::general(wait_timeout), crate::pyfmt::general(timeout));
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
        self.alias = alias;
        ctx.result("expiry_armed", Value::Bool(true));
        Ok(id)
    }

    fn remote(&self, command: &str, timeout: f64) -> Cmd {
        ssh(&self.alias, command, timeout)
    }

    fn lost(&self, ctx: &Ctx, id: &str) -> bool {
        let live = vast::list_instances(ctx, Some(15.0)).ok();
        live.as_ref().is_some_and(|live| {
            vast::find_instance(live, id).is_none_or(|i| {
                matches!(
                    i.get("actual_status").and_then(Value::as_str),
                    Some("stopped" | "exited" | "destroyed")
                )
            })
        })
    }

    fn restore_expiry(&self, ctx: &Ctx, id: &str, deadline: Timestamp) -> Result<()> {
        crate::expiry::manage_expiry(ctx, &self.alias, id, "arm", Some(deadline)).map(|_| ())
    }

    fn hook(&self, id: &str) -> String {
        format!("#!/bin/bash\nroot=\"${{HOME:-/root}}/.local/state/gpu-expire\"\nGPU_EXPIRE_SOURCE=$(cat \
            \"$root/runner.sh\")\nexport GPU_EXPIRE_SOURCE\nexec bash \"$root/runner.sh\" arm {id} \"$1\"\n")
    }

    fn wait_for_startup(&self) -> bool {
        true
    }

    fn destroy(&self, ctx: &Ctx, id: &str) -> Result<()> {
        destroy(ctx, id)
    }
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
        .map(|at| at.as_second() - now.as_second())
        .map_err(Error::Usage)
}

fn run_duration(text: &str, now: Timestamp) -> Result<i64> {
    let seconds = duration(text, now)?;
    if seconds <= DOWNLOAD_RESERVE {
        return Err(Error::Usage(
            "gpu run needs --expire longer than 10m to reserve time for downloading results."
                .into(),
        ));
    }
    Ok(seconds)
}

fn expiry_screen(now: Timestamp) -> crate::picker::Screen<String, String> {
    crate::picker::Screen::new(
        "Run expiry (e.g. 30m, 2h; longer than 10m)",
        Vec::new,
        crate::picker::Next::Done,
    )
    .text_input(Some(Box::new(move |text| {
        run_duration(text.trim(), now)
            .map(|_| ())
            .map_err(|e| e.text().to_string())
    })))
    .input_label("Expiry")
    .submit_on_right(false)
    .back_clears_input(false)
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

enum Selection {
    Vast(Dict),
    Modal(String, Option<crate::modal::Rates>),
}

fn choose(ctx: &Ctx, flags: &RunFlags, cfg: &Dict, args: &UpArgs) -> Result<Selection> {
    if ctx.interactive() {
        let modal_only = flags.provider.as_deref() == Some("modal");
        let search = if modal_only {
            None
        } else {
            Some(OfferSearch::new(Vast::new(ctx), args, cfg)?)
        };
        let modal = if flags.provider.as_deref() != Some("vast") {
            let worker = ctx.clone();
            let (cpu, memory) = crate::modal::resources(flags)?;
            let rates =
                Job::new(move || crate::modal::rates(&worker).map_err(|e| e.text().to_string()));
            Some((rates, flags.num_gpus, cpu, memory))
        } else {
            None
        };
        let wizard = crate::wizard::LaunchWizard::new(
            &ctx.env,
            crate::wizard::WizardOptions {
                repos: Job::new(|| Ok(Vec::new())),
                search: search
                    .clone()
                    .map(|s| Arc::new(s) as Arc<dyn crate::wizard::Search>),
                models: if modal_only {
                    Vec::new()
                } else {
                    crate::offers::known_gpu_models(&ctx.env)
                },
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
        )
        .with_modal(modal.clone(), modal_only);
        let picks = wizard.run(
            ctx,
            crate::wizard::Picks::new(
                Some(crate::repos::Repo::NoRepo),
                Some("container"),
                flags.gpu.as_deref(),
            ),
        )?;
        if let Some(name) = picks.modal {
            return Ok(Selection::Modal(
                name,
                modal.and_then(|(rates, ..)| rates.result()),
            ));
        }
        if picks.tpu.is_some() {
            return Err(Error::Usage("gpu run supports Vast GPUs only.".into()));
        }
        if let Some(offer) = picks.offer {
            return Ok(Selection::Vast(offer));
        }
        let search = search.ok_or_else(|| Error::Usage("Modal requires --gpu MODEL.".into()))?;
        return crate::launch::select_offer(
            ctx,
            search.offers("container", picks.gpu.as_deref())?,
            args,
        )
        .map(Selection::Vast);
    }
    let search = OfferSearch::new(Vast::new(ctx), args, cfg)?;
    crate::launch::select_offer(ctx, search.offers("container", flags.gpu.as_deref())?, args)
        .map(Selection::Vast)
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
    let prompted_expire;
    let expire = flags
        .expire
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.get("default_expire").and_then(Value::as_str));
    let expire = match expire {
        Some(expire) => expire,
        None if ctx.interactive() => {
            prompted_expire = crate::picker::pick(ctx, expiry_screen(now))?;
            prompted_expire.trim()
        }
        None => {
            return Err(Error::Usage(
                "gpu run requires --expire DURATION or default_expire in config.json.".into(),
            ));
        }
    };
    let seconds = run_duration(expire, now)?;
    if flags.provider.as_deref() == Some("modal") {
        crate::modal::validate(flags, seconds)?;
    } else if flags.cpu.is_some() || flags.memory.is_some() {
        return Err(Error::Usage(
            "--cpu and --memory require --provider modal.".into(),
        ));
    }
    let deadline = Timestamp::from_second(now.as_second() + seconds)
        .map_err(|_| Error::Usage("Expiry out of range.".into()))?;
    let linger = if flags.linger == "0" {
        0
    } else {
        duration(&flags.linger, now)?
    };
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
        ctx.protect_secret(value);
        env.push_str(&format!("export {key}={}\n", shell_quote(value)));
    }
    let args = args(flags);
    crate::launch::validate_launch_args(&args).map_err(|e| Error::Usage(e.text().into()))?;
    let upload = run_files::prepare(ctx, cwd, run_files::max_bytes(&flags.max_upload)?)?;
    let secrets: Vec<_> = upload
        .files
        .iter()
        .filter(|path| run_files::secret_path(path))
        .collect();
    if !secrets.is_empty() {
        let names = secrets
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        if !flags.allow_secrets {
            return Err(Error::Usage(format!("Refusing secret-looking upload files: {names}. Ignore them in .gitignore or explicitly pass --allow-secrets.")));
        }
        ctx.eprintln(&format!(
            "WARNING: uploading secret-looking files allowed by --allow-secrets: {names}."
        ))?;
    }
    let setup = setup(flags, &upload.root.join(&upload.relative));
    let mut cfg = cfg.clone();
    if let Some(image) = &flags.image {
        cfg.insert("container_image", Value::str(image));
    }
    // Vast startup/config do not apply to Modal's default Debian image.
    if flags.provider.as_deref() != Some("modal") {
        crate::startup::run_script(deadline)?;
    }
    if flags.provider.as_deref() != Some("modal")
        && !matches!(cfg.get("container_image"), Some(Value::Str(image)) if !image.is_empty())
    {
        return Err(Error::Usage(
            "container_image must be a non-empty string.".into(),
        ));
    }
    phase(ctx, "launch");
    ctx.result(
        "provider",
        Value::str(flags.provider.as_deref().unwrap_or("vast")),
    );
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
    if flags.dry_run {
        ctx.result(
            "files",
            Value::List(
                upload
                    .files
                    .iter()
                    .map(|p| Value::str(p.to_string_lossy()))
                    .collect(),
            ),
        );
    }
    ctx.result("uploaded_bytes", Value::Int(upload.bytes as i64));
    ctx.result(
        "setup",
        if setup.is_empty() {
            Value::Null
        } else {
            Value::str(&setup)
        },
    );
    let label = upload
        .root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let mut selected_flags = flags.clone();
    let mut backend: Box<dyn RunBackend> = if flags.provider.as_deref() == Some("modal")
        && (flags.gpu.is_some() || !ctx.interactive())
    {
        Box::new(crate::modal::ModalBackend::new(None))
    } else {
        match choose(ctx, flags, &cfg, &args)? {
            Selection::Vast(offer) => Box::new(VastBackend {
                offer: Some(offer),
                ..VastBackend::default()
            }),
            Selection::Modal(name, rates) => {
                selected_flags.provider = Some("modal".into());
                selected_flags.gpu = Some(name);
                ctx.result("provider", Value::str("modal"));
                crate::modal::validate(&selected_flags, seconds)?;
                Box::new(crate::modal::ModalBackend::new(rates))
            }
        }
    };
    ctx.result("provider", Value::str(backend.provider()));
    let plan = backend.plan(ctx, &selected_flags, &cfg, deadline, label)?;
    let rate = plan.rate;
    crate::guardrails::spending(ctx, &cfg, Some(rate), Some(deadline))?;
    let estimate = rate * seconds as f64 / 3600.0;
    ctx.result("estimated_spend", Value::Float(estimate));
    ctx.result("price_per_hour", Value::Float(rate));
    ctx.result("gpu", plan.gpu);
    let request = plan.request?;
    if flags.dry_run {
        ctx.result("request", request);
        ctx.println(&format!("DRY RUN: {} files, {} bytes; setup: {}; maximum compute estimate ${}. No instance created.", upload.files.len(), upload.bytes, setup, crate::pyfmt::fixed(estimate, 3)))?;
        return Ok(());
    }
    if !flags.yes
        && !crate::prompt::confirm(
            ctx,
            &format!(
                "Run {} ({} files, {} bytes) at ${}/hr, at most {expire} (up to ${})? Type 'yes': ",
                shell_join(&flags.command),
                upload.files.len(),
                upload.bytes,
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
    // Prove the destination is writable before spending anything. Keep a unique
    // staging directory on the same filesystem, then rename once the ID is known.
    let results = cwd.join("gpu-runs");
    if !results.exists() {
        ctx.eprintln("Results go in gpu-runs/; add gpu-runs/ to .gitignore.")?;
    }
    fs::create_dir_all(&results).map_err(|e| crate::error::os_error(&e, &results))?;
    let staging = tempfile::Builder::new()
        .prefix(".gpu-run-")
        .tempdir_in(&results)
        .map_err(|e| crate::error::os_error(&e, &results))?;
    fs::write(staging.path().join("log.txt"), b"")
        .map_err(|e| crate::error::os_error(&e, &staging.path().join("log.txt")))?;
    // Before create, Ctrl-C cancels at once: nothing has been rented yet.
    ctx.begin_run();
    let mut created_id = None;
    let mut run_id = String::new();
    let mut local = cwd.join("gpu-runs");
    let mut paths = None;
    let mut launched = false;
    let job = (|| {
        let id = backend.launch(ctx, &request, deadline, flags.timeout, &mut |id| {
            // Record ownership before any local I/O can fail. Launch readiness
            // failures must still reach provider-state-verified destruction.
            created_id = Some(id.to_string());
            run_id = if id.starts_with("modal:") {
                ctx.result_snapshot()
                    .get("run_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::msg("Modal launch has no run ID."))?
                    .into()
            } else {
                format!("{}-{id}", now.strftime("%Y%m%dT%H%M%SZ"))
            };
            local = cwd.join("gpu-runs").join(&run_id);
            fs::rename(staging.path(), &local).map_err(|e| crate::error::os_error(&e, &local))?;
            ctx.result("run_id", Value::str(&run_id));
            ctx.result(
                "log_path",
                Value::str(local.join("log.txt").to_string_lossy()),
            );
            Ok(())
        })?;
        let target = format!("/workspace/gpu-run-{run_id}");
        let jobdir = if upload.relative.as_os_str().is_empty() {
            target.clone()
        } else {
            format!("{target}/{}", upload.relative.to_string_lossy())
        };
        let root = format!("/root/.local/state/gpu-run/{run_id}");
        paths = Some((root.clone(), jobdir.clone()));
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
            backend.as_ref(),
            &backend
                .remote(&command, remaining)
                .stdin(Input::File(upload.archive.path().into())),
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
            (
                "WAIT_FOR_STARTUP",
                if backend.wait_for_startup() { "1" } else { "0" }.into(),
            ),
        ] {
            config.push_str(&format!("{key}={}\n", shell_quote(&value)));
        }
        let hook = backend.hook(&id);
        let payload = format!(
            "{}GPU_RUN_SOURCE={}\nGPU_RUN_CONFIG={}\nGPU_RUN_HOOK={}\n{REMOTE_SOURCE}",
            backend.start_prelude(),
            shell_quote(REMOTE_SOURCE),
            shell_quote(&config),
            shell_quote(&hook)
        );
        let start = format!(
            "bash -s -- start {} {}",
            shell_quote(&root),
            shell_quote(&run_id)
        );
        launched = true;
        checked(
            ctx,
            backend.as_ref(),
            &backend.remote(&start, 30.0).stdin(Input::Text(payload)),
            "Job startup unverified; instance will be destroyed.",
        )?;
        follow(ctx, backend.as_ref(), &id, &root, &run_id, deadline, &local)
    })();
    // A create failure without an ID cannot be cleaned up, just as before.
    let Some(id) = created_id else {
        return job;
    };
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
    // Ephemeral Modal files cannot be recovered once the sandbox is lost.
    // Do not replace exit 18 with the inevitable download transport failure.
    let modal_lost = backend.provider() == "modal"
        && (matches!(outcome, Err(Error::Status(ExitCode::InstanceLost, _)))
            || (outcome.is_err() && backend.lost(ctx, &id)));
    if modal_lost && ctx.interrupts() == 0 {
        outcome = Err(Error::status(ExitCode::InstanceLost, "Modal sandbox ended while the job was running (possibly preempted); ephemeral outputs are lost."));
    }
    if let Some((root, jobdir)) = paths.filter(|_| !modal_lost) {
        let response = || {
            checked(
                ctx,
                backend.as_ref(),
                &backend.remote(&control(&root, &run_id, "status"), 5.0),
                "Status unavailable.",
            )
            .ok()
        };
        let status = || response().and_then(|out| parse_status(&out.stdout));
        let mut last = None;
        if outcome.is_err() && launched {
            let _ = checked(
                ctx,
                backend.as_ref(),
                &backend.remote(&control(&root, &run_id, "stop"), 10.0),
                "Stop unverified.",
            );
            // The supervisor runs the linger hook before publishing the final
            // status; wait through transient SSH failures until it is seen.
            let until = ctx.clock.monotonic() + 35.0;
            while ctx.interrupts() < 2 && ctx.clock.monotonic() < until {
                let out = response();
                if out.as_ref().is_some_and(|o| o.stdout.trim() == "lost") {
                    break;
                }
                if let Some(s) = out
                    .and_then(|o| parse_status(&o.stdout))
                    .filter(|s| s.exit.is_some())
                {
                    last = Some(s);
                    break;
                }
                ctx.clock.sleep(1.0);
            }
        }
        if ctx.interrupts() < 2 {
            // Fetch bytes written during cancellation/TERM grace, too.
            if let Some(status) = last.or_else(status) {
                let _ = finish_log(ctx, backend.as_ref(), &root, &local, status.size);
            }
            // The finished job shortened expiry to linger; an attached
            // download keeps the original deadline.
            if backend.restore_expiry(ctx, &id, deadline).is_err() {
                let _ = ctx.eprintln(&format!(
                    "Could not restore expiry to {deadline}; downloading anyway."
                ));
            }
            phase(ctx, "download");
            let downloaded = download(
                ctx,
                backend.as_ref(),
                &root,
                &jobdir,
                &flags.outputs,
                &local,
                deadline,
            );
            if let Err(error) = downloaded {
                let _ = ctx.eprintln(&format!("Could not fetch results: {}", error.text()));
                if outcome.is_ok()
                    || matches!(
                        outcome,
                        Err(Error::Remote(_)
                            | Error::Status(ExitCode::JobTimeout | ExitCode::InstanceLost, _))
                    )
                {
                    outcome = if backend.provider() == "modal" && backend.lost(ctx, &id) {
                        Err(Error::status(
                            ExitCode::InstanceLost,
                            "Modal sandbox ended during download; ephemeral outputs are lost.",
                        ))
                    } else {
                        Err(error)
                    };
                    failure_phase = Value::str("download");
                }
            }
        }
    }
    phase(ctx, "cleanup");
    let cleaned = backend.destroy(ctx, &id);
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
    backend: &dyn RunBackend,
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
            backend,
            &backend.remote(&control(root, token, "status"), 15.0),
            "Job status unavailable.",
        );
        let response = match response {
            Err(
                error @ Error::Status(
                    ExitCode::NotSetup | ExitCode::AuthExpired | ExitCode::CommandTimeout,
                    _,
                ),
            ) if backend.provider() == "modal" => return Err(error),
            response => response.ok(),
        };
        if response.as_ref().is_some_and(|o| o.stdout.trim() == "lost") {
            return Err(Error::status(
                ExitCode::InstanceLost,
                "Remote job supervisor vanished (instance may have restarted).",
            ));
        }
        let status = response.and_then(|o| parse_status(&o.stdout));
        let Some(status) = status else {
            if backend.lost(ctx, id) {
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
            if let Err(error) = checked(
                ctx,
                backend,
                &backend.remote(&command, 15.0).stdout_file(chunk.path()),
                "Log connection interrupted.",
            ) {
                if backend.provider() == "modal"
                    && matches!(
                        error,
                        Error::Status(
                            ExitCode::NotSetup | ExitCode::AuthExpired | ExitCode::CommandTimeout,
                            _
                        )
                    )
                {
                    return Err(error);
                }
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

fn finish_log(
    ctx: &Ctx,
    backend: &dyn RunBackend,
    root: &str,
    local: &Path,
    size: u64,
) -> Result<()> {
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
            backend,
            &backend.remote(&command, 15.0).stdout_file(chunk.path()),
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

/// Only SSH transport failures are retried, a few times; a remote tar error
/// or an exhausted budget proceeds straight to destroy.
const DOWNLOAD_RETRY_DELAYS: [f64; 3] = [5.0, 15.0, 30.0];
const DESTROY_ATTEMPTS: usize = 3;
const DESTROY_VERIFY_SECONDS: f64 = 60.0;

fn download(
    ctx: &Ctx,
    backend: &dyn RunBackend,
    root: &str,
    jobdir: &str,
    outputs: &str,
    local: &Path,
    deadline: Timestamp,
) -> Result<()> {
    let archive = tempfile::NamedTempFile::new()?;
    let seconds = (deadline.as_second() - ctx.clock.now().as_second()).max(1) as f64;
    let command = format!(
        "tar -C {} -cf - -- {} & p=$!; while kill -0 $p 2>/dev/null; do date +%s >{}; sleep 5; done; wait $p",
        shell_quote(jobdir),
        shell_quote(outputs),
        shell_quote(&format!("{root}/attached"))
    );
    let end = ctx.clock.monotonic() + seconds;
    let message = "Results download failed; instance will still be destroyed.";
    for delay in DOWNLOAD_RETRY_DELAYS
        .iter()
        .copied()
        .map(Some)
        .chain([None])
    {
        let remaining = (end - ctx.clock.monotonic()).max(0.1);
        let result = ctx.transfer(
            &backend
                .remote(&command, remaining)
                .stdout_file(archive.path()),
            2,
        );
        let transport = matches!(result, Ok(Output { code: 255, .. }));
        let error = match backend.remote_result(ctx, result, message) {
            Ok(_) => break,
            Err(error) => error,
        };
        let Some(delay) = delay.filter(|delay| {
            transport && ctx.interrupts() < 2 && ctx.clock.monotonic() + delay < end
        }) else {
            return Err(error);
        };
        ctx.eprintln(&format!("{} Retrying download.", error.text()))?;
        ctx.clock.sleep(delay);
    }
    let bytes = run_files::unpack(ctx, archive.path(), local, Path::new(outputs))?;
    ctx.result("downloaded_bytes", Value::Int(bytes as i64));
    ctx.result(
        "outputs_dir",
        Value::str(local.join(outputs).to_string_lossy()),
    );
    Ok(())
}

/// Only ever called with the ID this invocation just created. A successful
/// mutation is not enough: verify disappearance. Always remove the local alias.
fn destroy(ctx: &Ctx, id: &str) -> Result<()> {
    let cleanup = || -> Result<()> {
        // This invocation owns the freshly created ID. A lagging or broken
        // inventory must never suppress the paid-resource cleanup request.
        let mut verified = Ok(());
        for _ in 0..DESTROY_ATTEMPTS {
            let sent = crate::process::run(
                ctx,
                &["vastai", "destroy", "instance", id, "--yes"],
                Some(30.0),
            );
            if let Err(error) = sent {
                // A failed/timeout reply does not prove the mutation was rejected.
                // A definite absence after the request still verifies completion.
                let _ = ctx.eprintln(&format!(
                    "Destroy request completion uncertain: {}",
                    ctx.safe_diagnostic(error.text())
                ));
            }
            verified = crate::commands::wait_for_lifecycle(
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
    let _ = crate::ssh_config::remove_host(&ctx.env, id);
    outcome.map_err(|_| Error::status(ExitCode::CleanupUnverified, format!("Instance {id}: destruction unverified; billing may continue. Next: gpu down {id} --yes")))
}
