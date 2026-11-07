//! Port of tests/test_expiry.py (local side). The remote worker's tests are
//! in remote/test_expiry_remote.sh, run by tests/remote_worker.rs.

use jiff::tz::TimeZone;
use jiff::Timestamp;

use super::instances::{fake_vast, ssh_ready};
use crate::ctx::Clock;
use crate::expiry::{
    expiry_summary, local_zone, manage_expiry, parse_duration, parse_expire_arguments, parse_when,
    remote_command, transport_payload, REMOTE_SOURCE,
};
use crate::process::{Output, RunError};
use crate::pyjson::Value;
use crate::testing::*;

fn now() -> Timestamp {
    "2026-01-01T12:00:00Z".parse().unwrap()
}

fn ts(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn new_york() -> TimeZone {
    TimeZone::get("America/New_York").unwrap()
}

fn plus(t: Timestamp, seconds: i64) -> Timestamp {
    t.checked_add(jiff::Span::new().seconds(seconds)).unwrap()
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| s.to_string()).collect()
}

#[test]
fn test_duration() {
    let cases = [
        ("30m", 1800),
        ("2h", 7200),
        ("1h30m", 5400),
        ("1d2h3m4s", 93784),
        ("45m", 2700),
        ("1s", 1),
    ];
    for (text, seconds) in cases {
        assert_eq!(parse_duration(text).unwrap(), seconds as u128);
        assert_eq!(
            parse_when(text, now(), &TimeZone::UTC).unwrap(),
            plus(now(), seconds)
        );
    }
}

#[test]
fn test_invalid_duration() {
    for text in [
        "", "0s", "-2h", "1.5h", "2", "1m2h", "1h1h", "2h;id", "infinity",
    ] {
        assert!(parse_duration(text).is_err(), "{text:?}");
    }
}

#[test]
fn test_times() {
    let cases = [
        ("18:30", "2026-01-01T18:30:00+00:00"),
        ("6:30PM", "2026-01-01T18:30:00+00:00"),
        ("6:30pm", "2026-01-01T18:30:00+00:00"),
        ("6pm", "2026-01-01T18:00:00+00:00"),
        ("12am", "2026-01-02T00:00:00+00:00"),
        ("12pm", "2026-01-02T12:00:00+00:00"),
        ("11:00", "2026-01-02T11:00:00+00:00"),
        ("2026-01-02T09:00:00-05:00", "2026-01-02T14:00:00+00:00"),
        ("2026-01-02T09:00Z", "2026-01-02T09:00:00+00:00"),
        ("2026-01-02 09:00", "2026-01-02T09:00:00+00:00"),
    ];
    for (text, expected) in cases {
        assert_eq!(
            parse_when(text, now(), &TimeZone::UTC).unwrap(),
            ts(expected),
            "{text}"
        );
    }
}

#[test]
fn test_invalid_times() {
    let cases = [
        "24:00",
        "13pm",
        "0am",
        "6:70pm",
        "18",
        "2026-01-01",
        "2025-01-01T18:30Z",
        "2026-01-01T12:00Z",
        "6:3pm",
        "6:30pm; rm -rf /",
        "2h 30m",
    ];
    for text in cases {
        assert!(parse_when(text, now(), &TimeZone::UTC).is_err(), "{text:?}");
    }
}

#[test]
fn test_dst_elapsed_duration_is_not_wall_time() {
    let start = ts("2026-03-08T06:30:00Z"); // 01:30 EST before spring-forward
    let deadline = parse_when("2h", start, &new_york()).unwrap();
    assert_eq!(deadline, plus(start, 7200));
    assert_eq!(deadline.to_zoned(new_york()).hour(), 4);
}

#[test]
fn test_dst_gap_and_fold() {
    let zone = new_york();
    for text in ["2026-03-08T02:30", "2026-11-01T01:30"] {
        let error = parse_when(text, now(), &zone).unwrap_err();
        assert!(error.contains("nonexistent or ambiguous"), "{error}");
    }
    assert_eq!(
        parse_when("2026-11-01T01:30-05:00", now(), &zone)
            .unwrap()
            .to_zoned(TimeZone::UTC)
            .hour(),
        6
    );
    // A clock time skips a nonexistent occurrence; fall-back picks the next fold.
    assert_eq!(
        parse_when("2:30am", ts("2026-03-08T06:00:00Z"), &zone).unwrap(),
        ts("2026-03-09T06:30:00Z")
    );
    assert_eq!(
        parse_when("1:30am", ts("2026-11-01T05:45:00Z"), &zone).unwrap(),
        ts("2026-11-01T06:30:00Z")
    );
}

#[test]
fn test_system_local_timezone_uses_future_rules() {
    // The system zone comes from TZ in the environment snapshot.
    let t = TestEnv::new();
    let zone = local_zone(&t.env().with("TZ", "America/New_York"));
    assert_eq!(
        parse_when("2026-07-01T12:00", now(), &zone).unwrap(),
        ts("2026-07-01T16:00:00Z")
    );
    assert!(parse_when("2026-03-08T02:30", now(), &zone)
        .unwrap_err()
        .contains("DST"));
    assert_eq!(
        parse_when("1:30am", ts("2026-11-01T05:45:00Z"), &zone).unwrap(),
        ts("2026-11-01T06:30:00Z")
    );
}

#[test]
fn test_argument_shape() {
    type Shape<'a> = (Option<&'a str>, &'a str);
    let cases: [(&[&str], Shape); 4] = [
        (&["2h"], (None, "2h")),
        (&["123", "6pm"], (Some("123"), "6pm")),
        (&["cancel"], (None, "cancel")),
        (&["123", "show"], (Some("123"), "show")),
    ];
    for (values, (id, when)) in cases {
        assert_eq!(
            parse_expire_arguments(&strings(values)).unwrap(),
            (id.map(String::from), when.to_string())
        );
    }
}

#[test]
fn test_ambiguous_arguments() {
    for values in [
        &[][..],
        &["123"],
        &["123", "456"],
        &["2h", "123"],
        &["123", "2h", "3h"],
    ] {
        let error = parse_expire_arguments(&strings(values)).unwrap_err();
        assert!(error.contains("Bare numbers are IDs"), "{values:?}");
    }
}

#[test]
fn test_arm_deadline_rounds_up_to_whole_seconds() {
    // Python sends math.ceil(deadline.timestamp()).
    for (deadline, seconds) in [
        ("2023-11-14T22:13:20.25Z", "1700000001"),
        ("2023-11-14T22:13:20.000000001Z", "1700000001"),
        ("2023-11-14T22:13:20Z", "1700000000"),
    ] {
        let command =
            remote_command("vast-1", "1", "arm", Some(deadline.parse().unwrap())).unwrap();
        let words = shlex::split(command.last().unwrap()).unwrap();
        assert_eq!(words.last().unwrap(), seconds, "{deadline}");
    }
}

#[test]
fn test_command_has_no_user_shell_input() {
    let command = remote_command("vast-123", "123", "arm", Some(now())).unwrap();
    assert_eq!(
        command[..command.len() - 1],
        [
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "vast-123"
        ]
    );
    let words = shlex::split(command.last().unwrap()).unwrap();
    assert_eq!(
        words,
        [
            "bash",
            "-s",
            "--",
            "arm",
            "123",
            &now().as_second().to_string()
        ]
    );
    for (alias, instance) in [
        ("vast-1;id", "1;id"),
        ("-oProxyCommand=bad", "123"),
        ("vast-456", "123"),
    ] {
        assert!(remote_command(alias, instance, "show", None).is_err());
    }
}

#[test]
fn test_transport_embeds_standalone_source_without_secrets() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|cmd| {
        assert!(cmd.argv.last().unwrap().starts_with("bash -s -- arm 123 "));
        let crate::process::Input::Text(payload) = &cmd.stdin else {
            panic!("no payload")
        };
        // Evaluate the payload as a library and read back the source it carries.
        let script = format!("{payload}\nprintf '%s' \"$GPU_EXPIRE_SOURCE\"\n");
        let output = run_bash_library(&script);
        assert_eq!(output, REMOTE_SOURCE);
        assert!(!payload.contains("personal-key-sentinel"));
        assert!(!cmd.argv.iter().any(|a| a.contains("personal-key-sentinel")));
        assert_eq!(cmd.timeout, Some(45.0));
        ok(&format!(
            r#"{{"status": "armed", "deadline": {}}}"#,
            now().as_second()
        ))
    });
    // The local account key, wherever vastai would find it.
    std::fs::write(t.home.join(".vast_api_key"), "personal-key-sentinel").unwrap();
    std::fs::create_dir_all(t.config.join("vastai")).unwrap();
    std::fs::write(
        t.config.join("vastai/vast_api_key"),
        "personal-key-sentinel",
    )
    .unwrap();
    let env = t.env().with("VAST_API_KEY", "personal-key-sentinel");
    let ctx = t.ctx().env(env).runner(&runner).build();
    let reply = manage_expiry(&ctx, "vast-123", "123", "arm", Some(now())).unwrap();
    assert_eq!(reply.get("status"), Some(&Value::str("armed")));
    assert!(transport_payload().ends_with(REMOTE_SOURCE));
}

fn run_bash_library(script: &str) -> String {
    use std::io::Write;
    let mut child = std::process::Command::new("bash")
        .arg("-s")
        .env("GPU_EXPIRE_LIBRARY", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn test_transport_errors_do_not_leak() {
    let cases = [
        (r#"{"error": "verification"}"#, "verification failed"),
        (r#"{"error": "credentials"}"#, "scoped"),
        (r#"{"status": "inactive"}"#, "verification failed"),
        (r#"{"error": "personal-key-sentinel"}"#, "Remote expiry"),
    ];
    for (reply, expected) in cases {
        let t = TestEnv::new();
        let runner = FakeRunner::new(move |_| {
            Ok(Output {
                code: 0,
                stdout: reply.to_string(),
                stderr: "personal-key-sentinel".into(),
            })
        });
        let ctx = t.ctx().runner(&runner).build();
        let error = manage_expiry(&ctx, "vast-123", "123", "arm", Some(now())).unwrap_err();
        assert!(error.text().contains(expected), "{reply}: {}", error.text());
        assert!(!error.text().contains("personal-key-sentinel"));
    }
}

#[test]
fn test_timeout_marks_arming_unverified() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|_| Err(RunError::TimedOut));
    let ctx = t.ctx().runner(&runner).build();
    let error = manage_expiry(&ctx, "vast-123", "123", "arm", Some(now())).unwrap_err();
    assert!(error.text().contains("unverified"));
    assert!(error.text().contains("gpu expire 123 show"));
}

#[test]
fn test_confirmation_local_timezone_and_remaining() {
    let t = TestEnv::new();
    let clock = FakeClock::now_real();
    let ctx = t
        .ctx()
        .clock(&clock)
        .env(t.env().with("TZ", "America/New_York"))
        .build();
    let deadline = clock.now().as_second() + 7200;
    let reply = dict(&format!(r#"{{"status": "armed", "deadline": {deadline}}}"#));
    let message = expiry_summary(&ctx, "123", &reply).unwrap();
    assert!(message.contains("auto-DESTROY") && message.contains("Disk will be deleted"));
    assert!(message.contains("in 2h"), "{message}");
    let abbreviation = Timestamp::from_second(deadline)
        .unwrap()
        .to_zoned(new_york())
        .strftime("%Z")
        .to_string();
    assert!(["EST", "EDT"].contains(&abbreviation.as_str()));
    assert!(message.contains(&abbreviation), "{message}");
}

#[test]
fn test_cli_dispatch() {
    let cases: [(&[&str], &str, &str); 4] = [
        (&["123", "2h"], "123", "arm"),
        (&["6pm"], "456", "arm"),
        (&["123", "cancel"], "123", "cancel"),
        (&["show"], "456", "show"),
    ];
    for (args, instance, action) in cases {
        let t = TestEnv::new();
        let listed = format!(
            r#"[{{"id": {instance}, "actual_status": "running", "ssh_host": "ssh1.vast.ai", "ssh_port": 22}}]"#
        );
        let runner = fake_vast(dicts(&listed), move |cmd| match strs(&cmd.argv)[..] {
            ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", ..] => {
                let status = if action == "cancel" {
                    r#"{"status": "cancelled"}"#
                } else {
                    r#"{"status": "armed", "deadline": 4102444800}"#
                };
                ok(status)
            }
            _ => ssh_ready(cmd),
        });
        let picked = instance == "456";
        let pickers: &[&str] = if picked {
            &[crate::picker::testing::RIGHT]
        } else {
            &[]
        };
        let ctx = t
            .ctx()
            .runner(&runner)
            .interactive(picked)
            .pickers(pickers)
            .build();
        let mut argv = vec!["expire"];
        argv.extend_from_slice(args);
        assert_eq!(main(&ctx, &argv), 0, "{args:?}: {}", err(&ctx));
        let calls = runner.argvs();
        let remote = calls
            .iter()
            .find(|a| {
                a.len() == 7 && a[4] == "ConnectTimeout=10" && a[5] == format!("vast-{instance}")
            })
            .expect("expiry call");
        let words = shlex::split(remote.last().unwrap()).unwrap();
        assert_eq!(words[3..5], [action, instance]);
        assert_eq!(words.len() == 6, action == "arm");
        assert_eq!(ctx.ui.remaining(), 0);
        // Only the picker path lists instances before the readiness wait.
        let listings = calls
            .iter()
            .filter(|a| a[..2] == ["vastai", "show"])
            .count();
        assert_eq!(listings, if picked { 2 } else { 1 });
    }
}

#[test]
fn test_cli_usage_errors_before_external_calls() {
    let cases: [&[&str]; 8] = [
        &[],
        &["123"],
        &["123", "123"],
        &["2h", "45m"],
        &["--in", "2h"],
        &["--at", "18:30"],
        &["--cancel"],
        &["6:80pm"],
    ];
    for args in cases {
        let t = TestEnv::new();
        let ctx = t.ctx().build();
        let mut argv = vec!["expire"];
        argv.extend_from_slice(args);
        assert_eq!(main(&ctx, &argv), 2, "{args:?}");
    }
}

#[test]
fn test_cli_show_inactive_is_nonzero() {
    let t = TestEnv::new();
    let clock = FakeClock::now_real();
    let deadline = clock.now().as_second() + 7200;
    let runner = fake_vast(
        dicts(
            r#"[{"id": 123, "actual_status": "running", "ssh_host": "ssh1.vast.ai", "ssh_port": 22}]"#,
        ),
        move |cmd| match strs(&cmd.argv)[..] {
            ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", ..] => ok(&format!(
                r#"{{"status": "inactive", "deadline": {deadline}}}"#
            )),
            _ => ssh_ready(cmd),
        },
    );
    let ctx = t.ctx().runner(&runner).clock(&clock).build();
    assert_eq!(main(&ctx, &["expire", "123", "show"]), 1);
    let stderr = err(&ctx);
    assert!(stderr.contains("NOT running"));
    assert!(stderr.contains("gpu expire 123 2h"));
}

#[test]
fn test_up_expire_invalid_fails_before_launch() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    assert_eq!(main(&ctx, &["up", "--expire", "0m"]), 2);
}
