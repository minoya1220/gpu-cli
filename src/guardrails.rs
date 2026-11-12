//! Local launch validation, before any provider request.
use jiff::Timestamp;

use crate::cli::{Command, UpFlags};
use crate::ctx::Ctx;
use crate::error::{Error, ExitCode, Result};
use crate::output::usage;
use crate::pyjson::{Dict, Value};

pub(crate) fn defaults(flags: &mut UpFlags, cfg: &Dict) -> Result<()> {
    if flags.expire.is_none() {
        match cfg.get("default_expire") {
            None | Some(Value::Null) => {}
            Some(Value::Str(s)) if !s.is_empty() => flags.expire = Some(s.clone()),
            _ => {
                return Err(usage(
                    "Set default_expire to a duration string, e.g. \"2h\".",
                ))
            }
        }
    }
    Ok(())
}

pub(crate) fn nonterminal(ctx: &Ctx, command: &Command, cfg: &Dict) -> Result<()> {
    if ctx.interactive() {
        return Ok(());
    }
    match command {
        Command::Up(flags) => {
            let mut missing = Vec::new();
            if flags.expire.as_deref().is_none_or(str::is_empty) {
                missing.push("Non-terminal gpu up requires --expire DURATION (e.g. --expire 2h), or default_expire in config.json.");
            }
            let tpu = flags.provider.as_deref() == Some("gcp")
                || flags.tpu.is_some()
                || flags.zone.is_some()
                || flags.runtime.is_some()
                || flags.spot
                || flags.on_demand;
            if if tpu {
                flags.tpu.is_none()
            } else {
                flags.gpu.is_none()
            } {
                missing.push("Choose hardware: pass --gpu MODEL or --tpu TYPE.");
            }
            if flags.repo.is_none() && !flags.no_repo {
                missing.push("Choose a repository: pass --repo URL or --no-repo.");
            }
            if !tpu && flags.mode.is_none() {
                missing.push("Choose a mode: pass --mode container or --mode vm.");
            }
            if tpu
                && flags.zone.is_none()
                && cfg
                    .get("gcp")
                    .and_then(Value::as_dict)
                    .and_then(|d| d.get("zone"))
                    .is_none()
            {
                missing.push("Choose a TPU zone: pass --zone ZONE (or configure gcp.zone).");
            }
            if !tpu && flags.dry_run && !flags.yes && flags.offer.is_none() {
                missing.push("Choose an offer: pass --yes (cheapest) or --offer ID.");
            }
            if !flags.yes && !flags.dry_run {
                missing.push("Non-terminal paid launch requires --yes; use --dry-run to inspect without renting.");
            }
            if !missing.is_empty() {
                let example = if tpu {
                    "gpu up --tpu TYPE --zone ZONE --no-repo --expire 2h --yes"
                } else {
                    "gpu up --gpu MODEL --mode container --no-repo --expire 2h --yes"
                };
                return Err(usage(format!("{}\nExample: {example}", missing.join("\n"))));
            }
        }
        Command::Down { yes: false, .. } => {
            return Err(usage(
                "Non-terminal destruction requires --yes. Next: gpu down ID --yes",
            ))
        }
        Command::Setup(flags) if flags.provider.as_deref() == Some("gcp") => {
            if !flags.remove && flags.project.is_none() {
                return Err(usage("Choose a project: pass --project PROJECT. Next: gpu setup --provider gcp --help"));
            }
            if !flags.dry_run && flags.confirm_project.is_none() {
                return Err(usage("Non-terminal setup requires --confirm-project PROJECT. Next: gpu setup --provider gcp --help"));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Opt-in per-launch compute estimate, not a billing-system limit. Refuse
/// unknown rates/deadlines rather than pretending to enforce a cap.
pub(crate) fn spending(
    ctx: &Ctx,
    cfg: &Dict,
    rate: Option<f64>,
    deadline: Option<Timestamp>,
) -> Result<()> {
    let Some(cap) = cfg.get("max_session_spend") else {
        return Ok(());
    };
    let cap = match cap {
        Value::Float(v) => *v,
        Value::Int(v) => *v as f64,
        _ => return Err(usage("max_session_spend must be a positive dollar amount.")),
    };
    if !cap.is_finite() || cap <= 0.0 {
        return Err(usage("max_session_spend must be a positive dollar amount."));
    }
    let error = || {
        Error::status(ExitCode::SpendLimit, "Cannot satisfy max_session_spend. Pass --expire, choose cheaper hardware/a shorter duration, or change \
            the opt-in cap in config.json.")
    };
    let (Some(rate), Some(deadline)) = (rate, deadline) else {
        return Err(error());
    };
    let seconds = (deadline.as_second() - ctx.clock.now().as_second()).max(0) as f64;
    let estimate = rate * seconds / 3600.0;
    if !rate.is_finite() || rate < 0.0 || !estimate.is_finite() || estimate > cap {
        return Err(error());
    }
    ctx.result("estimated_spend", Value::Float(estimate));
    Ok(())
}
