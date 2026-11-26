//! What a destroyed Vast instance cost, read from its own record before the
//! destroy request (the record may be gone afterwards). Vast's instance list
//! carries no per-instance charge total, so the figure is the instance's own
//! hourly price times its own runtime, and is always labelled an estimate.

use jiff::Timestamp;

use crate::ctx::Ctx;
use crate::pyfmt::{fixed, py_float};
use crate::pyjson::{Dict, Value};
use crate::vast::{find_instance, list_instances};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Billed {
    pub amount: f64,
    pub runtime_seconds: f64,
}

fn number(instance: &Dict, key: &str) -> Option<f64> {
    instance
        .get(key)
        .filter(|value| !matches!(value, Value::Bool(_)))
        .and_then(|value| py_float(value).ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
}

/// Estimate from `dph_total` x runtime. Runtime comes from `start_date` (epoch
/// seconds) against `now`, else Vast's own `duration` seconds.
pub(crate) fn estimate(instance: &Dict, now: Timestamp) -> Option<Billed> {
    let price = number(instance, "dph_total").or_else(|| number(instance, "dph"))?;
    let started = number(instance, "start_date")
        .filter(|s| *s > 0.0)
        .map(|s| now.as_second() as f64 - s)
        .filter(|s| *s >= 0.0);
    let runtime_seconds = started.or_else(|| number(instance, "duration"))?;
    Some(Billed {
        amount: price * runtime_seconds / 3600.0,
        runtime_seconds,
    })
}

/// Best effort: any failure just means no figure is shown.
pub(crate) fn lookup(ctx: &Ctx, id: &str) -> Option<Billed> {
    let live = list_instances(ctx, Some(10.0)).ok()?;
    estimate(find_instance(&live, id)?, ctx.clock.now())
}

pub(crate) fn format_runtime(seconds: f64) -> String {
    let total = seconds as u64;
    let (hours, minutes) = (total / 3600, total % 3600 / 60);
    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{total}s")
    }
}

pub(crate) fn format_amount(amount: f64) -> String {
    if amount > 0.0 && amount < 0.005 {
        "<$0.01".into()
    } else {
        format!("${}", fixed(amount, 2))
    }
}

/// Sentence tail such as ` after 3h12m; billed about $1.04 (estimate)`.
pub(crate) fn describe(billed: &Billed) -> String {
    format!(
        " after {}; billed about {} (estimate)",
        format_runtime(billed.runtime_seconds),
        format_amount(billed.amount)
    )
}

pub(crate) fn record(ctx: &Ctx, billed: &Billed) {
    ctx.result("billed_usd", Value::Float(billed.amount));
    ctx.result("billed_estimated", Value::Bool(true));
    ctx.result("runtime_seconds", Value::Float(billed.runtime_seconds));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::dict;

    #[test]
    fn formats_runtime_and_amount() {
        assert_eq!(format_runtime(3.0 * 3600.0 + 12.0 * 60.0 + 5.0), "3h12m");
        assert_eq!(format_runtime(45.0 * 60.0), "45m");
        assert_eq!(format_runtime(30.0), "30s");
        assert_eq!(format_amount(1.0384), "$1.04");
        assert_eq!(format_amount(0.0), "$0.00");
        assert_eq!(format_amount(0.001), "<$0.01");
    }

    #[test]
    fn estimate_uses_start_date_then_duration() {
        let now: Timestamp = "2026-01-01T03:12:00Z".parse().unwrap();
        let start = now.as_second() - 3 * 3600 - 12 * 60;
        let item = dict(&format!(
            r#"{{"dph_total": 0.325, "start_date": {start}.5, "duration": 5}}"#
        ));
        let billed = estimate(&item, now).unwrap();
        assert_eq!(billed.runtime_seconds, 11520.0 - 0.5);
        assert!((billed.amount - 0.325 * 11519.5 / 3600.0).abs() < 1e-9);
        let item = dict(r#"{"dph_total": 0.36, "duration": 7200}"#);
        assert_eq!(estimate(&item, now).unwrap().amount, 0.72);
        assert!(estimate(&dict(r#"{"duration": 7200}"#), now).is_none());
        assert!(estimate(&dict(r#"{"dph_total": 0.3}"#), now).is_none());
        assert!(estimate(&dict(r#"{"dph_total": true, "duration": 1}"#), now).is_none());
    }

    #[test]
    fn describes_an_estimate() {
        let billed = Billed {
            amount: 1.04,
            runtime_seconds: 11520.0,
        };
        assert_eq!(
            describe(&billed),
            " after 3h12m; billed about $1.04 (estimate)"
        );
    }
}
