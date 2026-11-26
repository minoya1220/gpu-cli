//! Attached jobs: detached remote execution, resumable logs, download, verified cleanup.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jiff::Timestamp;

use crate::cli::RunFlags;
use crate::ctx::Ctx;
use crate::error::{Error, ExitCode, Result};
use crate::launch::UpArgs;
use crate::picker::Job;
use crate::process::{Cmd, Input, Output};
use crate::provider::Provider;
use crate::pyfmt::{shell_join, shell_quote};
use crate::pyjson::{self, Dict, Value};
use crate::run_files;
use crate::vast::offers::OfferSearch;
use crate::vast::Vast;

#[cfg(test)]
mod tests;

pub const REMOTE_SOURCE: &str = include_str!("../remote/run_remote.sh");
pub(crate) const DOWNLOAD_RESERVE: i64 = 600;
const CHUNK: u64 = 131072;
/// Default `--timeout`: how long launch waits for SSH readiness.
const READINESS_TIMEOUT: f64 = 600.0;
/// Where results land, under the directory `gpu run` was started in.
const RESULTS_DIR: &str = "gpu-runs";

/// The run's current phase, reported as the `phase` result field.
struct Phase<'a> {
    ctx: &'a Ctx,
    current: std::cell::RefCell<String>,
}

impl<'a> Phase<'a> {
    fn start(ctx: &'a Ctx, phase: &str) -> Phase<'a> {
        let started = Phase {
            ctx,
            current: std::cell::RefCell::new(String::new()),
        };
        started.set(phase);
        started
    }
    fn set(&self, phase: &str) {
        *self.current.borrow_mut() = phase.into();
        self.ctx.result("phase", Value::str(phase));
    }
    fn get(&self) -> String {
        self.current.borrow().clone()
    }
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
            ctx.safe_diagnostic(&error.describe("ssh"))
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

/// The shared runner owns uploads, supervision, logs and downloads. A backend
/// owns provider planning, paid-resource lifecycle and remote command transport.
#[derive(Debug)]
pub(crate) struct RunPlan {
    pub(crate) gpu: Value,
    pub(crate) rate: f64,
    // Report request errors after publishing pricing, preserving partial JSON.
    pub(crate) request: Result<Value>,
}

/// `gpu run` rents Vast or Modal GPUs, never TPUs.
const NO_TPU: &str = "gpu run uses Vast or Modal GPUs; TPUs are not supported.";

pub(crate) trait RunBackend {
    fn provider(&self) -> Provider;
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
        // The created instance ID, and the provider's own run ID if it names runs.
        created: &mut dyn FnMut(&str, Option<&str>) -> Result<()>,
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
    /// Wrap the source archive extraction, which reads the archive on stdin.
    fn upload(&self, extract: String) -> String {
        extract
    }
    fn start_prelude(&self, _jobdir: &str, _outputs: &str) -> String {
        String::new()
    }
    fn outputs_hook(&self) -> &str {
        ""
    }
    /// The tar operand that archives the live outputs directory.
    fn archive_operand(&self, outputs: &str) -> String {
        outputs.into()
    }
    /// Persistent providers can fetch committed outputs after the host is lost.
    fn output_archive(&self, _outputs: &str, _timeout: f64) -> Option<Cmd> {
        None
    }
    fn wait_for_startup(&self) -> bool;
    fn destroy(&self, ctx: &Ctx, id: &str) -> Result<()>;
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
        .map_err(Error::usage)
}

fn run_duration(text: &str, now: Timestamp) -> Result<i64> {
    let seconds = duration(text, now)?;
    if seconds <= DOWNLOAD_RESERVE {
        return Err(Error::usage(
            "gpu run needs --expire longer than 10m to reserve time for downloading results.",
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

/// A run is a container launch without a repository.
impl From<&RunFlags> for UpArgs {
    fn from(flags: &RunFlags) -> UpArgs {
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
            max_upload: String::new(),
        }
    }
}

enum Selection {
    Vast(Dict),
    Modal(String, Option<crate::modal::Rates>),
}

fn choose(ctx: &Ctx, flags: &RunFlags, cfg: &Dict, args: &UpArgs) -> Result<Selection> {
    if ctx.interactive() {
        let modal_only = flags.provider == Some(Provider::Modal);
        let search = if modal_only {
            None
        } else {
            Some(OfferSearch::new(Vast::new(ctx), args, cfg)?)
        };
        let modal = if flags.provider != Some(Provider::Vast) {
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
                search: search
                    .clone()
                    .map(|s| Arc::new(s) as Arc<dyn crate::wizard::Search>),
                models: if modal_only {
                    Vec::new()
                } else {
                    crate::vast::offers::known_gpu_models(&ctx.env)
                },
                recent_gpus: crate::history::recent_picks(&ctx.env, "gpu"),
                choose_offer: !flags.yes && flags.offer.is_none(),
                tpu: crate::wizard::TpuOptions {
                    setup_error: Some(NO_TPU.into()),
                    ..Default::default()
                },
                ..Default::default()
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
            return Err(Error::usage(NO_TPU));
        }
        if let Some(offer) = picks.offer {
            return Ok(Selection::Vast(offer));
        }
        let search = search.ok_or_else(|| Error::usage("Modal requires --gpu MODEL."))?;
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
    if flags.provider.is_some_and(Provider::amd) {
        crate::amd::validate_run(flags)?;
    }
    let job = prepare(ctx, flags, cfg, cwd)?;
    let phase = Phase::start(ctx, "launch");
    report_plan(ctx, flags, &job);
    let (mut backend, selected) = select_backend(ctx, flags, &job)?;
    let Some((request, ledger)) = approve(ctx, flags, &selected, &job, backend.as_ref())? else {
        return Ok(());
    };
    let (results, staging) = stage_results(ctx, cwd)?;
    // Before create, Ctrl-C cancels at once: nothing has been rented yet.
    ctx.begin_run();
    let mut session = Session {
        created_id: None,
        ledger,
        run_id: String::new(),
        local: results.clone(),
        paths: None,
        launched: false,
    };
    let outcome = execute(
        &phase,
        flags,
        &job,
        backend.as_mut(),
        &request,
        (&results, staging.path()),
        &mut session,
    );
    // A create failure without an ID cannot be cleaned up, just as before.
    let Some(id) = session.created_id.clone() else {
        return outcome;
    };
    let owned = Owned {
        phase: &phase,
        flags,
        job: &job,
        backend: backend.as_ref(),
        session: &session,
        id: &id,
    };
    settle(&owned, outcome, started)
}

/// Everything `gpu run` checks and builds locally before choosing hardware.
struct Prepared {
    now: Timestamp,
    expire: String,
    seconds: i64,
    deadline: Timestamp,
    linger: i64,
    /// `export NAME=value` lines for the forwarded `--env` variables.
    env: String,
    args: UpArgs,
    upload: run_files::Upload,
    setup: String,
    cfg: Dict,
}

/// What a run owns once a paid instance may exist.
struct Session {
    created_id: Option<String>,
    /// The launch estimate to reserve right before create.
    ledger: Option<crate::spend::Budget>,
    run_id: String,
    local: PathBuf,
    /// The remote state root and job directory, once known.
    paths: Option<(String, String)>,
    /// Whether the job start request went out.
    launched: bool,
}

/// A created instance's run, as the phases after the job see it.
struct Owned<'a> {
    phase: &'a Phase<'a>,
    flags: &'a RunFlags,
    job: &'a Prepared,
    backend: &'a dyn RunBackend,
    session: &'a Session,
    id: &'a str,
}

/// Validate flags and config, then pack the upload. Nothing remote happens.
fn prepare(ctx: &Ctx, flags: &RunFlags, cfg: &Dict, cwd: &Path) -> Result<Prepared> {
    let now = ctx.clock.now();
    let expire = flags
        .expire
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| cfg.get("default_expire").and_then(Value::as_str));
    let expire = match expire {
        Some(expire) => expire.to_string(),
        None if ctx.interactive() => crate::picker::pick(ctx, expiry_screen(now))?
            .trim()
            .to_string(),
        None => {
            return Err(Error::usage(
                "gpu run requires --expire DURATION or default_expire in config.json.",
            ));
        }
    };
    let seconds = run_duration(&expire, now)?;
    if flags.provider == Some(Provider::Modal) {
        crate::modal::validate(flags, seconds)?;
    } else if flags.cpu.is_some() || flags.memory.is_some() {
        return Err(Error::usage("--cpu and --memory require --provider modal."));
    }
    let deadline = Timestamp::from_second(now.as_second() + seconds)
        .map_err(|_| Error::usage("Expiry out of range."))?;
    let linger = if flags.linger == "0" {
        0
    } else {
        duration(&flags.linger, now)?
    };
    if !run_files::relative(Path::new(&flags.outputs)) {
        return Err(Error::usage(
            "--outputs must be a relative directory without . or .. components.",
        ));
    }
    if !ctx.interactive() && flags.gpu.is_none() {
        return Err(Error::usage("Non-terminal gpu run requires --gpu MODEL."));
    }
    if !ctx.interactive() && !flags.yes && !flags.dry_run {
        return Err(Error::usage(
            "Non-terminal gpu run requires --yes; use --dry-run to inspect without renting.",
        ));
    }
    if flags.image.as_deref().is_some_and(str::is_empty) {
        return Err(Error::usage("--image must not be empty."));
    }
    let env = forwarded_env(ctx, flags)?;
    let args = UpArgs::from(flags);
    crate::launch::validate_launch_args(&args).map_err(|e| Error::usage(e.text()))?;
    let upload = run_files::prepare_including(
        ctx,
        cwd,
        run_files::max_bytes(&flags.max_upload)?,
        false,
        &flags.include,
    )?;
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
            return Err(Error::usage(format!(
                "Refusing secret-looking upload files: {names}. Ignore them in .gitignore or explicitly pass --allow-secrets."
            )));
        }
        ctx.eprintln(&format!(
            "WARNING: uploading secret-looking files allowed by --allow-secrets: {names}."
        ))?;
    }
    let setup = setup(flags, &upload.root);
    let mut cfg = cfg.clone();
    if let Some(image) = &flags.image {
        cfg.insert("container_image", Value::str(image));
    }
    // Vast startup/config do not apply to Modal's default Debian image.
    let vast = flags.provider.is_none_or(|p| p == Provider::Vast);
    if vast {
        crate::vast::startup::run_script(deadline)?;
    }
    if vast && !matches!(cfg.get("container_image"), Some(Value::Str(image)) if !image.is_empty()) {
        return Err(Error::usage("container_image must be a non-empty string."));
    }
    Ok(Prepared {
        now,
        expire,
        seconds,
        deadline,
        linger,
        env,
        args,
        upload,
        setup,
        cfg,
    })
}

/// `export` lines for each `--env NAME`, read from the local environment and
/// protected from every later diagnostic.
fn forwarded_env(ctx: &Ctx, flags: &RunFlags) -> Result<String> {
    let mut env = String::new();
    for key in &flags.env {
        if key.is_empty()
            || !key
                .bytes()
                .enumerate()
                .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
        {
            return Err(Error::usage(format!(
                "Invalid --env name: {key}. Pass a variable NAME, not NAME=value."
            )));
        }
        let value = ctx
            .env
            .get(key)
            .ok_or_else(|| Error::usage(format!("Local variable {key} is not set.")))?;
        if value.contains('\0') {
            return Err(Error::usage(format!(
                "Local variable {key} contains a NUL."
            )));
        }
        ctx.protect_secret(value);
        env.push_str(&format!("export {key}={}\n", shell_quote(value)));
    }
    Ok(env)
}

/// Every `gpu run` result field, in contract order, before hardware is chosen.
fn report_plan(ctx: &Ctx, flags: &RunFlags, job: &Prepared) {
    ctx.result(
        "provider",
        Value::str(flags.provider.unwrap_or(Provider::Vast).key()),
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
    ctx.result("expires_at", Value::str(job.deadline.to_string()));
    if flags.provider.is_some_and(Provider::amd) {
        ctx.result("expires_at", Value::Null);
        ctx.result("requested_expires_at", Value::str(job.deadline.to_string()));
        ctx.result("expiry_supported", Value::Bool(false));
        ctx.result("live_supported", Value::Bool(false));
    }
    ctx.result("uploaded_files", Value::Int(job.upload.files.len() as i64));
    if flags.dry_run {
        ctx.result(
            "files",
            Value::List(
                job.upload
                    .files
                    .iter()
                    .map(|p| Value::str(p.to_string_lossy()))
                    .collect(),
            ),
        );
    }
    ctx.result("uploaded_bytes", Value::Int(job.upload.bytes as i64));
    ctx.result(
        "setup",
        if job.setup.is_empty() {
            Value::Null
        } else {
            Value::str(&job.setup)
        },
    );
}

/// The backend for the chosen hardware, and the flags as that choice
/// completed them.
fn select_backend(
    ctx: &Ctx,
    flags: &RunFlags,
    job: &Prepared,
) -> Result<(Box<dyn RunBackend>, RunFlags)> {
    let mut selected = flags.clone();
    let backend: Box<dyn RunBackend> = if let Some(provider) = flags.provider.filter(|p| p.amd()) {
        Box::new(crate::amd::Backend(provider))
    } else if flags.provider == Some(Provider::Modal) && (flags.gpu.is_some() || !ctx.interactive())
    {
        Box::new(crate::modal::ModalBackend::new(None))
    } else {
        match choose(ctx, flags, &job.cfg, &job.args)? {
            Selection::Vast(offer) => Box::new(crate::vast::run::VastBackend::new(offer)),
            Selection::Modal(name, rates) => {
                selected.provider = Some(Provider::Modal);
                selected.gpu = Some(name);
                ctx.result("provider", Value::str(Provider::Modal.key()));
                crate::modal::validate(&selected, job.seconds)?;
                Box::new(crate::modal::ModalBackend::new(rates))
            }
        }
    };
    ctx.result("provider", Value::str(backend.provider().key()));
    Ok((backend, selected))
}

/// Price the plan against the spending limits, then end a dry run or ask for
/// confirmation. `None` when nothing should be rented.
fn approve(
    ctx: &Ctx,
    flags: &RunFlags,
    selected: &RunFlags,
    job: &Prepared,
    backend: &dyn RunBackend,
) -> Result<Option<(Value, Option<crate::spend::Budget>)>> {
    let label = job
        .upload
        .root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let plan = backend.plan(ctx, selected, &job.cfg, job.deadline, label)?;
    let rate = plan.rate;
    let ledger = crate::guardrails::spending(ctx, &job.cfg, Some(rate), Some(job.deadline))?;
    let estimate = rate * job.seconds as f64 / 3600.0;
    ctx.result("estimated_spend", Value::Float(estimate));
    ctx.result("price_per_hour", Value::Float(rate));
    ctx.result("gpu", plan.gpu);
    let request = plan.request?;
    if flags.dry_run {
        crate::guardrails::dry_run_budget(ctx, &job.cfg)?;
        if backend.provider().amd() {
            ctx.eprintln("AMD planning only: estimated spend is hypothetical; no enforceable deadline or live attached run is supported.")?;
        }
        ctx.result("request", request);
        ctx.println(&format!(
            "DRY RUN: {} files, {} bytes; setup: {}; maximum compute estimate ${}. No instance created.",
            job.upload.files.len(),
            job.upload.bytes,
            job.setup,
            crate::pyfmt::fixed(estimate, 3)
        ))?;
        return Ok(None);
    }
    if !flags.yes
        && !crate::prompt::confirm(
            ctx,
            Some(&format!(
                "Run {} ({} files, {} bytes) at ${}/hr, at most {} (up to ${})?",
                shell_join(&flags.command),
                job.upload.files.len(),
                job.upload.bytes,
                crate::pyfmt::fixed(rate, 3),
                job.expire,
                crate::pyfmt::fixed(estimate, 3)
            )),
        )?
    {
        return Err(Error::Cancelled(130));
    }
    if ctx.clock.now().as_second() >= job.deadline.as_second() - DOWNLOAD_RESERVE {
        return Err(Error::usage(
            "Not enough time remains before expiry; choose a longer --expire.",
        ));
    }
    Ok(Some((request, ledger)))
}

/// Prove the destination is writable before spending anything. Keep a unique
/// staging directory on the same filesystem, renamed once the ID is known.
fn stage_results(ctx: &Ctx, cwd: &Path) -> Result<(PathBuf, tempfile::TempDir)> {
    let results = cwd.join(RESULTS_DIR);
    if !results.exists() {
        ctx.eprintln(&format!(
            "Results go in {RESULTS_DIR}/; add {RESULTS_DIR}/ to .gitignore."
        ))?;
    }
    fs::create_dir_all(&results).map_err(|e| crate::error::os_error(&e, &results))?;
    let staging = tempfile::Builder::new()
        .prefix(".gpu-run-")
        .tempdir_in(&results)
        .map_err(|e| crate::error::os_error(&e, &results))?;
    fs::write(staging.path().join("log.txt"), b"")
        .map_err(|e| crate::error::os_error(&e, &staging.path().join("log.txt")))?;
    Ok((results, staging))
}

/// Create, upload, start and follow the job. `session` records ownership as
/// soon as an instance exists, whatever fails afterwards.
fn execute(
    phase: &Phase,
    flags: &RunFlags,
    job: &Prepared,
    backend: &mut dyn RunBackend,
    request: &Value,
    (results, staging): (&Path, &Path),
    session: &mut Session,
) -> Result<()> {
    let ctx = phase.ctx;
    let timeout = flags
        .timeout
        .unwrap_or(if backend.provider() == Provider::Modal {
            crate::modal::READINESS_TIMEOUT
        } else {
            READINESS_TIMEOUT
        });
    let backend_key = backend.provider().key();
    let mut reservation = crate::spend::reserve(ctx, session.ledger)?;
    let launched = backend.launch(ctx, request, job.deadline, timeout, &mut |id, named| {
        // Record ownership before any local I/O can fail. Launch readiness
        // failures must still reach provider-state-verified destruction.
        session.created_id = Some(id.to_string());
        crate::spend::claim_best_effort(
            ctx,
            reservation.take(),
            &crate::spend::key(backend_key, id),
        );
        session.run_id = match named {
            Some(named) => named.into(),
            None => format!("{}-{id}", job.now.strftime("%Y%m%dT%H%M%SZ")),
        };
        session.local = results.join(&session.run_id);
        fs::rename(staging, &session.local)
            .map_err(|e| crate::error::os_error(&e, &session.local))?;
        ctx.result("run_id", Value::str(&session.run_id));
        ctx.result(
            "log_path",
            Value::str(session.local.join("log.txt").to_string_lossy()),
        );
        Ok(())
    });
    let id = match launched {
        Err(error) if error.is_rejection() && session.created_id.is_none() => {
            crate::spend::release_best_effort(ctx, reservation.take());
            return Err(error);
        }
        launched => launched?,
    };
    let run_id = session.run_id.clone();
    let jobdir = format!("/workspace/gpu-run-{run_id}");
    let root = format!("/root/.local/state/gpu-run/{run_id}");
    session.paths = Some((root.clone(), jobdir.clone()));
    ctx.check_cancelled()?;
    phase.set("upload");
    let command = backend.upload(format!(
        "mkdir -p {} && tar -xzf - --no-same-owner -C {}",
        shell_quote(&jobdir),
        shell_quote(&jobdir)
    ));
    let remaining =
        (job.deadline.as_second() - DOWNLOAD_RESERVE - ctx.clock.now().as_second()).max(1) as f64;
    checked_transfer(
        ctx,
        backend,
        &backend
            .remote(&command, remaining)
            .stdin(Input::File(job.upload.archive.path().into())),
        1,
        "Upload failed; instance will be destroyed.",
    )?;
    ctx.check_cancelled()?;
    let mut config = format!("FORWARDED={}\n", shell_quote(&job.env));
    for (key, value) in [
        ("JOBDIR", jobdir.clone()),
        ("OUTPUTS", flags.outputs.clone()),
        ("SETUP", job.setup.clone()),
        ("COMMAND", shell_join(&flags.command)),
        ("DEADLINE", job.deadline.as_second().to_string()),
        (
            "JOB_DEADLINE",
            (job.deadline.as_second() - DOWNLOAD_RESERVE).to_string(),
        ),
        ("LINGER", job.linger.to_string()),
        (
            "WAIT_FOR_STARTUP",
            if backend.wait_for_startup() { "1" } else { "0" }.into(),
        ),
    ] {
        config.push_str(&format!("{key}={}\n", shell_quote(&value)));
    }
    let hook = backend.hook(&id);
    let payload = format!(
        "{}GPU_RUN_SOURCE={}\nGPU_RUN_CONFIG={}\nGPU_RUN_HOOK={}\nGPU_RUN_OUTPUTS_HOOK={}\n{REMOTE_SOURCE}",
        backend.start_prelude(&jobdir, &flags.outputs),
        shell_quote(REMOTE_SOURCE),
        shell_quote(&config),
        shell_quote(&hook),
        shell_quote(backend.outputs_hook())
    );
    let start = format!(
        "bash -s -- start {} {}",
        shell_quote(&root),
        shell_quote(&run_id)
    );
    session.launched = true;
    checked(
        ctx,
        backend,
        &backend.remote(&start, 30.0).stdin(Input::Text(payload)),
        "Job startup unverified; instance will be destroyed.",
    )?;
    follow(
        phase,
        backend,
        &id,
        &root,
        &run_id,
        job.deadline,
        &session.local,
    )
}

/// After a created instance's job ends, however it ended: recover the log
/// and outputs, destroy the instance, and write `result.json`.
fn settle(owned: &Owned, mut outcome: Result<()>, started: f64) -> Result<()> {
    let Owned {
        phase,
        backend,
        session,
        id,
        ..
    } = *owned;
    let ctx = phase.ctx;
    let mut failure_phase = Value::str(phase.get());
    // Cancellation is cooperative, including a setup/start SSH error caused by SIGINT.
    if ctx.interrupts() > 0 {
        outcome = Err(Error::Cancelled(130));
    }
    // Logs and supervisor state are ephemeral, but Modal outputs survive in a
    // run-scoped Volume. Preserve exit 18 while recovering its last commit.
    let modal_lost = backend.provider() == Provider::Modal
        && (matches!(outcome, Err(Error::Status(ExitCode::InstanceLost, _)))
            || (outcome.is_err() && backend.lost(ctx, id)));
    if modal_lost && ctx.interrupts() == 0 {
        outcome = Err(Error::status(
            ExitCode::InstanceLost,
            if session.launched {
                "Modal sandbox ended while the job was running (possibly preempted); recovering the last committed outputs.".into()
            } else {
                format!("Modal sandbox ended during {}, before the job started; the job never ran. A sandbox stops itself when its upload makes no progress for 5 minutes; check network access to Modal.", phase.get())
            },
        ));
    }
    // Before the start request nothing ran, so there is no log or output to
    // fetch: go straight to cleanup.
    if let Some(paths) = session.paths.as_ref().filter(|_| session.launched) {
        if modal_lost {
            if ctx.interrupts() < 2 {
                phase.set("download");
                if let Err(error) = download(
                    ctx,
                    backend,
                    paths,
                    &owned.flags.outputs,
                    &session.local,
                    owned.job.deadline,
                    true,
                ) {
                    let _ = ctx.eprintln(&format!(
                        "Could not fetch committed outputs: {}",
                        error.text()
                    ));
                }
            }
        } else if recover(owned, paths, &mut outcome) {
            failure_phase = Value::str("download");
        }
    }
    phase.set("cleanup");
    let cleaned = backend.destroy(ctx, id);
    if let Err(error) = cleaned {
        outcome = Err(error);
        failure_phase = Value::str("cleanup");
    } else {
        ctx.result("destroyed", Value::Bool(true));
        ctx.set_interrupt_notice(None);
        if let Err(Error::Status(ExitCode::WaitTimeout, message)) = &mut outcome {
            *message = format!("Instance {id} was destroyed. {message}").into();
        }
        if ctx.interrupts() > 0 {
            outcome = Err(Error::Cancelled(130));
        }
    }
    ctx.result(
        "duration_seconds",
        Value::Float(ctx.clock.monotonic() - started),
    );
    let local = &session.local;
    ctx.result(
        "log_tail",
        run_files::tail(&local.join("log.txt"))
            .map(Value::Str)
            .unwrap_or(Value::Null),
    );
    if outcome.is_ok() {
        phase.set("done");
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

/// Stop a failed job, fetch the rest of its log and download its outputs
/// under the original deadline. True when a download failure replaced
/// `outcome`.
fn recover(owned: &Owned, paths: &(String, String), outcome: &mut Result<()>) -> bool {
    let Owned {
        phase,
        flags,
        job,
        backend,
        session,
        id,
    } = *owned;
    let ctx = phase.ctx;
    let root = &paths.0;
    let run_id = &session.run_id;
    let deadline = job.deadline;
    let response = || {
        checked(
            ctx,
            backend,
            &backend.remote(&control(root, run_id, "status"), 5.0),
            "Status unavailable.",
        )
        .ok()
    };
    let status = || response().and_then(|out| parse_status(&out.stdout));
    let mut last = None;
    if outcome.is_err() && session.launched {
        let _ = checked(
            ctx,
            backend,
            &backend.remote(&control(root, run_id, "stop"), 10.0),
            "Stop unverified.",
        );
        // The supervisor runs the linger hook before publishing the final
        // status; wait through transient SSH failures until it is seen.
        let poll = ctx.polling(35.0, 1.0);
        while ctx.interrupts() < 2 && !poll.expired() {
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
            poll.pause();
        }
    }
    if ctx.interrupts() >= 2 {
        return false;
    }
    // Fetch bytes written during cancellation/TERM grace, too.
    if let Some(status) = last.or_else(status) {
        let _ = finish_log(ctx, backend, root, &session.local, status.size);
    }
    // The finished job shortened expiry to linger; an attached
    // download keeps the original deadline.
    if backend.restore_expiry(ctx, id, deadline).is_err() {
        let _ = ctx.eprintln(&format!(
            "Could not restore expiry to {deadline}; downloading anyway."
        ));
    }
    phase.set("download");
    let downloaded = download(
        ctx,
        backend,
        paths,
        &flags.outputs,
        &session.local,
        deadline,
        false,
    );
    let Err(error) = downloaded else {
        return false;
    };
    let _ = ctx.eprintln(&format!("Could not fetch results: {}", error.text()));
    let lost = backend.provider() == Provider::Modal && backend.lost(ctx, id);
    let committed = lost
        && ctx.interrupts() < 2
        && download(
            ctx,
            backend,
            paths,
            &flags.outputs,
            &session.local,
            deadline,
            true,
        )
        .inspect_err(|error| {
            let _ = ctx.eprintln(&format!(
                "Could not fetch committed outputs: {}",
                error.text()
            ));
        })
        .is_ok();
    if outcome.is_ok()
        || matches!(
            outcome,
            Err(Error::Remote(_) | Error::Status(ExitCode::JobTimeout | ExitCode::InstanceLost, _))
        )
    {
        *outcome = if committed {
            Err(Error::status(
                ExitCode::InstanceLost,
                "Modal sandbox ended during download; fetched its last committed outputs.",
            ))
        } else if lost {
            Err(Error::status(
                ExitCode::InstanceLost,
                "Modal sandbox ended during download; could not fetch its last committed outputs.",
            ))
        } else {
            Err(error)
        };
        return true;
    }
    false
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
    phase: &Phase,
    backend: &dyn RunBackend,
    id: &str,
    root: &str,
    token: &str,
    deadline: Timestamp,
    local: &Path,
) -> Result<()> {
    let ctx = phase.ctx;
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
            ) if backend.provider() == Provider::Modal => return Err(error),
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
        phase.set(&status.phase);
        let amount = status.size.saturating_sub(offset).min(CHUNK);
        if amount > 0 {
            if let Err(error) = checked(
                ctx,
                backend,
                &log_chunk(backend, root, offset, amount, chunk.path()),
                "Log connection interrupted.",
            ) {
                if backend.provider() == Provider::Modal
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
            append_chunk(ctx, &mut log, chunk.path(), amount)?;
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

/// The remote command writing `amount` log bytes from `offset` to `into`.
fn log_chunk(backend: &dyn RunBackend, root: &str, offset: u64, amount: u64, into: &Path) -> Cmd {
    let command = format!(
        "dd if={} bs=64K iflag=skip_bytes,count_bytes skip={offset} count={amount} status=none",
        shell_quote(&format!("{root}/log"))
    );
    backend.remote(&command, 15.0).stdout_file(into)
}

/// Append a fetched chunk to the local log and echo it: to stderr under
/// `--json`, so stdout stays one JSON document.
fn append_chunk(ctx: &Ctx, log: &mut fs::File, chunk: &Path, amount: u64) -> Result<()> {
    let bytes = fs::read(chunk)?;
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
    Ok(())
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
        checked_transfer(
            ctx,
            backend,
            &log_chunk(backend, root, offset, amount, chunk.path()),
            2,
            "Final log fetch interrupted.",
        )?;
        append_chunk(ctx, &mut log, chunk.path(), amount)?;
        offset += amount;
    }
    Ok(())
}

/// Only SSH transport failures are retried, a few times; a remote tar error,
/// a definite not-found or an exhausted budget proceeds straight to destroy.
const DOWNLOAD_RETRY_DELAYS: [f64; 3] = [5.0, 15.0, 30.0];

fn download(
    ctx: &Ctx,
    backend: &dyn RunBackend,
    (root, jobdir): &(String, String),
    outputs: &str,
    local: &Path,
    deadline: Timestamp,
    committed: bool,
) -> Result<()> {
    let archive = tempfile::NamedTempFile::new()?;
    let seconds = (deadline.as_second() - ctx.clock.now().as_second()).max(1) as f64;
    let command = format!(
        "tar -C {} -cf - -- {} & p=$!; while kill -0 $p 2>/dev/null; do date +%s >{}; sleep 5; done; wait $p",
        shell_quote(jobdir),
        shell_quote(&backend.archive_operand(outputs)),
        shell_quote(&format!("{root}/attached"))
    );
    let poll = ctx.polling(seconds, 0.0);
    let message = "Results download failed; instance will still be destroyed.";
    for delay in DOWNLOAD_RETRY_DELAYS
        .iter()
        .copied()
        .map(Some)
        .chain([None])
    {
        let remaining = poll.remaining().max(0.1);
        let cmd = committed
            .then(|| backend.output_archive(outputs, remaining))
            .flatten()
            .unwrap_or_else(|| backend.remote(&command, remaining));
        let result = ctx.transfer(&cmd.stdout_file(archive.path()), 2);
        let transport = matches!(result, Ok(Output { code: 255, .. }));
        let error = match backend.remote_result(ctx, result, message) {
            Ok(_) => break,
            Err(error) => error,
        };
        // A missing instance or output Volume will not come back.
        let transport = transport && !matches!(error, Error::Status(ExitCode::NotFound, _));
        let Some(delay) =
            delay.filter(|delay| transport && ctx.interrupts() < 2 && *delay < poll.remaining())
        else {
            return Err(error);
        };
        ctx.eprintln(&format!("{} Retrying download.", error.text()))?;
        poll.pause_for(delay);
    }
    let bytes = run_files::unpack(ctx, archive.path(), local, Path::new(outputs))?;
    ctx.result("downloaded_bytes", Value::Int(bytes as i64));
    ctx.result(
        "outputs_dir",
        Value::str(local.join(outputs).to_string_lossy()),
    );
    Ok(())
}
