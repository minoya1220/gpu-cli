//! Local launch validation, before any provider request.
use jiff::Timestamp;

use crate::cli::{Command, UpFlags};
use crate::ctx::Ctx;
use crate::error::{Error, ExitCode, Result};
use crate::pyjson::{Dict, Value};

pub(crate) fn defaults(flags: &mut UpFlags, cfg: &Dict) -> Result<()> {
    if flags.expire.is_none() {
        match cfg.get("default_expire") {
            None | Some(Value::Null) => {}
            Some(Value::Str(s)) if !s.is_empty() => flags.expire = Some(s.clone()),
            _ => {
                return Err(Error::usage(
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
            let amd = flags.target().amd();
            if !amd && flags.expire.as_deref().is_none_or(str::is_empty) {
                missing.push("Non-terminal gpu up requires --expire DURATION (e.g. --expire 2h), or default_expire in config.json.");
            }
            let tpu = flags.target() == crate::provider::Provider::Gcp;
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
            if !tpu && !amd && flags.mode.is_none() {
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
            if !tpu && !amd && flags.dry_run && !flags.yes && flags.offer.is_none() {
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
                return Err(Error::usage(format!(
                    "{}\nExample: {example}",
                    missing.join("\n")
                )));
            }
        }
        Command::Down { yes: false, .. } => {
            return Err(
                Error::usage("Non-terminal destruction requires --yes").next("gpu down ID --yes")
            )
        }
        Command::Setup(flags) if flags.provider == Some(crate::provider::Provider::Gcp) => {
            if !flags.remove && flags.project.is_none() {
                return Err(Error::usage("Choose a project: pass --project PROJECT")
                    .next("gpu setup --provider gcp --help"));
            }
            if !flags.dry_run && flags.confirm_project.is_none() {
                return Err(
                    Error::usage("Non-terminal setup requires --confirm-project PROJECT")
                        .next("gpu setup --provider gcp --help"),
                );
            }
        }
        _ => {}
    }
    Ok(())
}

fn cap_setting(cfg: &Dict, key: &str) -> Result<Option<f64>> {
    let Some(cap) = cfg.get(key) else {
        return Ok(None);
    };
    let cap = match cap {
        Value::Float(v) => *v,
        Value::Int(v) => *v as f64,
        _ => {
            return Err(Error::usage(format!(
                "{key} must be a positive dollar amount."
            )))
        }
    };
    if !cap.is_finite() || cap <= 0.0 {
        return Err(Error::usage(format!(
            "{key} must be a positive dollar amount."
        )));
    }
    Ok(Some(cap))
}

fn dollars(amount: f64) -> String {
    crate::vast::billing::format_amount(amount)
}

/// Opt-in compute estimate caps, not billing-system limits: `max_session_spend`
/// bounds this launch, `max_daily_spend` this launch plus every launch started
/// in the last 24 hours. Refuse unknown rates/deadlines rather than pretending
/// to enforce a cap. Returns the launch's estimate when `max_daily_spend` is
/// set, for the caller to record in the ledger once the launch exists.
pub(crate) fn spending(
    ctx: &Ctx,
    cfg: &Dict,
    rate: Option<f64>,
    deadline: Option<Timestamp>,
) -> Result<Option<f64>> {
    let session = cap_setting(cfg, "max_session_spend")?;
    let daily = cap_setting(cfg, "max_daily_spend")?;
    if session.is_none() && daily.is_none() {
        return Ok(None);
    }
    let estimate = match (rate, deadline) {
        (Some(rate), Some(deadline)) => {
            let seconds = (deadline.as_second() - ctx.clock.now().as_second()).max(0) as f64;
            let estimate = rate * seconds / 3600.0;
            (rate.is_finite() && rate >= 0.0 && estimate.is_finite()).then_some(estimate)
        }
        _ => None,
    };
    if let Some(cap) = session {
        match estimate {
            Some(estimate) if estimate <= cap => ctx.result("estimated_spend", Value::Float(estimate)),
            _ => {
                return Err(Error::status(ExitCode::SpendLimit, "Cannot satisfy max_session_spend. Pass --expire, choose cheaper hardware/a shorter duration, or change \
            the opt-in cap in config.json."))
            }
        }
    }
    let Some(cap) = daily else {
        return Ok(None);
    };
    let Some(estimate) = estimate else {
        return Err(Error::status(ExitCode::SpendLimit, "Cannot satisfy max_daily_spend. Pass --expire, choose hardware with a known price, or change \
            the opt-in cap in config.json."));
    };
    let spent = crate::spend::window_total(ctx);
    ctx.result("daily_spent_usd", Value::Float(spent));
    ctx.result("daily_cap_usd", Value::Float(cap));
    ctx.result("daily_remaining_usd", Value::Float((cap - spent).max(0.0)));
    if spent + estimate > cap {
        return Err(Error::status(
            ExitCode::SpendLimit,
            format!(
                "Cannot satisfy max_daily_spend: {} estimated spent in the last 24h + {} this launch exceeds the {} cap. \
                 Wait for earlier launches to leave the window, choose cheaper hardware/a shorter duration, or change the opt-in cap in config.json.",
                dollars(spent),
                dollars(estimate),
                dollars(cap)
            ),
        ));
    }
    Ok(Some(estimate))
}

/// Dry-run line for `max_daily_spend`: the window total and what is left.
/// Records nothing.
pub(crate) fn dry_run_budget(ctx: &Ctx, cfg: &Dict) -> Result<()> {
    let Some(cap) = cap_setting(cfg, "max_daily_spend")? else {
        return Ok(());
    };
    let spent = crate::spend::window_total(ctx);
    ctx.println(&format!(
        "Daily budget: {} estimated spent in the last 24h of {}; {} remaining.",
        dollars(spent),
        dollars(cap),
        dollars((cap - spent).max(0.0))
    ))
}
