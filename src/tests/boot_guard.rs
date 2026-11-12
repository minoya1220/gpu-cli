//! `gpu up` against Vast: the boot-time expiry guard in the onstart script,
//! `--cancel-unavail` refusals, and what an interrupted launch reports. All
//! provider calls are fakes; nothing rents an instance.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;

use jiff::Timestamp;

use super::cli::bash_n;
use super::instances::{editor_stubs, ssh_ready};
use crate::ctx::Clock;
use crate::expiry::{local_time, GUARD_SOURCE};
use crate::process::{Output, RunError};
use crate::pyjson::{Dict, Value};
use crate::redact::redact_text;
use crate::repos::Repo;
use crate::testing::fixture_config as defaults;
use crate::testing::*;
use crate::vast::startup::{startup_script, ONSTART_LIMIT};

const DEADLINE: i64 = 4_102_444_800;
const CREATED: &str =
    r#"{"success": true, "new_contract": 123, "instance_api_key": "private-sentinel"}"#;
const ARMED: &str = r#"{"status": "armed", "deadline": 4102444800}"#;
const REFUSED: &str = r#"{"error": true, "status_code": 410, "msg": "error 410/3907: no_such_ask Instance type 42 is no longer available."}"#;
const UP: [&str; 11] = [
    "up",
    "--no-repo",
    "--mode",
    "container",
    "--gpu",
    "Tesla T4",
    "--offer",
    "42",
    "--yes",
    "--expire",
    "2h",
];

fn deadline() -> Timestamp {
    Timestamp::from_second(DEADLINE).unwrap()
}

fn script(mode: &str) -> String {
    startup_script(
        &Repo::from_arg("gpu-kernels"),
        mode,
        &defaults(),
        Some(deadline()),
    )
    .map(|script| unpack(&script))
    .unwrap()
}

fn unpack(script: &str) -> String {
    if let Some(rest) = script
        .lines()
        .nth(1)
        .and_then(|l| l.strip_prefix("bash -c \"$(echo "))
    {
        let packed = rest.split(" | ").next().unwrap();
        decode(packed)
    } else {
        script.into()
    }
}

/// The gzip+base64 payload in the guard line and its `boot DEADLINE` word.
fn guard_payload(script: &str) -> (String, String) {
    let line = script.lines().nth(2).unwrap();
    let packed = line
        .split_once("echo ")
        .and_then(|(_, rest)| rest.split_once(" | base64 -d | gunzip"))
        .unwrap()
        .0;
    let boot = line.split_once(" boot ").unwrap().1;
    (packed.into(), boot.split(' ').next().unwrap().into())
}

/// Decode with the same tools the instance uses.
fn decode(packed: &str) -> String {
    let mut child = Command::new("sh")
        .args(["-c", "base64 -d | gunzip"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{packed}\n").as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn test_expire_script_starts_guard_first_with_absolute_deadline() {
    for mode in ["container", "vm"] {
        let script = script(mode);
        let lines: Vec<&str> = script.lines().collect();
        assert_eq!(lines[0], "#!/bin/bash");
        assert_eq!(
            lines[1],
            "GPU_EXPIRE=${HOME:-/root}/.local/state/gpu-expire"
        );
        assert!(
            lines[2].starts_with(r#"mkdir -p "$GPU_EXPIRE" && chmod 700 "$GPU_EXPIRE" && echo "#)
        );
        assert!(lines[2].ends_with(&format!(
            r#" | base64 -d | gunzip >"$GPU_EXPIRE/guard.sh" && (setsid bash "$GPU_EXPIRE/guard.sh" boot {DEADLINE} </dev/null >/dev/null 2>&1 &)"#
        )));
        assert_eq!(lines[3], if mode == "container" { "(" } else { "set -e" });
        // Everything after the guard is the script without a deadline.
        let plain = unpack(
            &startup_script(&Repo::from_arg("gpu-kernels"), mode, &defaults(), None).unwrap(),
        );
        assert_eq!(
            format!("{}\n{}", lines[0], lines[3..].join("\n")) + "\n",
            plain
        );
        let result = bash_n(&script);
        assert!(result.status.success(), "{mode}: {result:?}");
    }
}

#[test]
fn test_guard_payload_is_the_guard_without_comment_lines() {
    let (packed, boot) = guard_payload(&script("container"));
    assert_eq!(boot, DEADLINE.to_string());
    let decoded = decode(&packed);
    let expected: String = GUARD_SOURCE
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(decoded, expected);
    assert!(bash_n(&decoded).status.success());
    // Comments are the only thing dropped: every code line survives.
    assert_eq!(
        decoded.lines().count(),
        GUARD_SOURCE
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .count()
    );
}

#[test]
fn test_deadline_rounds_up_to_whole_seconds() {
    let at = Timestamp::from_second(DEADLINE).unwrap() + jiff::SignedDuration::from_millis(1);
    let script =
        startup_script(&Repo::from_arg("gpu-kernels"), "vm", &defaults(), Some(at)).unwrap();
    assert_eq!(
        guard_payload(&unpack(&script)).1,
        (DEADLINE + 1).to_string()
    );
}

#[test]
fn test_guard_scripts_fit_vast_onstart_limit_with_room_for_long_repos() {
    let long_repo = format!("https://github.com/{}/{}", "o".repeat(39), "r".repeat(100));
    for mode in ["container", "vm"] {
        let script = startup_script(
            &Repo::from_arg(&long_repo),
            mode,
            &defaults(),
            Some(deadline()),
        )
        .unwrap();
        assert!(script.len() < ONSTART_LIMIT, "{mode}: {}", script.len());
    }
    // The longest generated script (VM) keeps a margin for configuration.
    assert!(
        startup_script(&Repo::NoRepo, "vm", &defaults(), Some(deadline()))
            .unwrap()
            .len()
            < ONSTART_LIMIT
    );
}

#[test]
fn test_oversized_script_fails_before_any_request() {
    let mut cfg = defaults();
    let identity: String = (0..ONSTART_LIMIT)
        .map(|i| format!("{:x}", i * 1664525 + 1013904223))
        .collect();
    cfg.insert("git_name", Value::Str(identity));
    let error = startup_script(&Repo::NoRepo, "container", &cfg, None).unwrap_err();
    assert!(error
        .text()
        .contains(&format!("{ONSTART_LIMIT} character onstart limit")));
}

#[test]
fn test_guard_survives_dry_run_redaction_unchanged() {
    for mode in ["container", "vm"] {
        let script = script(mode);
        let guard = script.lines().take(3).collect::<Vec<_>>().join("\n");
        assert_eq!(redact_text(&guard), guard, "{mode}");
    }
}

/// A launch that answers create with `create` and the expiry SSH with ARMED.
fn launch(create: Result<Output, RunError>) -> Arc<FakeRunner> {
    FakeRunner::new(move |cmd| match strs(&cmd.argv)[..] {
        ["vastai", "search", "offers", ..] => {
            ok(r#"[{"id": 42, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.1}]"#)
        }
        ["vastai", "create", "instance", ..] => create.clone(),
        ["vastai", "show", "instances", "--raw"] => ok(
            r#"[{"id": 123, "actual_status": "running", "ssh_host": "ssh1.vast.ai", "ssh_port": 22}]"#,
        ),
        ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", ..] => ok(ARMED),
        _ => ssh_ready(cmd),
    })
}

/// vastai 1.8.2 under `--raw`: the refusal on stderr, exit 0, no stdout.
fn refused(stderr: &str) -> Result<Output, RunError> {
    Ok(Output {
        code: 0,
        stdout: String::new(),
        stderr: stderr.into(),
    })
}

fn create_calls(runner: &FakeRunner) -> Vec<Vec<String>> {
    runner
        .argvs()
        .into_iter()
        .filter(|a| a.starts_with(&["vastai".into(), "create".into()]))
        .collect()
}

#[test]
fn test_create_passes_cancel_unavail_and_the_guard() {
    let t = TestEnv::new();
    let runner = launch(ok(CREATED));
    let ctx = t.ctx().runner(&runner).build();
    assert_eq!(main(&ctx, &UP), 0, "{}", err(&ctx));
    let create = &create_calls(&runner)[0];
    assert!(create.contains(&"--cancel-unavail".to_string()));
    let onstart = &create[create.iter().position(|a| a == "--onstart-cmd").unwrap() + 1];
    assert!(onstart.contains("/guard.sh\" boot "));
}

#[test]
fn test_cancel_unavail_refusal_is_no_capacity_and_creates_nothing() {
    for json in [false, true] {
        let t = TestEnv::new();
        let runner = launch(refused(&format!(
            "Update available: vastai 1.9.0\n{REFUSED}\n"
        )));
        let ctx = t.ctx().runner(&runner).build();
        let mut argv = UP.to_vec();
        if json {
            argv.push("--json");
        }
        assert_eq!(main(&ctx, &argv), 10, "{}", err(&ctx));
        assert!(runner.argvs().iter().all(|a| a[0] != "ssh"));
        assert_eq!(ctx.interrupt_notice(), None);
        if json {
            let envelope = dict(out(&ctx).trim());
            let result = envelope.get("result").and_then(Value::as_dict).unwrap();
            assert_eq!(result.get("created"), Some(&Value::Bool(false)));
            assert_eq!(result.get("id"), Some(&Value::Null));
            let error = envelope.get("error").and_then(Value::as_dict).unwrap();
            assert_eq!(error.get("code"), Some(&Value::str("no_capacity")));
        } else {
            let error = err(&ctx);
            assert!(
                error.contains("Vast has no capacity for this offer: error 410/3907"),
                "{error}"
            );
            assert!(error.contains("Next: gpu up with another --gpu model or --offer ID"));
            assert!(!error.contains("unreadable"), "{error}");
        }
    }
}

#[test]
fn test_other_create_refusals_report_vast_message() {
    let t = TestEnv::new();
    let runner = launch(refused(
        r#"{"error": true, "status_code": 400, "msg": "Insufficient credit; api_key=secret-sentinel"}"#,
    ));
    let ctx = t.ctx().runner(&runner).build();
    assert_eq!(main(&ctx, &UP), 1);
    let error = err(&ctx);
    assert!(
        error.contains("Vast did not create the instance: Insufficient credit"),
        "{error}"
    );
    assert!(!error.contains("secret-sentinel"), "{error}");
    assert_eq!(ctx.interrupt_notice(), None);
}

#[test]
fn test_unreadable_create_reply_without_refusal_still_points_to_ls() {
    let t = TestEnv::new();
    let runner = launch(refused("Update available: vastai 1.9.0\n"));
    let ctx = t.ctx().runner(&runner).build();
    assert_eq!(main(&ctx, &UP), 1);
    assert!(out(&ctx).contains("response was unreadable; run `gpu ls`"));
    assert!(err(&ctx).contains("no ID returned; expiry NOT verified. Its boot guard"));
    // An instance may exist, so an interrupt must still say so.
    let notice = ctx.interrupt_notice().unwrap();
    assert!(notice.starts_with("Vast may have created an instance; if so, it self-destroys at "));
    assert!(notice.ends_with(". Check: gpu ls"), "{notice}");
}

/// The boot deadline the create request carried.
fn boot_deadline(runner: &FakeRunner) -> Timestamp {
    let create = &create_calls(runner)[0];
    let onstart = &create[create.iter().position(|a| a == "--onstart-cmd").unwrap() + 1];
    Timestamp::from_second(guard_payload(onstart).1.parse().unwrap()).unwrap()
}

#[test]
fn test_interrupt_after_create_names_instance_and_self_destroy_time() {
    let t = TestEnv::new();
    let runner = launch(ok(CREATED));
    let clock = FakeClock::now_real();
    let ctx = t.ctx().runner(&runner).clock(&clock).build();
    let now = clock.now().as_second();
    assert_eq!(main(&ctx, &UP), 0, "{}", err(&ctx));
    let at = boot_deadline(&runner);
    assert!((7199..=7201).contains(&(at.as_second() - now)));
    assert_eq!(
        ctx.interrupt_notice().unwrap(),
        format!(
            "Instance 123 keeps running; it self-destroys at {}. Check: gpu expire 123 show. Stop billing now: gpu down 123",
            local_time(&ctx, at)
        )
    );
}

#[test]
fn test_interrupt_without_expire_says_no_auto_destroy() {
    let t = TestEnv::new();
    let runner = launch(ok(CREATED));
    // Without --expire, past the non-terminal guardrail that requires it.
    let ctx = t.ctx().runner(&runner).build();
    let args = crate::launch::UpArgs {
        repo: Some(Repo::NoRepo),
        mode: Some("container".into()),
        gpu: Some("Tesla T4".into()),
        num_gpus: 1,
        offer: Some(42),
        yes: true,
        ..Default::default()
    };
    crate::launch::launch(&ctx, &args, &defaults(), None).unwrap();
    let create = &create_calls(&runner)[0];
    let onstart = &create[create.iter().position(|a| a == "--onstart-cmd").unwrap() + 1];
    assert!(!onstart.contains("guard.sh"));
    assert_eq!(
        ctx.interrupt_notice().unwrap(),
        "Instance 123 keeps running and billing, with no auto-destroy. Stop billing: gpu down 123"
    );
}

#[test]
fn test_dry_run_sets_no_interrupt_notice() {
    let t = TestEnv::new();
    let runner = launch(ok(CREATED));
    let ctx = t.ctx().runner(&runner).build();
    let mut argv = UP.to_vec();
    argv.push("--dry-run");
    assert_eq!(main(&ctx, &argv), 0, "{}", err(&ctx));
    assert!(create_calls(&runner).is_empty());
    assert_eq!(ctx.interrupt_notice(), None);
}

#[test]
fn test_ctrl_c_in_editor_picker_after_up_reports_running_instance() {
    let t = TestEnv::new();
    editor_stubs(&t);
    let runner = launch(ok(CREATED));
    let ctx = t
        .ctx()
        .runner(&runner)
        .interactive(true)
        .pickers(&["\x03"])
        .build();
    assert_eq!(main(&ctx, &UP), 130);
    assert!(out(&ctx).contains("Cancelled."));
    let error = err(&ctx);
    let at = local_time(&ctx, boot_deadline(&runner));
    assert!(
        error.contains(&format!(
            "Instance 123 keeps running; it self-destroys at {at}. Check: gpu expire 123 show. Stop billing now: gpu down 123"
        )),
        "{error}"
    );
}

#[test]
fn test_ssh_arm_failure_names_boot_guard_time() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(move |cmd| match strs(&cmd.argv)[..] {
        ["vastai", "search", "offers", ..] => {
            ok(r#"[{"id": 42, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.1}]"#)
        }
        ["vastai", "create", "instance", ..] => ok(CREATED),
        ["vastai", "show", "instances", "--raw"] => ok(
            r#"[{"id": 123, "actual_status": "running", "ssh_host": "ssh1.vast.ai", "ssh_port": 22}]"#,
        ),
        ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", ..] => {
            ok(r#"{"error": "verification"}"#)
        }
        _ => ssh_ready(cmd),
    });
    let ctx = t.ctx().runner(&runner).build();
    assert_eq!(main(&ctx, &UP), 1);
    let at = local_time(&ctx, boot_deadline(&runner));
    let error = err(&ctx);
    assert!(
        error.contains(&format!(
            "Instance 123 created but expiry NOT verified; billing continues. Its boot guard should still destroy it at {at}.\n"
        )),
        "{error}"
    );
}

#[test]
fn test_request_dict_shape_for_refusal_is_redacted() {
    let ctx = TestEnv::new()
        .ctx()
        .runner(&FakeRunner::new(|_| {
            refused(r#"{"error": true, "status_code": 400, "msg": "token=abc"}"#)
        }))
        .build();
    let reply = crate::vast::Vast::new(&ctx)
        .create(&[Value::str("vastai"), Value::str("create")])
        .unwrap()
        .unwrap();
    let expected: Dict = Dict::from_iter([
        ("success".into(), Value::Bool(false)),
        ("msg".into(), Value::str("token=REDACTED")),
    ]);
    assert_eq!(reply, expected);
}
