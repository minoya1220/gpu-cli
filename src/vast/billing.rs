//! What a destroyed Vast instance cost, read from its own record before the
//! destroy request (the record may be gone afterwards). Vast's instance list
//! carries no per-instance charge total, so the figure is the instance's own
//! hourly price times its own runtime, and is always labelled an estimate.
//! A stopped instance bills only storage for its stopped time, so no figure
//! is given unless the instance is running.

use jiff::Timestamp;

use crate::ctx::Ctx;
use crate::pyfmt::{fixed, py_float};
use crate::pyjson::{Dict, Value};

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

/// Estimate from `dph_total` x runtime since `start_date` (epoch seconds).
/// `None` unless the instance is running and both fields are present.
pub(crate) fn estimate(instance: &Dict, now: Timestamp) -> Option<Billed> {
    if instance.get("actual_status").and_then(Value::as_str) != Some("running") {
        return None;
    }
    let price = number(instance, "dph_total")?;
    let runtime_seconds = number(instance, "start_date")
        .filter(|s| *s > 0.0)
        .map(|s| now.as_second() as f64 - s)
        .filter(|s| *s >= 0.0)?;
    Some(Billed {
        amount: price * runtime_seconds / 3600.0,
        runtime_seconds,
    })
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
    fn estimate_needs_start_date_and_dph_total() {
        let now: Timestamp = "2026-01-01T03:12:00Z".parse().unwrap();
        let start = now.as_second() - 3 * 3600 - 12 * 60;
        let running = |fields: &str| dict(&format!(r#"{{"actual_status": "running", {fields}}}"#));
        let billed = estimate(
            &running(&format!(r#""dph_total": 0.325, "start_date": {start}.5"#)),
            now,
        )
        .unwrap();
        assert_eq!(billed.runtime_seconds, 11520.0 - 0.5);
        assert!((billed.amount - 0.325 * 11519.5 / 3600.0).abs() < 1e-9);
        let start = format!(r#""start_date": {start}"#);
        for fields in [
            r#""dph_total": 0.36, "duration": 7200"#.to_string(),
            format!(r#""dph": 0.36, {start}"#),
            format!(r#""dph_total": true, {start}"#),
            r#""dph_total": 0.36, "start_date": 0"#.to_string(),
            format!(
                r#""dph_total": 0.36, "start_date": {}"#,
                now.as_second() + 60
            ),
        ] {
            assert!(estimate(&running(&fields), now).is_none(), "{fields}");
        }
    }

    #[test]
    fn no_estimate_unless_running() {
        let now: Timestamp = "2026-01-01T03:12:00Z".parse().unwrap();
        for status in [
            r#""actual_status": "stopped", "#,
            r#""actual_status": "exited", "#,
            "",
        ] {
            let item = dict(&format!(
                r#"{{{status}"dph_total": 0.36, "start_date": 1}}"#
            ));
            assert!(estimate(&item, now).is_none());
        }
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
