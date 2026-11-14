//! Bounded, pure invariants; no provider commands or network access.
use proptest::prelude::*;

use crate::pyjson::{self, Dict, Value};
use crate::{expiry, modal, pyfmt, redact, repos};

fn text() -> impl Strategy<Value = String> {
    prop::collection::vec(any::<char>(), 0..64).prop_map(|chars| chars.into_iter().collect())
}

fn json_value() -> impl Strategy<Value = Value> {
    // NaN is deliberately excluded: it is not equal to itself.
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::Int),
        text().prop_map(Value::Str),
    ]
    .prop_recursive(3, 32, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::List),
            prop::collection::vec((text(), inner), 0..4)
                .prop_map(|entries| Value::Dict(entries.into_iter().collect())),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn shell_quoting_round_trips(
        args in prop::collection::vec(
            prop::collection::vec(any::<char>().prop_filter("shell argv cannot contain NUL", |c| *c != '\0'), 0..64)
                .prop_map(|chars| chars.into_iter().collect::<String>()), 0..8)
    ) {
        prop_assert_eq!(shlex::split(&pyfmt::shell_join(&args)), Some(args));
    }

    #[test]
    fn json_escaping_round_trips(value in json_value()) {
        for indent in [None, Some(2)] {
            prop_assert_eq!(pyjson::loads(&pyjson::dumps(&value, indent)).unwrap(), value.clone());
        }
    }

    #[test]
    fn nested_secret_fields_and_env_pairs_are_redacted(
        secret in text(), depth in 0..6usize,
        key in prop::sample::select(vec!["api_key", "API-KEY", "token", "client_secret", "Password", "passwd", "Authorization"])
    ) {
        let mut fields = Dict::new();
        fields.insert(key, Value::str(&secret));
        fields.insert("safe", Value::Int(42));
        let mut expected = fields.clone();
        expected.insert(key, Value::str(redact::REDACTED));
        let mut input = Value::List(vec![Value::Dict(fields), Value::List(vec![Value::str(key), Value::str(secret)])]);
        let mut output = Value::List(vec![Value::Dict(expected), Value::List(vec![Value::str(key), Value::str(redact::REDACTED)])]);
        for _ in 0..depth {
            input = Value::Dict([("nested".into(), Value::List(vec![input]))].into_iter().collect());
            output = Value::Dict([("nested".into(), Value::List(vec![output]))].into_iter().collect());
        }
        prop_assert_eq!(redact::redact(&input), output.clone());
        prop_assert_eq!(redact::redact(&output), output);
    }

    #[test]
    fn assignment_redaction_preserves_nonsecret_context(
        secret in "[a-zA-Z0-9_-]{1,48}", bearer in any::<bool>(),
        key in prop::sample::select(vec!["api_key", "TOKEN", "secret", "password", "passwd", "authorization"])
    ) {
        let prefix = format!("safe=42 {key}=\"{}", if bearer { "Bearer " } else { "" });
        let input = format!("{prefix}{secret}\"; tail=ok");
        let expected = format!("{prefix}REDACTED\"; tail=ok");
        prop_assert_eq!(redact::redact_text(&input), expected.clone());
        prop_assert_eq!(redact::redact_text(&expected), expected);
    }

    #[test]
    fn modal_ids_accept_only_their_safe_alphabets(raw in prop_oneof![text(), text().prop_map(|s| format!("sb-{s}")), text().prop_map(|s| format!("gpu-cli-run-{s}"))]) {
        let sandbox = format!("modal:{raw}");
        let valid = raw.starts_with("sb-") && raw.len() > 3
            && raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        prop_assert_eq!(modal::sandbox_id(&sandbox).is_ok(), valid);
        let volume = format!("modal:volume:{raw}");
        let valid = raw.starts_with("gpu-cli-run-") && raw.len() > 12
            && raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        prop_assert_eq!(modal::volume_name(&volume).is_ok(), valid);
    }

    #[test]
    fn generated_valid_ids_round_trip(suffix in "[A-Za-z0-9_-]{1,40}", volume_suffix in "[A-Za-z0-9-]{1,40}") {
        let raw = format!("sb-{suffix}");
        let id = format!("modal:{raw}");
        prop_assert_eq!(modal::sandbox_id(&id).unwrap(), raw);
        let raw = format!("gpu-cli-run-{volume_suffix}");
        let id = format!("modal:volume:{raw}");
        prop_assert_eq!(modal::volume_name(&id).unwrap(), raw);
    }

    #[test]
    fn tpu_ids_round_trip_and_reject_component_injection(
        project in "[A-Za-z0-9][A-Za-z0-9-]{0,30}",
        zone in "[A-Za-z0-9][A-Za-z0-9-]{0,30}",
        name in "[A-Za-z0-9][A-Za-z0-9-]{0,30}",
        unsafe_char in prop::sample::select(vec![':', '/', ' ', '\n', ';', '\'', '$'])
    ) {
        let id = format!("gcp:{project}:{zone}:{name}");
        let parsed = crate::gcp::TpuId::parse(&id).unwrap();
        prop_assert_eq!(parsed.cli(), id);
        prop_assert_eq!(crate::gcp::TpuId::from_resource(&parsed.resource()).unwrap(), parsed);
        for id in [format!("gcp:{project}{unsafe_char}:{zone}:{name}"), format!("gcp:{project}:{zone}{unsafe_char}:{name}"), format!("gcp:{project}:{zone}:{name}{unsafe_char}")] {
            prop_assert!(crate::gcp::TpuId::parse(&id).is_err());
        }
    }

    #[test]
    fn repository_names_match_the_safe_component_contract(name in text()) {
        let valid = !name.is_empty() && name != "." && name != ".."
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        prop_assert_eq!(repos::is_name(&name), valid);
        if let Ok((_, basename)) = repos::resolve_repo(&name, "owner") {
            prop_assert!(repos::is_name(&basename));
            prop_assert!(!basename.contains('/'));
        }
    }

    #[test]
    fn clone_forms_agree_on_workspace_name(owner in "[A-Za-z][A-Za-z0-9_-]{0,20}", name in "[A-Za-z][A-Za-z0-9_-]{0,30}") {
        for repo in [name.clone(), format!("{owner}/{name}"), format!("https://github.com/{owner}/{name}.git"), format!("git@github.com:{owner}/{name}.git")] {
            prop_assert_eq!(repos::resolve_repo(&repo, &owner).unwrap().1, name.clone());
        }
        for repo in [format!("{owner}/../{name}"), format!("https://user:pass@github.com/{owner}/{name}"), format!("https://github.com/{owner}/{name}?token=x")] {
            prop_assert!(repos::resolve_repo(&repo, &owner).is_err());
        }
    }

    #[test]
    fn duration_deadlines_are_exact_and_increasing(
        now in -2_000_000_000i64..2_000_000_000, days in 0u32..366, hours in 0u32..48,
        minutes in 0u32..120, seconds in 1u32..120
    ) {
        let duration = format!("{days}d{hours}h{minutes}m{seconds}s");
        let delta = i64::from(days) * 86400 + i64::from(hours) * 3600 + i64::from(minutes) * 60 + i64::from(seconds);
        let now = jiff::Timestamp::from_second(now).unwrap();
        let deadline = expiry::parse_deadline_duration(&duration, now).unwrap();
        prop_assert!(deadline > now);
        prop_assert_eq!(deadline.as_second(), now.as_second() + delta);
        prop_assert_eq!(expiry::parse_when(&duration, now, &jiff::tz::TimeZone::UTC).unwrap(), deadline);
        prop_assert_eq!(expiry::parse_duration(&duration).unwrap(), delta as u128);
    }

    #[test]
    fn absolute_deadlines_must_be_strictly_in_the_future(now in 0i64..2_000_000_000, delta in -86400i64..86400) {
        let target = jiff::Timestamp::from_second(now + delta).unwrap();
        let now = jiff::Timestamp::from_second(now).unwrap();
        let result = expiry::parse_when(&target.to_string(), now, &jiff::tz::TimeZone::UTC);
        if delta > 0 {
            prop_assert_eq!(result.unwrap(), target);
        } else {
            prop_assert!(result.is_err());
        }
    }

    #[test]
    fn overflowing_deadlines_are_rejected(seconds in 1u32..1_000_000) {
        let duration = format!("{seconds}s");
        let huge = format!("{}d", "9".repeat(80));
        prop_assert!(expiry::parse_deadline_duration(&duration, jiff::Timestamp::MAX).is_err());
        prop_assert!(expiry::parse_deadline_duration(&huge, jiff::Timestamp::UNIX_EPOCH).is_err());
        prop_assert!(expiry::parse_deadline_duration("0s", jiff::Timestamp::UNIX_EPOCH).is_err());
    }
}
