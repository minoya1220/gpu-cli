//! All cloud, SSH and copy operations are fakes; these tests cannot rent.
use crate::ctx::Ctx;
use crate::process::Output;
use crate::pyjson::{Dict, Value};
use crate::testing::*;
use std::sync::{Arc, Mutex};

const OFFER: &str = r#"[{"id":42,"gpu_name":"Tesla T4","num_gpus":1,"dph_total":0.1}]"#;
const LIVE: &str =
    r#"[{"id":123,"actual_status":"running","ssh_host":"ssh1.vast.ai","ssh_port":22}]"#;
const UP: &[&str] = &[
    "up",
    "--no-repo",
    "--mode",
    "container",
    "--gpu",
    "Tesla T4",
    "--yes",
    "--expire",
    "2h",
];

fn fixture() -> (TestEnv, Ctx, Arc<FakeRunner>) {
    let t = TestEnv::new();
    let state = Arc::new(Mutex::new("running"));
    let runner = FakeRunner::new(move |cmd| match strs(&cmd.argv)[..] {
        ["vastai", "search", "offers", ..] => ok(OFFER),
        ["vastai", "create", "instance", ..] => {
            ok(r#"{"success":true,"new_contract":123,"instance_api_key":"private-sentinel"}"#)
        }
        ["vastai", "show", "instances", "--raw"] => match *state.lock().unwrap() {
            "destroyed" => ok("[]"),
            "stopped" => ok(r#"[{"id":123,"actual_status":"stopped"}]"#),
            _ => ok(LIVE),
        },
        ["vastai", "destroy", "instance", ..] => {
            *state.lock().unwrap() = "destroyed";
            ok("")
        }
        ["vastai", "stop", "instance", ..] => {
            *state.lock().unwrap() = "stopped";
            ok("")
        }
        ["vastai", "ssh-url", ..] => ok("ssh://root@ssh1.vast.ai:22"),
        ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", ..] => {
            ok(r#"{"status":"armed","deadline":4102444800}"#)
        }
        ["ssh", "-T", ..] => Ok(Output {
            code: 7,
            stdout: "remote output\n".into(),
            stderr: "remote error\n".into(),
        }),
        ["ssh", ..] | ["scp", ..] | ["code", ..] | ["cursor", ..] => ok(""),
        _ => panic!("unexpected command: {:?}", cmd.argv),
    });
    for editor in ["code", "cursor"] {
        t.executable(editor, "#!/bin/sh\nexit 0\n");
    }
    let ctx = t.ctx().runner(&runner).build();
    (t, ctx, runner)
}

fn object(ctx: &Ctx, command: &str, code: i32, args: &[&str]) -> Dict {
    let mut args = args.to_vec();
    args.push("--json");
    assert_eq!(main(ctx, &args), code, "{}", err(ctx));
    let text = out(ctx);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(!text.contains("private-sentinel"));
    let object = dict(&text);
    assert_eq!(object.get("command"), Some(&Value::str(command)));
    assert_eq!(object.get("ok"), Some(&Value::Bool(code == 0)));
    object.get("result").unwrap().as_dict().unwrap().clone()
}

#[test]
fn every_vast_command_has_one_json_object_with_machine_results() {
    let (_t, ctx, runner) = fixture();
    let mut up = UP.to_vec();
    up.push("--wait");
    let result = object(&ctx, "up", 0, &up);
    assert_eq!(result.get("id"), Some(&Value::str("123")));
    assert_eq!(result.get("ssh_alias"), Some(&Value::str("vast-123")));
    assert_eq!(result.get("price_per_hour"), Some(&Value::Float(0.1)));
    assert!(matches!(result.get("expires_at"), Some(Value::Str(_))));
    assert_eq!(result.get("created"), Some(&Value::Bool(true)));
    assert_eq!(result.get("expiry_armed"), Some(&Value::Bool(true)));
    assert_eq!(result.get("ready"), Some(&Value::Bool(true)));
    let result = object(&ctx, "wait", 0, &["wait", "123", "--timeout", "3"]);
    assert_eq!(result.get("ready"), Some(&Value::Bool(true)));
    assert_eq!(main(&ctx, &["ls", "--json"]), 0, "{}", err(&ctx));
    assert_eq!(dicts(&out(&ctx)).len(), 1);
    let result = object(&ctx, "expire", 0, &["expire", "123", "show"]);
    assert_eq!(result.get("action"), Some(&Value::str("show")));
    assert_eq!(
        result
            .get("expiry")
            .unwrap()
            .as_dict()
            .unwrap()
            .get("status"),
        Some(&Value::str("armed"))
    );
    let result = object(&ctx, "ssh", 0, &["ssh", "123"]);
    assert!(result.get("ssh_command").is_some());
    for (command, args) in [
        ("edit", vec!["edit", "123", "--editor", "code"]),
        ("cursor", vec!["cursor", "123"]),
    ] {
        let result = object(&ctx, command, 0, &args);
        assert_eq!(result.get("ssh_alias"), Some(&Value::str("vast-123")));
        assert_eq!(result.get("directory"), Some(&Value::str("/workspace")));
    }
    let result = object(&ctx, "setup", 0, &["setup", "--provider", "vast"]);
    assert_eq!(result.get("ready"), Some(&Value::Bool(true)));
    let result = object(
        &ctx,
        "cp",
        0,
        &["cp", "123", "./a file", ":/workspace/a file"],
    );
    assert_eq!(result.get("exit_code"), Some(&Value::Int(0)));
    let scp = runner
        .calls()
        .into_iter()
        .find(|c| c.program() == "scp")
        .unwrap();
    assert!(scp.argv.contains(&"vast-123:/workspace/a file".into()));
    assert!(scp.argv.contains(&"-B".into()));
    assert_eq!(scp.stdin, crate::process::Input::Null);
    let result = object(&ctx, "stop", 0, &["stop", "123"]);
    assert_eq!(result.get("state"), Some(&Value::str("stopped")));
    let result = object(&ctx, "down", 0, &["down", "123", "--yes"]);
    assert_eq!(result.get("state"), Some(&Value::str("destroyed")));
}

#[test]
fn exec_quotes_arguments_and_preserves_remote_status_in_json() {
    let (_t, ctx, runner) = fixture();
    assert_eq!(
        main(
            &ctx,
            &["--json", "exec", "123", "--", "printf", "%s", "a b;$(id)"]
        ),
        7
    );
    let object = dict(&out(&ctx));
    let result = object.get("result").unwrap().as_dict().unwrap();
    assert_eq!(result.get("stdout"), Some(&Value::str("remote output\n")));
    assert_eq!(result.get("stderr"), Some(&Value::str("remote error\n")));
    assert_eq!(result.get("exit_code"), Some(&Value::Int(7)));
    assert_eq!(
        object.get("error").unwrap().as_dict().unwrap().get("code"),
        Some(&Value::str("remote_exit"))
    );
    let command = runner.calls().last().unwrap().clone();
    assert_eq!(command.argv.last().unwrap(), "printf %s 'a b;$(id)'");
    assert!(command.argv.contains(&"-T".into()));
    assert_eq!(command.stdin, crate::process::Input::Null);
}

#[test]
fn nonterminal_missing_choices_refuse_before_any_dependency_or_prompt() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    for (args, missing) in [
        (vec!["up"], "--expire"),
        (vec!["up", "--expire", "2h"], "--gpu"),
        (vec!["up", "--expire", "2h", "--gpu", "T4"], "--repo"),
        (
            vec!["up", "--expire", "2h", "--gpu", "T4", "--no-repo"],
            "--mode",
        ),
        (
            vec![
                "up",
                "--expire",
                "2h",
                "--gpu",
                "T4",
                "--no-repo",
                "--mode",
                "vm",
            ],
            "--yes",
        ),
        (vec!["ssh"], "ID"),
        (vec!["edit"], "ID"),
        (vec!["expire", "show"], "ID"),
        (vec!["stop"], "ID"),
        (vec!["down", "123"], "--yes"),
        (vec!["setup", "--provider", "gcp"], "--project"),
    ] {
        assert_eq!(main(&ctx, &args), 2, "{args:?}");
        assert!(err(&ctx).contains(missing), "{args:?}");
        assert!(out(&ctx).is_empty());
    }
}

#[test]
fn bounded_wait_reports_a_reason_and_post_create_timeout_keeps_id() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|cmd| match strs(&cmd.argv)[..] {
        ["vastai", "search", ..] => ok(OFFER),
        ["vastai", "create", ..] => ok(r#"{"success":true,"new_contract":123}"#),
        ["vastai", "show", ..] => ok(LIVE),
        ["ssh", ..] => Ok(Output::failed(255, "connection refused")),
        _ => panic!("unexpected command"),
    });
    for launch in [false, true] {
        let clock = FakeClock::now_real();
        let ctx = t.ctx().runner(&runner).clock(&clock).build();
        let mut args = if launch {
            UP.to_vec()
        } else {
            vec!["wait", "123"]
        };
        if launch {
            args.push("--wait");
        }
        args.extend(["--timeout", "1", "--json"]);
        assert_eq!(main(&ctx, &args), 14, "{}", err(&ctx));
        assert_eq!(clock.elapsed(), 1.0);
        let object = dict(&out(&ctx));
        let result = object.get("result").unwrap().as_dict().unwrap();
        assert_eq!(result.get("id"), Some(&Value::str("123")));
        if launch {
            assert_eq!(result.get("created"), Some(&Value::Bool(true)));
        }
        let error = object.get("error").unwrap().as_dict().unwrap();
        assert_eq!(error.get("code"), Some(&Value::str("wait_timeout")));
        assert!(crate::pyfmt::py_str(error.get("message").unwrap())
            .contains("SSH probe exited with status 255"));
    }
}

fn transient_then(states: &'static [&'static str]) -> Arc<FakeRunner> {
    let polls = Arc::new(Mutex::new(0usize));
    FakeRunner::new(move |cmd| match strs(&cmd.argv)[..] {
        ["vastai", "search", ..] => ok(OFFER),
        ["vastai", "create", ..] => ok(r#"{"success":true,"new_contract":123}"#),
        ["vastai", "show", "instances", "--raw"] => {
            let mut polls = polls.lock().unwrap();
            let state = states[(*polls).min(states.len() - 1)];
            *polls += 1;
            if state == "running" {
                ok(LIVE)
            } else {
                ok(&format!(
                    r#"[{{"id":123,"actual_status":"{state}","intended_status":"running"}}]"#
                ))
            }
        }
        ["vastai", "ssh-url", ..] => ok("ssh://root@ssh1.vast.ai:22"),
        ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", ..] => {
            ok(r#"{"status":"armed","deadline":4102444800}"#)
        }
        ["ssh", ..] => ok(""),
        _ => panic!("unexpected command: {:?}", cmd.argv),
    })
}

#[test]
fn post_create_expiry_arms_after_transient_vast_states_recover() {
    let t = TestEnv::new();
    let runner = transient_then(&["offline", "exited", "error", "running"]);
    let clock = FakeClock::now_real();
    let ctx = t.ctx().runner(&runner).clock(&clock).build();
    let result = object(&ctx, "up", 0, UP);
    assert_eq!(result.get("id"), Some(&Value::str("123")));
    assert_eq!(clock.elapsed(), 9.0);
    assert!(runner.argvs().iter().any(|argv| strs(argv)[..4]
        == ["ssh", "-o", "BatchMode=yes", "-o"]
        && argv.iter().any(|a| a == "ConnectTimeout=10")));
}

#[test]
fn persistent_transient_vast_state_times_out_with_reason() {
    let t = TestEnv::new();
    let runner = transient_then(&["offline"]);
    let clock = FakeClock::now_real();
    let ctx = t.ctx().runner(&runner).clock(&clock).build();
    assert_eq!(
        main(&ctx, &["wait", "123", "--timeout", "10", "--json"]),
        14,
        "{}",
        err(&ctx)
    );
    assert_eq!(clock.elapsed(), 10.0);
    let object = dict(&out(&ctx));
    let error = object.get("error").unwrap().as_dict().unwrap();
    assert_eq!(error.get("code"), Some(&Value::str("wait_timeout")));
    assert!(
        crate::pyfmt::py_str(error.get("message").unwrap()).contains("provider status: offline")
    );
}

#[test]
fn terminal_vast_states_fail_wait_immediately() {
    for listing in [
        r#"[{"id":123,"actual_status":"destroyed","intended_status":"running"}]"#,
        r#"[{"id":123,"actual_status":"stopped","intended_status":"stopped"}]"#,
        r#"[{"id":123,"actual_status":"exited","intended_status":"stopped"}]"#,
    ] {
        let t = TestEnv::new();
        let runner = FakeRunner::new(move |cmd| match strs(&cmd.argv)[..] {
            ["vastai", "show", "instances", "--raw"] => ok(listing),
            _ => panic!("unexpected command: {:?}", cmd.argv),
        });
        let clock = FakeClock::now_real();
        let ctx = t.ctx().runner(&runner).clock(&clock).build();
        assert_eq!(main(&ctx, &["wait", "123", "--timeout", "60", "--json"]), 1);
        assert_eq!(clock.elapsed(), 0.0, "{listing}");
        let object = dict(&out(&ctx));
        let error = object.get("error").unwrap().as_dict().unwrap();
        assert!(
            crate::pyfmt::py_str(error.get("message").unwrap()).contains("SSH cannot become ready")
        );
    }
}

#[test]
fn stable_failure_codes_and_json_errors_do_not_expose_provider_secrets() {
    for (reply, max_price, expected, name) in [
        ("[]", false, 10, "no_capacity"),
        (OFFER, true, 11, "over_max_price"),
        (
            "missing api key token=private-sentinel",
            false,
            12,
            "not_setup",
        ),
        (
            "invalid api key token=private-sentinel",
            false,
            13,
            "auth_expired",
        ),
    ] {
        let t = TestEnv::new();
        let runner = FakeRunner::new(move |_| {
            if reply.starts_with('[') {
                ok(reply)
            } else {
                Ok(Output::failed(1, reply))
            }
        });
        let ctx = t.ctx().runner(&runner).build();
        let mut args = UP.to_vec();
        if max_price {
            args.extend(["--max-price", "0.01"]);
        }
        args.push("--json");
        assert_eq!(main(&ctx, &args), expected);
        let text = out(&ctx);
        assert!(!text.contains("private-sentinel"));
        let object = dict(&text);
        let error = object.get("error").unwrap().as_dict().unwrap();
        assert_eq!(error.get("code"), Some(&Value::str(name)));
        assert!(
            crate::pyfmt::py_str(error.get("message").unwrap()).contains("Next:") || expected == 10
        );
    }
}

#[test]
fn empty_priced_search_distinguishes_price_from_actual_stockout() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|cmd| {
        assert_eq!(cmd.argv[1..3], ["search", "offers"]);
        if cmd.argv[3].contains("dph<=") {
            ok("[]")
        } else {
            ok(OFFER)
        }
    });
    let ctx = t.ctx().runner(&runner).build();
    let mut flags = UP.to_vec();
    flags.extend(["--max-price", "0.01", "--json"]);
    assert_eq!(main(&ctx, &flags), 11);
    assert!(out(&ctx).contains("over_max_price"));
    assert_eq!(runner.calls().len(), 2);
}

#[test]
fn default_expiry_and_opt_in_session_spend_cap_are_local_guardrails() {
    for (cap, expected) in [("0.01", 15), ("1", 0), ("null", 0)] {
        let (t, ctx, runner) = fixture();
        let path = crate::config::config_path(&ctx.env);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let cap = if cap == "null" {
            String::new()
        } else {
            format!(",\"max_session_spend\":{cap}")
        };
        std::fs::write(path, format!(r#"{{"default_expire":"2h"{cap}}}"#)).unwrap();
        let mut args = UP[..UP.len() - 2].to_vec();
        args.extend(["--dry-run", "--json"]);
        assert_eq!(main(&ctx, &args), expected, "{}", err(&ctx));
        let object = dict(&out(&ctx));
        if expected == 0 {
            let result = object.get("result").unwrap().as_dict().unwrap();
            assert!(matches!(result.get("expires_at"), Some(Value::Str(_))));
            assert_eq!(result.get("created"), Some(&Value::Bool(false)));
        }
        assert!(!runner
            .argvs()
            .iter()
            .any(|a| a.get(1).is_some_and(|s| s == "create")));
        drop(t);
    }
}

#[test]
fn nonterminal_dry_run_without_an_offer_choice_refuses_before_searching() {
    let (_t, ctx, runner) = fixture();
    let mut flags: Vec<_> = UP.iter().copied().filter(|a| *a != "--yes").collect();
    flags.push("--dry-run");
    assert_eq!(main(&ctx, &flags), 2);
    let stderr = err(&ctx);
    assert!(
        stderr.contains("--yes (cheapest) or --offer ID"),
        "{stderr}"
    );
    assert!(out(&ctx).is_empty());
    assert!(runner.calls().is_empty());
}

#[test]
fn vast_ids_containing_401_are_not_authentication_failures() {
    for (args, reply, expected, name) in [
        (
            vec!["stop", "24015678"],
            "no such instance 24015678",
            1,
            "error",
        ),
        (
            vec!["down", "24015678", "--yes"],
            "failed with error 401: please log in again",
            13,
            "auth_expired",
        ),
    ] {
        let t = TestEnv::new();
        let runner = FakeRunner::new(move |_| Ok(Output::failed(1, reply)));
        let ctx = t.ctx().runner(&runner).build();
        let mut args = args.clone();
        args.push("--json");
        assert_eq!(main(&ctx, &args), expected, "{args:?}");
        let object = dict(&out(&ctx));
        let error = object.get("error").unwrap().as_dict().unwrap();
        assert_eq!(error.get("code"), Some(&Value::str(name)), "{args:?}");
        assert!(crate::pyfmt::py_str(error.get("message").unwrap()).contains(reply));
    }
}

#[test]
fn vast_offer_gone_with_401_digits_reports_no_capacity_with_detail() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|cmd| match strs(&cmd.argv)[..] {
        ["vastai", "search", ..] => {
            ok(r#"[{"id":14012345,"gpu_name":"Tesla T4","num_gpus":1,"dph_total":0.1}]"#)
        }
        ["vastai", "create", ..] => Ok(Output::failed(1, "offer 14012345 is no longer available")),
        _ => panic!("unexpected command: {:?}", cmd.argv),
    });
    let ctx = t.ctx().runner(&runner).build();
    let mut args = UP.to_vec();
    args.extend(["--offer", "14012345"]);
    args.push("--json");
    assert_eq!(main(&ctx, &args), 10, "{}", err(&ctx));
    let object = dict(&out(&ctx));
    let error = object.get("error").unwrap().as_dict().unwrap();
    assert_eq!(error.get("code"), Some(&Value::str("no_capacity")));
    assert!(crate::pyfmt::py_str(error.get("message").unwrap())
        .contains("offer 14012345 is no longer available"));
}

#[test]
fn invalid_copy_timeout_and_json_shell_arguments_fail_without_calls() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    for args in [
        vec!["cp", "123", "a", "b"],
        vec!["cp", "123", ":/a", ":/b"],
        vec!["cp", "123", "other:/a", ":/b"],
        vec!["wait", "123", "--timeout", "NaN"],
        vec!["wait", "123", "--timeout", "0"],
        vec!["exec", "123"],
        vec!["--json", "ssh", "123", "--exec"],
    ] {
        assert_eq!(main(&ctx, &args), 2, "{args:?}");
        out(&ctx);
        err(&ctx);
    }
}
