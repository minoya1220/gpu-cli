//! Port of tests/test_uptime.py.

use crate::commands::render_instances;
use crate::instances::{format_uptime, instance_label};
use crate::pyjson::{Dict, Value};
use crate::testing::*;

#[test]
fn test_format_uptime() {
    let cases = [
        ("0", "0s"),
        ("0.9", "0s"),
        ("45", "45s"),
        ("59.99", "59s"),
        ("60", "1m"),
        ("720", "12m"),
        ("3599.99", "59m"),
        ("3600", "1h 00m"),
        ("7342.81", "2h 02m"),
        (r#""7342.81""#, "2h 02m"),
        ("86399", "23h 59m"),
        ("86400", "1d 00h"),
        ("273600", "3d 04h"),
        ("null", "-"),
        (r#""""#, "-"),
        (r#""garbage""#, "-"),
        (r#""2h""#, "-"),
        ("-1", "-"),
        ("NaN", "-"),
        ("Infinity", "-"),
        (r#""-inf""#, "-"),
        ("true", "-"),
        ("{}", "-"),
    ];
    for (value, expected) in cases {
        assert_eq!(format_uptime(Some(&json(value))), expected, "{value}");
    }
    assert_eq!(format_uptime(None), "-");
}

#[test]
fn test_label_and_table_share_uptime() {
    for duration in ["null", r#""garbage""#, "0", r#""7342.81""#] {
        let t = TestEnv::new();
        let mut item = Dict::new();
        item.insert("id", Value::Int(123));
        item.insert("duration", json(duration));
        let expected = format_uptime(Some(&json(duration)));
        assert_eq!(
            instance_label(&item).unwrap(),
            format!("123  -  -  -  {expected}  -")
        );
        let ctx = t.ctx().build();
        render_instances(&ctx, &Value::List(vec![Value::Dict(item)])).unwrap();
        let output = out(&ctx);
        assert!(
            output
                .lines()
                .last()
                .unwrap()
                .trim_end()
                .ends_with(&expected),
            "{duration}"
        );
    }
}
