//! Port of tests/test_lifecycle.py: lifecycle verification uses only fake
//! provider state and a fake clock.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::commands::wait_for_lifecycle;
use crate::process::Output;
use crate::ssh_config::update_host;
use crate::testing::*;

#[test]
fn test_lifecycle_verifies_before_reporting() {
    for action in ["stop", "destroy"] {
        for outcome in ["success", "timeout", "failure", "poll_failure"] {
            let t = TestEnv::new();
            let clock = FakeClock::now_real();
            let polls = Arc::new(AtomicUsize::new(0));
            let requested = Arc::new(AtomicBool::new(false));
            let (counter, asked) = (polls.clone(), requested.clone());
            let runner = FakeRunner::new(move |cmd| {
                let timeout = cmd.timeout.expect("every call has a timeout");
                assert!(0.0 < timeout && timeout <= 60.0);
                if cmd.argv[1] == action {
                    asked.store(true, Ordering::SeqCst);
                    if outcome == "failure" {
                        return Ok(Output::failed(1, "provider refused request"));
                    }
                    return ok("success");
                }
                assert_eq!(strs(&cmd.argv), ["vastai", "show", "instances", "--raw"]);
                if asked.load(Ordering::SeqCst) {
                    counter.fetch_add(1, Ordering::SeqCst);
                    if outcome == "poll_failure" {
                        return Ok(Output::failed(1, "provider unavailable"));
                    }
                }
                let status = if outcome == "success" && counter.load(Ordering::SeqCst) >= 3 {
                    if action == "destroy" {
                        return ok("[]");
                    }
                    "stopped"
                } else {
                    "running"
                };
                ok(&format!(
                    r#"[{{"id": 12, "gpu_name": "RTX 4090", "actual_status": "{status}"}}]"#
                ))
            });
            // A managed alias that only a verified destroy may remove.
            update_host(
                &t.env(),
                &dict(r#"{"id": 12, "ssh_host": "ssh6.vast.ai", "ssh_port": 2222}"#),
            )
            .unwrap();
            let managed = t.home.join(".ssh").join("vst_config");
            let ctx = t.ctx().runner(&runner).clock(&clock).build();
            let args: &[&str] = if action == "destroy" {
                &["down", "12", "--yes"]
            } else {
                &["stop", "12"]
            };
            let expected = if outcome == "success" { 0 } else { 1 };
            assert_eq!(main(&ctx, args), expected, "{action} {outcome}");
            let (stdout, stderr) = (out(&ctx), err(&ctx));
            let removed = !read(&managed).contains("Host vast-12\n");
            if outcome == "success" {
                assert_eq!(polls.load(Ordering::SeqCst), 3);
                assert!(stdout.contains("Instance 12 (RTX 4090)"));
                let done = if action == "destroy" {
                    "destroyed; billing stopped."
                } else {
                    "stopped; storage charges may continue."
                };
                assert!(stdout.contains(done));
                assert_eq!(removed, action == "destroy");
            } else {
                assert!(stdout.is_empty(), "{action} {outcome}: {stdout}");
                assert!(stderr.contains("Instance 12"));
                assert!(!removed);
                if outcome == "timeout" {
                    assert_eq!(clock.elapsed(), 60.0);
                    assert!(stderr.contains("not verified"));
                }
                if outcome == "failure" {
                    assert_eq!(polls.load(Ordering::SeqCst), 0);
                    assert!(stderr.contains("failed"));
                }
            }
        }
    }
}

#[test]
fn test_stop_disappearance_is_not_stopped() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|_| ok("[]"));
    let ctx = t
        .ctx()
        .runner(&runner)
        .clock(&FakeClock::now_real())
        .build();
    let error = wait_for_lifecycle(&ctx, "12", "stop", 2.0).unwrap_err();
    assert!(error.text().contains("stop not verified"));
}
