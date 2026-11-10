//! Port of tests/test_expiry_launch.py. All launch and SSH operations here
//! are fakes; nothing rents an instance.

use std::sync::{Arc, Mutex};

use super::instances::ssh_ready;
use crate::ctx::Clock;
use crate::testing::*;

const ARGV: [&str; 12] = [
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
    "--dry-run",
];

struct Launch {
    t: TestEnv,
    runner: Arc<FakeRunner>,
    clock: Arc<FakeClock>,
}

/// A launch that creates instance 123 and answers the expiry SSH with `expiry`.
fn launch(create: &'static str, expiry: &'static str) -> Launch {
    let created = Arc::new(Mutex::new(false));
    let runner = FakeRunner::new(move |cmd| match strs(&cmd.argv)[..] {
        ["vastai", "search", "offers", ..] => {
            ok(r#"[{"id": 42, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.1}]"#)
        }
        ["vastai", "create", "instance", ..] => {
            *created.lock().unwrap() = true;
            ok(create)
        }
        ["vastai", "show", "instances", "--raw"] => ok(
            r#"[{"id": 123, "actual_status": "running", "ssh_host": "ssh1.vast.ai", "ssh_port": 22}]"#,
        ),
        ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", ..] => ok(expiry),
        _ => ssh_ready(cmd),
    });
    Launch {
        t: TestEnv::new(),
        runner,
        clock: FakeClock::now_real(),
    }
}

const CREATED: &str =
    r#"{"success": true, "new_contract": 123, "instance_api_key": "private-sentinel"}"#;
const ARMED: &str = r#"{"status": "armed", "deadline": 4102444800}"#;

fn calls_to(runner: &FakeRunner, words: &[&str]) -> Vec<Vec<String>> {
    runner
        .argvs()
        .into_iter()
        .filter(|a| a.len() >= words.len() && a[..words.len()] == *words)
        .collect()
}

#[test]
fn test_up_arms_after_creation() {
    let l = launch(CREATED, ARMED);
    let ctx = l.t.ctx().runner(&l.runner).clock(&l.clock).build();
    let now = l.clock.now().as_second();
    assert_eq!(main(&ctx, &ARGV[..11]), 0, "{}", err(&ctx));
    assert_eq!(
        calls_to(&l.runner, &["vastai", "create", "instance"]).len(),
        1
    );
    let expiry = calls_to(
        &l.runner,
        &[
            "ssh",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "vast-123",
        ],
    );
    let words = shlex::split(expiry[0].last().unwrap()).unwrap();
    assert_eq!(words[3..5], ["arm", "123"]);
    let deadline: i64 = words[5].parse().unwrap();
    assert!(
        (7199..=7201).contains(&(deadline - now)),
        "{}",
        deadline - now
    );
    assert!(!out(&ctx).contains("private-sentinel"));
}

#[test]
fn test_up_dry_run_does_not_arm() {
    let l = launch(CREATED, ARMED);
    let ctx = l.t.ctx().runner(&l.runner).clock(&l.clock).build();
    assert_eq!(main(&ctx, &ARGV), 0);
    assert!(calls_to(&l.runner, &["vastai", "create"]).is_empty());
    assert!(calls_to(&l.runner, &["ssh"]).is_empty());
    assert!(out(&ctx).contains("no timer installed"));
}

#[test]
fn test_up_arming_failure_leaves_instance_and_warns() {
    let l = launch(CREATED, r#"{"error": "verification"}"#);
    let ctx = l.t.ctx().runner(&l.runner).clock(&l.clock).build();
    assert_eq!(main(&ctx, &ARGV[..11]), 1);
    // No attempted rollback or lifecycle call.
    assert_eq!(calls_to(&l.runner, &["vastai", "create"]).len(), 1);
    assert!(calls_to(&l.runner, &["vastai", "destroy"]).is_empty());
    let error = err(&ctx);
    assert!(error.contains("123 created but expiry NOT verified"));
    assert!(error.contains("billing continues"));
    assert_eq!(error.matches("gpu expire 123 show").count(), 1);
    assert!(error.contains("Or stop billing: gpu down 123."));
}

#[test]
fn test_up_arming_failure_without_command_names_show_and_down() {
    let l = launch(CREATED, r#"{"error": "credentials"}"#);
    let ctx = l.t.ctx().runner(&l.runner).clock(&l.clock).build();
    assert_eq!(main(&ctx, &ARGV[..11]), 1);
    assert!(err(&ctx).contains("expiry NOT armed.\nRun gpu expire 123 show or gpu down 123."));
}

#[test]
fn test_up_arming_past_deadline_names_rearm_and_down_once() {
    let l = launch(CREATED, r#"{"error": "past"}"#);
    let ctx = l.t.ctx().runner(&l.runner).clock(&l.clock).build();
    assert_eq!(main(&ctx, &ARGV[..11]), 1);
    let error = err(&ctx);
    assert!(
        error.contains("expiry NOT armed. Run gpu expire 123 2h.\nOr stop billing: gpu down 123.")
    );
    assert_eq!(error.matches("gpu expire 123").count(), 1);
}

#[test]
fn test_up_without_returned_id_cannot_claim_expiry() {
    let l = launch(r#"{"success": true}"#, ARMED);
    let ctx = l.t.ctx().runner(&l.runner).clock(&l.clock).build();
    assert_eq!(main(&ctx, &ARGV[..11]), 1);
    assert!(calls_to(&l.runner, &["ssh"]).is_empty());
    let error = err(&ctx);
    assert!(
        error.contains(
            "no ID returned; expiry NOT verified. Its boot guard should still destroy it at "
        ),
        "{error}"
    );
    assert!(
        error.contains("\nRun gpu ls, then gpu expire ID show."),
        "{error}"
    );
}
