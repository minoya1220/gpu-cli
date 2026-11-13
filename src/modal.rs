//! Modal Sandbox planning and the pinned, private Python helper protocol.
#[cfg(test)]
mod tests;

use std::io::Write;

use jiff::Timestamp;

use crate::cli::RunFlags;
use crate::ctx::Ctx;
use crate::error::{Error, ExitCode, Result};
use crate::process::{Cmd, Input, RunError};
use crate::pyjson::{self, Dict, Value};
use crate::run::{RunBackend, RunPlan};

pub const HELPER: &str = include_str!("../remote/modal_helper.py");
pub const KEEPER: &str = include_str!("../remote/modal_keeper.sh");
const KEEPER_ROOT: &str = "/tmp/gpu-modal-keeper";

fn client_version() -> &'static str {
    HELPER
        .lines()
        .find_map(|line| {
            line.strip_prefix("# dependencies = [\"modal==")?
                .strip_suffix("\"]")
        })
        .expect("bundled helper has an exact Modal dependency pin")
}

// Names and rate keys only; prices always come from this workspace's live rates.
pub const GPUS: &[(&str, &str, i64)] = &[
    ("T4", "t4", 8),
    ("L4", "l4", 8),
    ("A10", "a10g", 4),
    ("L40S", "l40s", 8),
    ("A100", "a100_40gb", 8),
    ("A100-40GB", "a100_40gb", 8),
    ("A100-80GB", "a100_80gb", 8),
    // Multi-GPU limit not documented for this model: fail closed above one.
    ("RTX-PRO-6000", "rtx6000", 1),
    ("H100", "h100", 8),
    ("H100!", "h100", 8),
    ("H200", "h200", 8),
    ("B200", "b200", 8),
    ("B200+", "b200", 8),
    ("B300", "b300", 8),
];

pub fn gpu(name: &str, count: i64) -> Result<(&'static str, &'static str)> {
    let normalized = name.to_uppercase().replace(' ', "-");
    let &(name, key, maximum) = GPUS
        .iter()
        .find(|(name, _, _)| *name == normalized)
        .ok_or_else(|| {
            Error::usage(format!(
                "Unknown Modal GPU {name:?}. Valid GPUs: {}.",
                GPUS.iter().map(|g| g.0).collect::<Vec<_>>().join(", ")
            ))
        })?;
    if count < 1 || count > maximum {
        return Err(Error::usage(format!(
            "Modal {name} supports --num-gpus 1..={maximum}."
        )));
    }
    Ok((name, key))
}

pub fn resources(flags: &RunFlags) -> Result<(f64, f64)> {
    let cpu = flags.cpu.unwrap_or(4.0);
    let memory = flags.memory.unwrap_or(16.0);
    if !cpu.is_finite()
        || cpu <= 0.0
        || !memory.is_finite()
        || memory <= 0.0
        || !(memory * 1024.0).is_finite()
        || memory * 1024.0 > i32::MAX as f64
        || (memory * 1024.0).fract() != 0.0
    {
        return Err(Error::usage("Modal --cpu and --memory must be finite and positive; memory must be a whole number of MiB (specified in GiB)."));
    }
    Ok((cpu, memory))
}

pub fn validate(flags: &RunFlags, seconds: i64) -> Result<()> {
    for (present, flag) in [
        (flags.offer.is_some(), "--offer"),
        (flags.disk.is_some(), "--disk"),
        (flags.reliability.is_some(), "--reliability"),
        (flags.unverified, "--unverified"),
    ] {
        if present {
            return Err(Error::usage(format!(
                "{flag} is not supported with --provider modal."
            )));
        }
    }
    if seconds > 86400 {
        return Err(Error::usage("Modal --expire must be at most 24h."));
    }
    resources(flags)?;
    if let Some(name) = &flags.gpu {
        gpu(name, flags.num_gpus)?;
    }
    crate::launch::check_max_price(flags.max_price)
}

struct Helper(tempfile::NamedTempFile);
impl Helper {
    fn new() -> Result<Self> {
        let mut file = tempfile::Builder::new().suffix(".py").tempfile()?;
        file.write_all(HELPER.as_bytes())?;
        file.flush()?;
        Ok(Self(file))
    }
    fn command(&self, op: &str) -> Cmd {
        Cmd::new(&[
            "uv",
            "run",
            "--quiet",
            "--script",
            &self.0.path().to_string_lossy(),
            op,
        ])
        .timeout(Some(60.0))
    }
    fn call(&self, ctx: &Ctx, op: &str) -> Result<(Value, String)> {
        self.call_command(ctx, &self.command(op), false)
    }
    fn call_command(&self, ctx: &Ctx, cmd: &Cmd, cancellable: bool) -> Result<(Value, String)> {
        for key in [
            "MODAL_TOKEN_ID",
            "MODAL_TOKEN_SECRET",
            "MODAL_OAUTH_REFRESH_TOKEN",
            "MODAL_OAUTH_CLIENT_ID",
            "MODAL_OAUTH_CLIENT_SECRET",
            "MODAL_OAUTH_JWT_KEY",
        ] {
            if let Some(value) = ctx.env.get(key) {
                ctx.protect_secret(value);
            }
        }
        let output = (if cancellable {
            ctx.transfer(cmd, 1)
        } else {
            ctx.run(cmd)
        })
        .map_err(|error| match error {
            RunError::NotFound => {
                Error::status(ExitCode::NotSetup, "Install uv").next("gpu setup --provider modal")
            }
            RunError::TimedOut => Error::status(
                ExitCode::CommandTimeout,
                "Modal helper timed out. Retry gpu setup --provider modal.",
            ),
            _ => Error::msg(ctx.safe_diagnostic(&error.describe("uv"))),
        })?;
        let diagnostic = ctx.safe_diagnostic(&output.stderr).trim().to_string();
        let failed = |message: &str| {
            Error::msg(if diagnostic.is_empty() {
                message.to_string()
            } else {
                format!("{message}\n{diagnostic}")
            })
        };
        // Never expose raw stdout on parse errors: it may contain credentials.
        let value = pyjson::loads(&output.stdout).map_err(|_| {
            failed(if output.code != 0 {
                "Modal helper failed. Retry gpu setup --provider modal."
            } else {
                "Invalid Modal helper response. Update gpu or retry gpu setup --provider modal."
            })
        })?;
        if let Some(error) = value
            .as_dict()
            .and_then(|d| d.get("error"))
            .and_then(Value::as_dict)
        {
            let kind = error
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("Unknown");
            let detail = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Modal helper failed.");
            return Err(crate::provider_errors::modal(
                kind,
                &ctx.safe_diagnostic(detail),
            ));
        }
        if output.code != 0 {
            return Err(failed(
                "Modal helper failed. Retry gpu setup --provider modal.",
            ));
        }
        Ok((value, diagnostic))
    }
}

pub fn check(ctx: &Ctx) -> Result<()> {
    let (reply, diagnostic) = Helper::new()?.call(ctx, "check")?;
    if !diagnostic.is_empty() {
        ctx.eprintln(&diagnostic)?;
    }
    let reply = reply
        .as_dict()
        .ok_or_else(|| Error::msg("Invalid Modal readiness response."))?;
    let workspace = reply
        .get("workspace")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::msg("Modal readiness response has no workspace."))?;
    if reply.get("client_version").and_then(Value::as_str) != Some(client_version()) {
        return Err(Error::msg(
            "Modal client version is incompatible. Update gpu.",
        ));
    }
    for key in ["workspace", "environment", "client_version"] {
        ctx.result(key, reply.get(key).cloned().unwrap_or(Value::Null));
    }
    ctx.result("provider", Value::str("modal"));
    ctx.result("ready", Value::Bool(true));
    ctx.println(&format!(
        "{}  {} (workspace: {})",
        crate::provider::Provider::Modal.label(),
        crate::setup::READY,
        ctx.safe_diagnostic(workspace)
    ))?;
    ctx.println("Next: gpu run --provider modal --gpu T4 --expire 30m --dry-run -- <command>")
}

#[derive(Clone)]
pub struct Rates {
    prices: Dict,
    diagnostic: String,
}
impl Rates {
    fn component(&self, key: &str) -> Result<f64> {
        let number = match self.prices.get(key) {
            Some(Value::Str(s)) => s.parse().ok(),
            Some(Value::Float(v)) => Some(*v),
            Some(Value::Int(v)) => Some(*v as f64),
            _ => None,
        }
        .filter(|v| v.is_finite() && *v >= 0.0);
        number.ok_or_else(|| {
            Error::msg(format!(
                "Modal rates missing a finite nonnegative {key}; no sandbox created."
            ))
        })
    }
    pub fn price(&self, name: &str, count: i64, cpu: f64, memory: f64) -> Result<f64> {
        let (_, key) = gpu(name, count)?;
        let price = self.component(&format!("gpu_hour_cost_{key}"))? * count as f64
            + self.component("cpu_hour_cost_sandbox")? * cpu
            + self.component("mem_gib_hour_cost_sandbox")? * memory;
        if !price.is_finite() {
            return Err(Error::msg("Modal total hourly price is not finite."));
        }
        Ok(price)
    }
}

pub fn rates(ctx: &Ctx) -> Result<Rates> {
    let (value, diagnostic) = Helper::new()?.call(ctx, "rates")?;
    Ok(Rates {
        prices: value
            .as_dict()
            .cloned()
            .ok_or_else(|| Error::msg("Invalid Modal rates response."))?,
        diagnostic,
    })
}

#[derive(Default)]
pub struct ModalBackend {
    pub rates: Option<Rates>,
    helper: Option<Helper>,
    id: String,
}
impl ModalBackend {
    pub fn new(rates: Option<Rates>) -> Self {
        Self {
            rates,
            ..Self::default()
        }
    }
}
impl RunBackend for ModalBackend {
    fn provider(&self) -> crate::provider::Provider {
        crate::provider::Provider::Modal
    }
    fn plan(
        &self,
        ctx: &Ctx,
        flags: &RunFlags,
        _cfg: &Dict,
        deadline: Timestamp,
        label: String,
    ) -> Result<RunPlan> {
        let seconds = deadline.as_second() - ctx.clock.now().as_second();
        validate(flags, seconds)?;
        let (name, _) = gpu(
            flags
                .gpu
                .as_deref()
                .ok_or_else(|| Error::usage("Modal requires --gpu MODEL."))?,
            flags.num_gpus,
        )?;
        let (cpu, memory) = resources(flags)?;
        let rates = self.rates.clone().map_or_else(|| rates(ctx), Ok)?;
        if !rates.diagnostic.is_empty() {
            ctx.eprintln(&rates.diagnostic)?;
        }
        let price = rates.price(name, flags.num_gpus, cpu, memory)?;
        if flags.max_price.is_some_and(|cap| price > cap) {
            return Err(Error::status(
                ExitCode::OverMaxPrice,
                "Modal total compute price exceeds --max-price; no sandbox created.",
            ));
        }
        let mut request = Dict::new();
        request.insert(
            "image",
            flags.image.as_ref().map_or(Value::Null, Value::str),
        );
        request.insert(
            "gpu",
            Value::str(if flags.num_gpus == 1 {
                name.into()
            } else {
                format!("{name}:{}", flags.num_gpus)
            }),
        );
        request.insert("cpu", Value::Float(cpu));
        request.insert("memory", Value::Int((memory * 1024.0) as i64));
        request.insert("timeout", Value::Int(seconds));
        request.insert(
            "command",
            Value::List(
                [
                    "bash".into(),
                    "-c".into(),
                    KEEPER.into(),
                    "gpu-modal-keeper".into(),
                    KEEPER_ROOT.into(),
                    deadline.as_second().to_string(),
                    "300".into(),
                ]
                .into_iter()
                .map(Value::str)
                .collect(),
            ),
        );
        let mut tags = Dict::new();
        tags.insert("gpu-cli", Value::str("true"));
        tags.insert("label", Value::str(label));
        request.insert("tags", Value::Dict(tags));
        request.insert(
            "env_names",
            Value::List(flags.env.iter().map(Value::str).collect()),
        );
        Ok(RunPlan {
            gpu: Value::str(name),
            rate: price,
            request: Ok(Value::Dict(request)),
        })
    }
    fn launch(
        &mut self,
        ctx: &Ctx,
        request: &Value,
        deadline: Timestamp,
        timeout: f64,
        created: &mut dyn FnMut(&str, Option<&str>) -> Result<()>,
    ) -> Result<String> {
        self.helper = Some(Helper::new()?);
        let helper = self.helper.as_ref().unwrap();
        // A unique reconciliation key exists before any paid request. No job or
        // secret is put in Modal metadata. Never retry an ambiguous create.
        let nonce: String = helper
            .0
            .path()
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        let run_id = format!(
            "{}-modal-{nonce}",
            ctx.clock.now().strftime("%Y%m%dT%H%M%SZ")
        );
        let mut spec = request
            .as_dict()
            .cloned()
            .ok_or_else(|| Error::msg("Invalid Modal create spec."))?;
        let remaining = deadline.as_second() - ctx.clock.now().as_second();
        if remaining <= 600 {
            return Err(Error::usage("Not enough time remains before expiry."));
        }
        spec.insert("timeout", Value::Int(remaining));
        let mut tags = spec
            .get("tags")
            .and_then(Value::as_dict)
            .cloned()
            .unwrap_or_default();
        tags.insert("run_id", Value::str(&run_id));
        tags.insert("gpu", spec.get("gpu").cloned().unwrap_or(Value::Null));
        spec.insert("tags", Value::Dict(tags));
        ctx.result("run_id", Value::str(&run_id));
        ctx.result("created", Value::Null);
        ctx.set_interrupt_notice(Some(format!("Modal create may be pending (run_id {run_id}); hard deadline {deadline}. Check: gpu ls --provider modal")));
        let wait = timeout.min((remaining - 600) as f64);
        let mut cmd = helper.command("create");
        cmd.argv.push(crate::pyfmt::float_repr(wait));
        cmd = cmd
            .timeout(Some(wait + 30.0))
            .stdin(Input::Text(pyjson::dumps(&Value::Dict(spec), None)));
        let reply = helper.call_command(ctx, &cmd, true).and_then(|(reply, _)| {
            let raw = reply
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::msg("Modal create response has no sandbox ID."))?;
            sandbox_id(&format!("modal:{raw}"))?;
            Ok(format!("modal:{raw}"))
        });
        let id = match reply {
            Ok(id) => id,
            Err(error) => {
                let mut cleanup = helper.command("terminate");
                cleanup
                    .argv
                    .extend(["--tag".into(), format!("run_id={run_id}")]);
                if !confirmed(helper.call_command(ctx, &cleanup, false)) {
                    return Err(cleanup_error(&format!("run_id={run_id}")));
                }
                ctx.result("destroyed", Value::Bool(true));
                ctx.set_interrupt_notice(None);
                return if ctx.interrupts() > 0 {
                    Err(Error::Cancelled(130))
                } else {
                    Err(error)
                };
            }
        };
        self.id = id.clone();
        ctx.result("id", Value::str(&id));
        ctx.result("created", Value::Bool(true));
        ctx.result("expiry_armed", Value::Bool(true));
        ctx.set_interrupt_notice(Some(format!("Sandbox {id} may still be running; hard deadline {deadline}. Next: gpu down {id} --yes")));
        created(&id, Some(&run_id))?;
        ctx.check_cancelled()?;
        Ok(id)
    }
    fn remote(&self, command: &str, timeout: f64) -> Cmd {
        let mut cmd = self
            .helper
            .as_ref()
            .expect("launched Modal helper")
            .command("exec");
        cmd.argv.extend([
            self.id.trim_start_matches("modal:").into(),
            "--".into(),
            "bash".into(),
            "-c".into(),
            command.into(),
        ]);
        cmd.timeout(Some(timeout.max(0.1))).raw_output()
    }
    fn remote_result(
        &self,
        ctx: &Ctx,
        result: std::result::Result<crate::process::Output, RunError>,
        message: &str,
    ) -> Result<crate::process::Output> {
        match &result {
            Err(RunError::TimedOut) => {
                return Err(Error::status(
                    ExitCode::CommandTimeout,
                    format!("{message} Modal helper timed out."),
                ))
            }
            Err(RunError::NotFound) => {
                return Err(Error::status(ExitCode::NotSetup, "Install uv")
                    .next("gpu setup --provider modal"))
            }
            Ok(output) if output.code == 255 => {
                if let Some(error) = pyjson::loads(&output.stderr)
                    .ok()
                    .and_then(|reply| reply.get("error").cloned())
                {
                    if let Some(kind) = error.get("kind").and_then(Value::as_str) {
                        return Err(crate::provider_errors::modal(
                            kind,
                            &ctx.safe_diagnostic(
                                error
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("Modal exec failed."),
                            ),
                        ));
                    }
                }
            }
            _ => {}
        }
        crate::run::step_result(ctx, result, message)
    }
    fn lost(&self, ctx: &Ctx, id: &str) -> bool {
        match status(ctx, id) {
            Ok(reply) => reply.get("running") == Some(&Value::Bool(false)),
            Err(Error::Status(ExitCode::NotFound, _)) => true,
            _ => false,
        }
    }
    fn restore_expiry(&self, ctx: &Ctx, _id: &str, deadline: Timestamp) -> Result<()> {
        // Usually the supervisor is still attached and has not fired its hook.
        // If we reconnected during linger, extend the keeper's early-stop time
        // for the download too. Never extend Modal's original server timeout.
        let command = format!("printf '%s\\n' {} >{KEEPER_ROOT}/restore.tmp && mv -f {KEEPER_ROOT}/restore.tmp {KEEPER_ROOT}/end", deadline.as_second());
        self.remote_result(
            ctx,
            ctx.run(&self.remote(&command, 5.0)),
            "Could not restore Modal download deadline.",
        )
        .map(|_| ())
    }
    fn upload(&self, extract: String) -> String {
        upload(KEEPER_ROOT, &extract)
    }
    fn start_prelude(&self) -> String {
        // Mark a job as started only after the supervisor has published its
        // status. A failed start must still be covered by the keeper's grace.
        format!(
            "mkdir -p {KEEPER_ROOT}\ntrap '[ \"$?\" != 0 ] || touch {KEEPER_ROOT}/started' EXIT\n"
        )
    }
    fn hook(&self, _id: &str) -> String {
        format!("#!/bin/bash\nprintf '%s\\n' \"$1\" >{KEEPER_ROOT}/end.tmp && mv -f {KEEPER_ROOT}/end.tmp {KEEPER_ROOT}/end\n")
    }
    fn wait_for_startup(&self) -> bool {
        false
    }
    fn destroy(&self, ctx: &Ctx, id: &str) -> Result<()> {
        terminate(ctx, id)
    }
}

/// Refresh the keeper's start grace only as archive records arrive, so a slow
/// but progressing upload finishes while a stalled one (dead CLI) does not.
/// GNU tar runs the checkpoint action; other tars keep the fixed grace.
fn upload(root: &str, extract: &str) -> String {
    format!(
        "mkdir -p {root} && printf '%s\\n' '#!/bin/sh' 'date +%s >{root}/upload.tmp && mv -f {root}/upload.tmp {root}/upload' >{root}/beat && chmod 700 {root}/beat || exit; case $(tar --version 2>/dev/null) in *'GNU tar'*) export TAR_OPTIONS='--checkpoint=100 --checkpoint-action=exec={root}/beat';; esac; {extract}"
    )
}

/// Qualified IDs are deliberately narrow: never pass malformed IDs to another
/// provider or interpolate them into a shell command.
pub fn sandbox_id(id: &str) -> Result<&str> {
    id.strip_prefix("modal:")
        .filter(|raw| {
            raw.starts_with("sb-")
                && raw.len() > 3
                && raw
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        })
        .ok_or_else(|| Error::usage("Invalid Modal sandbox ID").next("gpu ls --provider modal"))
}

fn confirmed(reply: Result<(Value, String)>) -> bool {
    reply.is_ok_and(|(reply, _)| reply.get("finished") == Some(&Value::Bool(true)))
}
fn cleanup_error(id: &str) -> Error {
    let command = if sandbox_id(id).is_ok() {
        format!("gpu down {id} --yes")
    } else {
        "gpu ls --provider modal --json, then gpu down modal:sb-<id> --yes".into()
    };
    Error::status(
        ExitCode::CleanupUnverified,
        format!("Sandbox {id}: termination unverified; billing may continue"),
    )
    .next(command)
}
pub fn status(ctx: &Ctx, id: &str) -> Result<Value> {
    let raw = sandbox_id(id)?;
    let helper = Helper::new()?;
    let mut cmd = helper.command("status").timeout(Some(15.0));
    cmd.argv.push(raw.into());
    helper
        .call_command(ctx, &cmd, false)
        .map(|(value, _)| value)
}
pub fn terminate(ctx: &Ctx, id: &str) -> Result<()> {
    let raw = sandbox_id(id)?;
    let helper = Helper::new()?;
    // Always send termination even if inventory/status is stale. Only poll()
    // confirmation (or a definite NotFound) proves that billing has stopped.
    for _ in 0..3 {
        let mut cmd = helper.command("terminate").timeout(Some(20.0));
        cmd.argv.push(raw.into());
        match helper.call_command(ctx, &cmd, false) {
            Ok((reply, _)) if reply.get("finished") == Some(&Value::Bool(true)) => return Ok(()),
            Err(Error::Status(ExitCode::NotFound, _)) => return Ok(()),
            _ => {}
        }
        match status(ctx, id) {
            Ok(reply) if reply.get("running") == Some(&Value::Bool(false)) => return Ok(()),
            Err(Error::Status(ExitCode::NotFound, _)) => return Ok(()),
            _ => {}
        }
    }
    Err(cleanup_error(id))
}
pub fn down(ctx: &Ctx, id: &str, yes: bool) -> Result<()> {
    sandbox_id(id)?;
    ctx.result("provider", Value::str("modal"));
    ctx.result("id", Value::str(id));
    ctx.result("destroyed", Value::Bool(false));
    // A missing sandbox requested by the user is exit 16; absence after our
    // own termination request, in contrast, verifies successful cleanup.
    status(ctx, id)?;
    if !yes && !crate::prompt::confirm(ctx, Some(&format!("Terminate sandbox {id}?")))? {
        return Err(Error::Cancelled(130));
    }
    terminate(ctx, id)?;
    ctx.result("destroyed", Value::Bool(true));
    ctx.println(&format!("Sandbox {id} terminated (verified)."))
}

/// Detect only local auth *presence*, never read or print its contents. This
/// keeps ordinary Vast/GCP inventory independent of an absent Modal install.
pub fn configured(ctx: &Ctx) -> bool {
    [
        "MODAL_TOKEN_ID",
        "MODAL_TOKEN_SECRET",
        "MODAL_OAUTH_REFRESH_TOKEN",
        "MODAL_OAUTH_CLIENT_ID",
        "MODAL_OAUTH_JWT_KEY",
    ]
    .iter()
    .any(|key| ctx.env.get(key).is_some_and(|value| !value.is_empty()))
        || ctx
            .env
            .get("MODAL_CONFIG_PATH")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.env.home().join(".modal.toml"))
            .is_file()
}
pub fn list(ctx: &Ctx) -> Result<Vec<Dict>> {
    let (reply, _) = Helper::new()?.call(ctx, "list")?;
    let rows = reply
        .get("sandboxes")
        .and_then(Value::as_list)
        .ok_or_else(|| Error::msg("Invalid Modal inventory response."))?;
    rows.iter()
        .map(|row| {
            let raw = row
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::msg("Modal inventory row has no ID."))?;
            let id = format!("modal:{raw}");
            sandbox_id(&id)?;
            let mut item = Dict::new();
            item.insert("id", Value::str(id));
            item.insert("provider", Value::str("modal"));
            item.insert("actual_status", Value::str("running"));
            if let Some(tags) = row.get("tags").and_then(Value::as_dict) {
                for (from, to) in [
                    ("label", "label"),
                    ("gpu", "gpu_name"),
                    ("run_id", "run_id"),
                ] {
                    if let Some(value) = tags.get(from) {
                        item.insert(to, value.clone());
                    }
                }
            }
            Ok(item)
        })
        .collect()
}

pub fn unsupported() -> Error {
    Error::usage("Modal supports gpu run, gpu ls and gpu down only").next("gpu ls --provider modal")
}
