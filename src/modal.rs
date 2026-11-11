//! Modal Sandbox planning and the pinned, private Python helper protocol.
#[cfg(test)]
#[path = "modal_tests.rs"]
mod tests;

use std::io::Write;

use jiff::Timestamp;

use crate::cli::RunFlags;
use crate::ctx::Ctx;
use crate::error::{Error, ExitCode, Result};
use crate::process::{Cmd, RunError};
use crate::pyjson::{self, Dict, Value};
use crate::run::{RunBackend, RunPlan};

pub const HELPER: &str = include_str!("../remote/modal_helper.py");
pub const NOT_LIVE: &str = "Live Modal runs are not yet supported. Use gpu run --provider modal --dry-run to inspect a plan.";

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
            Error::Usage(format!(
                "Unknown Modal GPU {name:?}. Valid GPUs: {}.",
                GPUS.iter().map(|g| g.0).collect::<Vec<_>>().join(", ")
            ))
        })?;
    if count < 1 || count > maximum {
        return Err(Error::Usage(format!(
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
        return Err(Error::Usage("Modal --cpu and --memory must be finite and positive; memory must be a whole number of MiB (specified in GiB).".into()));
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
            return Err(Error::Usage(format!(
                "{flag} is not supported with --provider modal."
            )));
        }
    }
    if seconds > 86400 {
        return Err(Error::Usage("Modal --expire must be at most 24h.".into()));
    }
    resources(flags)?;
    if let Some(name) = &flags.gpu {
        gpu(name, flags.num_gpus)?;
    }
    if flags.max_price.is_some_and(|v| !v.is_finite() || v <= 0.0) {
        return Err(Error::Usage(
            "--max-price must be finite and greater than zero.".into(),
        ));
    }
    Ok(())
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
    fn call(&self, ctx: &Ctx, op: &str) -> Result<Value> {
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
        let output = ctx.run(&self.command(op)).map_err(|error| match error {
            RunError::NotFound => Error::status(
                ExitCode::NotSetup,
                "Install uv. Next: gpu setup --provider modal",
            ),
            RunError::TimedOut => Error::status(
                ExitCode::CommandTimeout,
                "Modal helper timed out. Retry gpu setup --provider modal.",
            ),
            _ => Error::msg(ctx.safe_diagnostic(&error.python_text("uv"))),
        })?;
        let diagnostic = ctx.safe_diagnostic(&output.stderr);
        if !diagnostic.trim().is_empty() {
            ctx.eprintln(&diagnostic)?;
        }
        // Never expose raw stdout on parse errors: it may contain credentials.
        let value = pyjson::loads(&output.stdout).map_err(|_| {
            Error::msg(
                "Invalid Modal helper response. Update gpu or retry gpu setup --provider modal.",
            )
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
            return Err(Error::msg(
                "Modal helper failed. Retry gpu setup --provider modal.",
            ));
        }
        Ok(value)
    }
}

pub fn check(ctx: &Ctx) -> Result<()> {
    let reply = Helper::new()?.call(ctx, "check")?;
    let reply = reply
        .as_dict()
        .ok_or_else(|| Error::msg("Invalid Modal readiness response."))?;
    let workspace = reply
        .get("workspace")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::msg("Modal readiness response has no workspace."))?;
    if reply.get("client_version").and_then(Value::as_str) != Some("1.6.3") {
        return Err(Error::msg(
            "Modal client version is incompatible. Update gpu.",
        ));
    }
    for key in ["workspace", "environment", "client_version"] {
        ctx.result(key, reply.get(key).cloned().unwrap_or(Value::Null));
    }
    ctx.result("provider", Value::str("modal"));
    ctx.result("ready", Value::Bool(true));
    ctx.println(&format!("Modal  ✓ set up (workspace: {})\nNext: gpu run --provider modal --gpu T4 --expire 30m --dry-run -- <command>", ctx.safe_diagnostic(workspace)))
}

#[derive(Clone)]
pub struct Rates(Dict);
impl Rates {
    fn component(&self, key: &str) -> Result<f64> {
        let number = match self.0.get(key) {
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
    let value = Helper::new()?.call(ctx, "rates")?;
    Ok(Rates(value.as_dict().cloned().ok_or_else(|| {
        Error::msg("Invalid Modal rates response.")
    })?))
}

pub struct ModalBackend {
    pub rates: Option<Rates>,
}
impl RunBackend for ModalBackend {
    fn provider(&self) -> &'static str {
        "modal"
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
                .ok_or_else(|| Error::Usage("Modal requires --gpu MODEL.".into()))?,
            flags.num_gpus,
        )?;
        let (cpu, memory) = resources(flags)?;
        let rates = self.rates.clone().map_or_else(|| rates(ctx), Ok)?;
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
        // Preview entrypoint only. The live backend will replace this with the
        // keeper before enabling launch; no shared-run code can create yet.
        request.insert(
            "command",
            Value::List(vec![Value::str("sleep"), Value::str("infinity")]),
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
        _ctx: &Ctx,
        _request: &Value,
        _deadline: Timestamp,
        _timeout: f64,
        _created: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<String> {
        Err(Error::Usage(NOT_LIVE.into()))
    }
    fn remote(&self, _command: &str, _timeout: f64) -> Cmd {
        unreachable!("Modal live transport is not enabled")
    }
    fn lost(&self, _ctx: &Ctx, _id: &str) -> bool {
        false
    }
    fn restore_expiry(&self, _ctx: &Ctx, _id: &str, _deadline: Timestamp) -> Result<()> {
        Err(Error::Usage(NOT_LIVE.into()))
    }
    fn hook(&self, _id: &str) -> String {
        String::new()
    }
    fn wait_for_startup(&self) -> bool {
        false
    }
    fn destroy(&self, _ctx: &Ctx, _id: &str) -> Result<()> {
        Err(Error::Usage(NOT_LIVE.into()))
    }
}
