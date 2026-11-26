//! `spend.json`: the estimated cost of every launch in the last 24 hours, for
//! the opt-in `max_daily_spend` cap. Each launch reserves its estimate to its
//! deadline before its create request; a destroy that knows the billed amount
//! replaces it.

use std::path::PathBuf;

use crate::ctx::{Ctx, Env};
use crate::error::{os_message, Error, ExitCode, Result};
use crate::pyjson::{self, Dict, Value};

pub(crate) const WINDOW_SECONDS: i64 = 24 * 3600;

pub(crate) fn ledger_path(env: &Env) -> PathBuf {
    env.xdg("XDG_STATE_HOME", ".local/state")
        .join("gpu")
        .join("spend.json")
}

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    key: String,
    at: i64,
    usd: f64,
}

fn unreadable(env: &Env, reason: &str) -> Error {
    let path = ledger_path(env);
    Error::status(
        ExitCode::SpendLimit,
        format!(
            "Cannot enforce max_daily_spend: the spend ledger {} {reason}. Fix or remove it, or change the opt-in cap in config.json.",
            path.display()
        ),
    )
}

fn entry(item: &Value) -> Option<Entry> {
    let item = item.as_dict()?;
    let usd = match item.get("usd")? {
        Value::Float(v) => *v,
        Value::Int(v) => *v as f64,
        _ => return None,
    };
    let at = match item.get("at")? {
        Value::Int(v) => *v,
        Value::Float(v) => *v as i64,
        _ => return None,
    };
    let key = item.get("key")?.as_str()?.to_string();
    (usd.is_finite() && usd >= 0.0).then_some(Entry { key, at, usd })
}

/// Empty bytes are a missing ledger; anything else must parse completely, so
/// a damaged ledger is never mistaken for one with less spend in it.
fn parse(env: &Env, bytes: &[u8]) -> Result<Vec<Entry>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let corrupt = || unreadable(env, "is not a valid ledger");
    let text = std::str::from_utf8(bytes).map_err(|_| corrupt())?;
    let Ok(Value::Dict(root)) = pyjson::loads(text) else {
        return Err(corrupt());
    };
    let Some(Value::List(items)) = root.get("launches") else {
        return Err(corrupt());
    };
    items
        .iter()
        .map(|item| entry(item).ok_or_else(corrupt))
        .collect()
}

fn dump(entries: &[Entry]) -> Vec<u8> {
    let launches = entries
        .iter()
        .map(|e| {
            let mut item = Dict::new();
            item.insert("key", Value::str(&e.key));
            item.insert("at", Value::Int(e.at));
            item.insert("usd", Value::Float(e.usd));
            Value::Dict(item)
        })
        .collect();
    let mut root = Dict::new();
    root.insert("launches", Value::List(launches));
    (pyjson::dumps(&Value::Dict(root), Some(2)) + "\n").into_bytes()
}

fn in_window(entry: &Entry, now: i64) -> bool {
    entry.at > now - WINDOW_SECONDS
}

/// Estimated spend of launches started in the last 24 hours. Only a missing
/// ledger counts as empty.
pub(crate) fn window_total(ctx: &Ctx) -> Result<f64> {
    let now = ctx.clock.now().as_second();
    let path = ledger_path(&ctx.env);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            return Err(unreadable(
                &ctx.env,
                &format!("cannot be read ({})", os_message(&e, None)),
            ))
        }
    };
    Ok(parse(&ctx.env, &bytes)?
        .iter()
        .filter(|e| in_window(e, now))
        .fold(0.0, |total, e| total + e.usd))
}

/// Provider-qualified key, so IDs from different providers never collide.
pub(crate) fn key(provider: &str, id: &str) -> String {
    format!("{provider}:{id}")
}

/// A launch's estimate checked against `max_daily_spend`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Budget {
    pub(crate) cap: f64,
    pub(crate) estimate: f64,
}

/// A ledger entry holding a launch's estimate from before its create request.
#[must_use]
#[derive(Debug)]
pub(crate) struct Reservation(String);

/// Recheck the cap and record the estimate under one lock, before anything
/// is rented, so a ledger that cannot be written refuses the launch and
/// concurrent launches cannot both pass. Prunes entries past the window.
pub(crate) fn reserve(ctx: &Ctx, budget: Option<Budget>) -> Result<Option<Reservation>> {
    let Some(Budget { cap, estimate }) = budget else {
        return Ok(None);
    };
    let now = ctx.clock.now().as_second();
    let mut reserved = String::new();
    crate::atomic_file::update(&ledger_path(&ctx.env), |original| {
        let mut entries = parse(&ctx.env, original)?;
        entries.retain(|e| in_window(e, now));
        let spent = entries.iter().fold(0.0, |total, e| total + e.usd);
        if spent + estimate > cap {
            return Err(crate::guardrails::daily_refusal(spent, estimate, cap));
        }
        reserved = (0..)
            .map(|n| format!("pending:{now}:{n}"))
            .find(|k| entries.iter().all(|e| &e.key != k))
            .expect("unused key");
        entries.push(Entry {
            key: reserved.clone(),
            at: now,
            usd: estimate,
        });
        Ok(dump(&entries))
    })
    .map_err(|error| match error {
        Error::Status(ExitCode::SpendLimit, _) => error,
        error => unreadable(
            &ctx.env,
            &format!(
                "cannot be written ({})",
                crate::redact::redact_text(error.text())
            ),
        ),
    })?;
    Ok(Some(Reservation(reserved)))
}

fn rewrite(ctx: &Ctx, change: impl FnOnce(&mut Vec<Entry>)) -> Result<()> {
    crate::atomic_file::update(&ledger_path(&ctx.env), |original| {
        let mut entries = parse(&ctx.env, original)?;
        change(&mut entries);
        Ok(dump(&entries))
    })
}

fn warn(ctx: &Ctx, result: Result<()>) {
    if let Err(error) = result {
        let _ = ctx.eprintln(&format!(
            "Warning: could not update launch spend: {}",
            crate::redact::redact_text(error.text())
        ));
    }
}

/// File a reservation under the launch it became, keeping its time and
/// estimate. A failed write leaves the reservation counted.
pub(crate) fn claim_best_effort(ctx: &Ctx, reservation: Option<Reservation>, key: &str) {
    let Some(Reservation(reserved)) = reservation else {
        return;
    };
    warn(
        ctx,
        rewrite(ctx, |entries| {
            entries.retain(|e| e.key != key);
            for entry in entries.iter_mut().filter(|e| e.key == reserved) {
                entry.key = key.to_string();
            }
        }),
    );
}

/// Drop a reservation once nothing can have been rented.
pub(crate) fn release_best_effort(ctx: &Ctx, reservation: Option<Reservation>) {
    let Some(Reservation(reserved)) = reservation else {
        return;
    };
    warn(
        ctx,
        rewrite(ctx, |entries| entries.retain(|e| e.key != reserved)),
    );
}

/// Replace a launch's estimate with the amount it was billed. An unknown or
/// already pruned launch is left alone.
pub(crate) fn settle(ctx: &Ctx, key: &str, billed: f64) -> Result<()> {
    if !ledger_path(&ctx.env).exists() {
        return Ok(());
    }
    let now = ctx.clock.now().as_second();
    crate::atomic_file::update(&ledger_path(&ctx.env), |original| {
        let mut entries = parse(&ctx.env, original)?;
        entries.retain(|e| in_window(e, now));
        for entry in entries.iter_mut().filter(|e| e.key == key) {
            entry.usd = billed;
        }
        Ok(dump(&entries))
    })
}

pub(crate) fn settle_best_effort(ctx: &Ctx, key: &str, billed: f64) {
    warn(ctx, settle(ctx, key, billed));
}

#[cfg(test)]
mod tests;
