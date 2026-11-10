//! `history.json`: recent picks per step, most recent first.

use std::path::PathBuf;

use crate::ctx::Env;
use crate::error::Result;
use crate::pyjson::{self, Dict, Value};

pub fn history_path(env: &Env) -> PathBuf {
    env.xdg("XDG_STATE_HOME", ".local/state")
        .join("gpu")
        .join("history.json")
}

/// The stored history; a missing, unreadable or invalid file is empty.
pub fn load_history(env: &Env) -> Dict {
    let Ok(text) = std::fs::read_to_string(history_path(env)) else {
        return Dict::new();
    };
    match pyjson::loads(&text) {
        Ok(Value::Dict(dict)) => dict,
        _ => Dict::new(),
    }
}

/// Earlier picks for `step`, most recent first, with duplicates.
pub fn recent_picks(env: &Env, step: &str) -> Vec<String> {
    match load_history(env).get(step) {
        Some(Value::List(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

pub fn record_picks(env: &Env, picks: &[(&str, &str)]) -> Result<()> {
    let path = history_path(env);
    crate::atomic_file::update(&path, |original| {
        let mut history = match pyjson::loads(&String::from_utf8_lossy(original)) {
            Ok(Value::Dict(history)) => history,
            _ => Dict::new(),
        };
        for (step, value) in picks {
            let mut entries = match history.get(step) {
                Some(Value::List(items)) => items.clone(),
                _ => Vec::new(),
            };
            entries.insert(0, Value::str(*value));
            entries.truncate(100);
            history.insert(*step, Value::List(entries));
        }
        let text = pyjson::dumps(&Value::Dict(history), Some(2)) + "\n";
        Ok(text.into_bytes())
    })
}

/// Picker convenience must never turn a successful paid operation into failure.
pub fn record_picks_best_effort(ctx: &crate::ctx::Ctx, picks: &[(&str, &str)]) {
    if let Err(error) = record_picks(&ctx.env, picks) {
        let _ = ctx.eprintln(&format!(
            "Warning: could not save pick history: {}",
            crate::redact::redact_text(error.text())
        ));
    }
}

/// Options picked before, ranked by recency-weighted frequency
/// (`sum(0.85 ** age)`), stable for ties. `key` gives an option's history
/// text, or `None` when it can never match a history entry.
pub fn by_frequent_recency<T: Clone>(
    options: &[T],
    history: &[String],
    key: impl Fn(&T) -> Option<String>,
) -> Vec<T> {
    let mut scored: Vec<(f64, T)> = Vec::new();
    for option in options {
        let score: f64 = match key(option) {
            Some(text) => history
                .iter()
                .enumerate()
                .filter(|(_, item)| **item == text)
                .map(|(age, _)| 0.85f64.powf(age as f64))
                .sum(),
            None => 0.0,
        };
        if score != 0.0 {
            scored.push((score, option.clone()));
        }
    }
    // Stable descending sort, as Python's sorted(..., reverse=True) keeps ties in order.
    stable_sort_by_key(&mut scored, |pair| -pair.0);
    scored.into_iter().map(|(_, value)| value).collect()
}

/// A stable sort that only uses `<`, like Python's `sorted`, so values that
/// do not compare (NaN) never make it panic.
pub fn stable_sort_by_key<T>(items: &mut [T], key: impl Fn(&T) -> f64) {
    for i in 1..items.len() {
        let mut j = i;
        while j > 0 && key(&items[j]) < key(&items[j - 1]) {
            items.swap(j, j - 1);
            j -= 1;
        }
    }
}
