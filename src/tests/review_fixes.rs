//! Regression coverage for paid-resource and agent-surface review findings.
//! All external tools cross FakeRunner; these tests cannot reach a provider.
use crate::process::{Output, RunError};
use crate::pyjson::{self, Dict, Value};
use crate::testing::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

const LIVE: &str = r#"[{"id":123,"actual_status":"running","ssh_host":"fake","ssh_port":22}]"#;
const OFFER: &str = r#"[{"id":42,"gpu_name":"T4","num_gpus":1,"dph_total":0.1}]"#;
const UP: &[&str] = &[
    "up",
    "--gpu",
    "T4",
    "--no-repo",
    "--mode",
    "container",
    "--expire",
    "2h",
    "--yes",
    "--json",
];

fn result(ctx: &crate::ctx::Ctx) -> Dict {
    dict(&out(ctx))
}

#[test]
fn transient_inventory_retries_but_auth_and_absence_do_not() {
    for failure in [
        ok("<html>502</html>"),
        ok("{}"),
        Ok(Output::failed(1, "Connection reset by peer")),
        Err(RunError::TimedOut),
    ] {
        let env = TestEnv::new();
        let n = Arc::new(AtomicUsize::new(0));
        let polls = n.clone();
        let runner = FakeRunner::new(move |cmd| match cmd.program() {
            "vastai" if polls.fetch_add(1, Ordering::SeqCst) == 0 => failure.clone(),
            "vastai" => ok(LIVE),
            "ssh" => ok(""),
            _ => panic!("{:?}", cmd.argv),
        });
        let ctx = env.ctx().runner(&runner).build();
        assert_eq!(
            main(&ctx, &["wait", "123", "--timeout", "10", "--json"]),
            0,
            "{}",
            out(&ctx)
        );
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }
    for (reply, code) in [("[]", 16), (r#"{"error":"unauthorized"}"#, 13)] {
        let env = TestEnv::new();
        let runner = FakeRunner::new(move |_| ok(reply));
        let ctx = env.ctx().runner(&runner).build();
        assert_eq!(main(&ctx, &["wait", "123", "--json"]), code);
        assert_eq!(runner.calls().len(), 1);
    }
}

#[test]
fn repeated_unreadable_reply_reaches_deadline_without_login_advice() {
    let env = TestEnv::new();
    let clock = FakeClock::now_real();
    let runner = FakeRunner::new(|_| ok("<html>502 secret-sentinel</html>"));
    let ctx = env.ctx().runner(&runner).clock(&clock).build();
    assert_eq!(main(&ctx, &["wait", "123", "--timeout", "5", "--json"]), 14);
    let text = out(&ctx);
    assert!(text.contains("temporarily unavailable"));
    assert!(!text.contains("Log in") && !text.contains("secret-sentinel"));
    assert_eq!(clock.elapsed(), 5.0);
    assert!(runner.calls().len() > 1);
}

#[test]
fn successful_paid_up_ignores_history_write_failure() {
    let env = TestEnv::new();
    std::fs::write(env.state.join("gpu"), b"not a directory").unwrap();
    let runner = FakeRunner::new(|cmd| match strs(&cmd.argv)[..] {
        ["vastai", "search", ..] => ok(OFFER),
        ["vastai", "create", ..] => ok(r#"{"success":true,"new_contract":123}"#),
        ["vastai", "show", ..] => ok(LIVE),
        ["ssh", ..] if cmd.argv.last().unwrap().starts_with("cat ") => ok("done"),
        ["ssh", ..] => ok(r#"{"status":"armed","deadline":4102444800}"#),
        _ => panic!("{:?}", cmd.argv),
    });
    let ctx = env.ctx().runner(&runner).build();
    assert_eq!(main(&ctx, UP), 0);
    let reply = result(&ctx);
    assert_eq!(reply.get("ok"), Some(&Value::Bool(true)));
    assert_eq!(
        reply.get("result").unwrap().get("id"),
        Some(&Value::str("123"))
    );
    assert!(err(&ctx).contains("could not save pick history"));
}

#[test]
fn all_missing_launch_flags_are_reported_at_once_and_invalid_ranges_are_usage() {
    let env = TestEnv::new();
    let ctx = env.ctx().build();
    assert_eq!(main(&ctx, &["up", "--json"]), 2);
    let text = out(&ctx);
    for flag in ["--gpu", "--expire", "--repo", "--mode", "--yes"] {
        assert!(text.contains(flag), "{text}");
    }
    for (flag, value) in [
        ("--num-gpus", "0"),
        ("--disk", "0"),
        ("--max-price", "-1"),
        ("--reliability", "2"),
        ("--offer", "0"),
    ] {
        for command in ["up", "run"] {
            let mut args = if command == "up" {
                UP.to_vec()
            } else {
                vec!["run", "--gpu", "T4", "--expire", "2h", "--yes", "--json"]
            };
            args.extend([flag, value]);
            if command == "run" {
                args.extend(["--", "true"]);
            }
            assert_eq!(main(&ctx, &args), 2, "{args:?}");
            assert_eq!(
                result(&ctx).get("error").unwrap().get("code"),
                Some(&Value::str("usage"))
            );
        }
    }
}

#[test]
fn dry_run_json_preserves_shell_request_and_actual_create_argv() {
    let env = TestEnv::new();
    let runner = FakeRunner::new(|cmd| match strs(&cmd.argv)[..] {
        ["vastai", "search", ..] => ok(OFFER),
        ["vastai", "create", ..] => ok(r#"{"success":true,"new_contract":123}"#),
        ["vastai", "show", ..] => ok(LIVE),
        ["ssh", ..] => ok(r#"{"status":"armed","deadline":4102444800}"#),
        _ => panic!("{:?}", cmd.argv),
    });
    let ctx = env.ctx().runner(&runner).build();
    let mut args = UP.to_vec();
    args.push("--dry-run");
    assert_eq!(main(&ctx, &args), 0);
    let reply = result(&ctx);
    let expected = reply
        .get("result")
        .unwrap()
        .get("request")
        .unwrap()
        .as_list()
        .unwrap();
    assert!(pyjson::dumps(&Value::List(expected.clone()), None).contains("${GH_TOKEN:-}"));
    assert_eq!(main(&ctx, UP), 0);
    let actual = runner
        .calls()
        .into_iter()
        .find(|cmd| cmd.argv.starts_with(&["vastai".into(), "create".into()]))
        .unwrap();
    assert_eq!(
        actual.argv,
        expected
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    );
}

#[test]
fn mistyped_ids_never_suggest_setting_up_another_provider() {
    let env = TestEnv::new();
    let ctx = env.ctx().build();
    for args in [
        vec!["down", "abc", "--yes", "--json"],
        vec!["stop", "gpu-foo", "--json"],
        vec!["wait", "abc", "--json"],
        vec!["expire", "abc", "show", "--json"],
    ] {
        assert_eq!(main(&ctx, &args), 16);
        let text = out(&ctx);
        assert!(text.contains("Vast IDs are numeric") && !text.contains("not set up"));
    }
}

#[test]
fn parse_errors_honor_json_prefixes_and_separate_usage() {
    let env = TestEnv::new();
    let ctx = env.ctx().build();
    for flag in ["--j", "--js", "--jso", "--json"] {
        assert_eq!(main(&ctx, &["up", "--bogus", flag]), 2);
        let reply = result(&ctx);
        assert_eq!(reply.get("command"), Some(&Value::str("up")));
        let message = reply
            .get("error")
            .unwrap()
            .get("message")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(!message.contains('\n'));
        assert!(reply.get("result").unwrap().get("usage").is_some());
    }
}

#[test]
fn remote_deadlines_keepalives_and_provider_deadlines_are_applied() {
    for operation in ["exec", "cp"] {
        let env = TestEnv::new();
        let runner = FakeRunner::new(|cmd| match cmd.program() {
            "vastai" => {
                assert!(cmd.timeout.is_some());
                ok(LIVE)
            }
            "ssh" if cmd.argv.last().unwrap() == "true" => ok(""),
            "ssh" if cmd.argv.last().unwrap().starts_with("cat ") => ok("done"),
            "ssh" | "scp" => {
                assert_eq!(cmd.timeout, Some(7.0));
                assert!(cmd.argv.contains(&"ServerAliveInterval=15".into()));
                assert!(cmd.argv.contains(&"ServerAliveCountMax=4".into()));
                Err(RunError::TimedOut)
            }
            _ => panic!("{:?}", cmd.argv),
        });
        let ctx = env.ctx().runner(&runner).build();
        let mut args = vec![operation, "123", "--command-timeout", "7", "--json"];
        if operation == "exec" {
            args.extend(["--", "sleep", "999"]);
        } else {
            args.extend(["./file", ":/workspace/file"]);
        }
        assert_eq!(main(&ctx, &args), 20);
        assert_eq!(
            result(&ctx).get("error").unwrap().get("code"),
            Some(&Value::str("command_timeout"))
        );
    }
}

#[test]
fn concurrent_ssh_updates_are_lossless_and_unchanged_updates_do_not_replace_file() {
    use std::os::unix::fs::MetadataExt;
    let env = TestEnv::new();
    let vars = env.env();
    std::thread::scope(|scope| {
        for id in 1..=16 {
            let vars = &vars;
            scope.spawn(move || {
                let item = dict(&format!(r#"{{"id":{id},"ssh_host":"fake","ssh_port":22}}"#));
                crate::ssh_config::update_host(vars, &item).unwrap();
            });
        }
    });
    for id in 1..=16 {
        assert!(crate::ssh_config::has_host(&vars, &id.to_string()).unwrap());
    }
    let path = env.home.join(".ssh/vst_config");
    let inode = std::fs::metadata(&path).unwrap().ino();
    crate::ssh_config::update_host(&vars, &dict(r#"{"id":1,"ssh_host":"fake","ssh_port":22}"#))
        .unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
}

#[test]
fn identity_is_fresh_on_each_launch_and_never_saved() {
    let env = TestEnv::new();
    for tool in ["gh", "git"] {
        env.executable(tool, "#!/bin/sh\nexit 1\n");
    }
    let account = Arc::new(AtomicUsize::new(0));
    let selected = account.clone();
    let runner = FakeRunner::new(move |cmd| match strs(&cmd.argv)[..] {
        ["gh", "api", "user", ..] => ok(&format!("owner{}", selected.load(Ordering::SeqCst))),
        ["git", "config", "--get", field] => {
            ok(&format!("{field}{}", selected.load(Ordering::SeqCst)))
        }
        ["vastai", "search", ..] => ok(OFFER),
        _ => panic!("{:?}", cmd.argv),
    });
    let ctx = env.ctx().runner(&runner).build();
    for value in 0..2 {
        account.store(value, Ordering::SeqCst);
        let mut args = UP.to_vec();
        args.extend(["--dry-run"]);
        assert_eq!(main(&ctx, &args), 0);
        let text = out(&ctx);
        assert!(text.contains(&format!("user.name{value}")));
        assert!(!crate::config::config_path(&env.env()).exists());
    }
    let defaults = crate::config::defaults();
    for key in ["owner", "git_name", "git_email"] {
        assert_eq!(defaults.get(key), Some(&Value::str("")));
    }
}

#[test]
fn setup_readiness_is_separate_from_ssh_and_wait_setup_observes_failure() {
    for (state, required, expected) in [
        ("pending", false, 0),
        ("failed", false, 0),
        ("failed", true, 1),
        ("done", true, 0),
        ("pending", true, 14),
    ] {
        let env = TestEnv::new();
        let runner = FakeRunner::new(move |cmd| match cmd.program() {
            "vastai" => ok(LIVE),
            "ssh" if cmd.argv.last().unwrap() == "true" => ok(""),
            "ssh" => ok(state),
            _ => panic!("{:?}", cmd.argv),
        });
        let ctx = env.ctx().runner(&runner).build();
        let mut args = vec!["wait", "123", "--timeout", "5", "--json"];
        if required {
            args.push("--wait-setup");
        }
        assert_eq!(main(&ctx, &args), expected);
        let reply = result(&ctx);
        let result = reply.get("result").unwrap();
        assert_eq!(result.get("ready"), Some(&Value::Bool(true)));
        assert_eq!(
            result.get("setup_done"),
            Some(&Value::Bool(state != "pending"))
        );
    }
}

#[test]
fn enveloped_ls_reports_partial_failure_without_changing_legacy_array() {
    let env = TestEnv::new();
    let runner = FakeRunner::new(|_| ok(LIVE));
    let ctx = env.ctx().runner(&runner).build();
    assert_eq!(main(&ctx, &["ls", "--json"]), 0);
    assert!(matches!(pyjson::loads(&out(&ctx)).unwrap(), Value::List(_)));
    assert_eq!(main(&ctx, &["ls", "--json", "--envelope"]), 0);
    let reply = result(&ctx);
    assert_eq!(reply.get("command"), Some(&Value::str("ls")));
    assert_eq!(
        reply.get("result").unwrap().get("complete"),
        Some(&Value::Bool(true))
    );
}

#[test]
fn all_public_arguments_have_help() {
    use clap::CommandFactory;
    let mut cli = crate::cli::Cli::command();
    cli.build();
    for command in cli.get_subcommands() {
        for arg in command.get_arguments() {
            if arg.is_hide_set() || arg.get_id() == "json" {
                continue;
            }
            assert!(
                arg.get_help().is_some(),
                "{}: {}",
                command.get_name(),
                arg.get_id()
            );
        }
    }
}
