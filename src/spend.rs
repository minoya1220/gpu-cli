//! `spend.json`: the estimated cost of every launch in the last 24 hours, for
//! the opt-in `max_daily_spend` cap. Each launch is recorded at its estimate
//! to its deadline; a destroy that knows the billed amount replaces it.

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

/// Record a started launch at its estimate and prune entries past the window.
pub(crate) fn record(ctx: &Ctx, key: &str, estimate: f64) -> Result<()> {
    let now = ctx.clock.now().as_second();
    crate::atomic_file::update(&ledger_path(&ctx.env), |original| {
        let mut entries = parse(&ctx.env, original)?;
        entries.retain(|e| in_window(e, now) && e.key != key);
        entries.push(Entry {
            key: key.to_string(),
            at: now,
            usd: estimate,
        });
        Ok(dump(&entries))
    })
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

/// Ledger writes never turn a successful paid operation into a failure.
pub(crate) fn record_best_effort(ctx: &Ctx, key: &str, estimate: Option<f64>) {
    let Some(estimate) = estimate else { return };
    if let Err(error) = record(ctx, key, estimate) {
        let _ = ctx.eprintln(&format!(
            "Warning: could not record launch spend: {}",
            crate::redact::redact_text(error.text())
        ));
    }
}

pub(crate) fn settle_best_effort(ctx: &Ctx, key: &str, billed: f64) {
    if let Err(error) = settle(ctx, key, billed) {
        let _ = ctx.eprintln(&format!(
            "Warning: could not update launch spend: {}",
            crate::redact::redact_text(error.text())
        ));
    }
}

#[cfg(test)]
mod tests;
