use super::*;
use crate::process::Runner;

fn result(ctx: &Ctx, args: &[&str]) -> Dict {
    let mut args = args.to_vec();
    args.insert(0, "--json");
    assert_eq!(main(ctx, &args), 0, "{}", err(ctx));
    let printed = out(ctx);
    assert_eq!(printed.lines().count(), 1);
    assert!(!printed.contains("do-not-print"));
    dict(&printed)
        .get("result")
        .unwrap()
        .as_dict()
        .unwrap()
        .clone()
}

#[test]
fn tpu_json_launch_wait_exec_copy_expiry_and_lifecycle_use_managed_aliases() {
    let (env, ctx, native, state) = fixture();
    super::super::gcp_prices::cache(&env);
    let relay = native.clone();
    let runner = FakeRunner::new(move |cmd| {
        if cmd.program() == "scp" {
            ok("")
        } else {
            relay.run(cmd)
        }
    });
    let ctx = ctx.with_runner(runner.clone());
    let mut flags = UP.to_vec();
    flags.push("--wait");
    let launch = result(&ctx, &flags);
    let id = crate::gcp::text(&launch, "id").unwrap();
    assert!(id.starts_with("gcp:test-project:us-east5-b:gpu-"));
    let alias = crate::gcp::text(&launch, "ssh_alias").unwrap();
    assert!(alias.starts_with("gcp_test-project_us-east5-b_gpu-"));
    assert_eq!(launch.get("ready"), Some(&Value::Bool(true)));
    assert_eq!(launch.get("created"), Some(&Value::Bool(true)));
    assert_eq!(launch.get("expiry_armed"), Some(&Value::Bool(true)));
    assert!(matches!(
        launch.get("price_per_hour"),
        Some(Value::Float(_))
    ));
    assert!(env.home.join(".ssh/gpu_gcp_config").exists());
    let ready = result(&ctx, &["wait", id, "--timeout", "2"]);
    assert_eq!(ready.get("ssh_alias"), launch.get("ssh_alias"));
    let executed = result(&ctx, &["exec", id, "--", "printf", "%s", "a b"]);
    assert_eq!(executed.get("exit_code"), Some(&Value::Int(0)));
    let ssh = runner
        .calls()
        .into_iter()
        .rfind(|c| c.program() == "ssh")
        .unwrap();
    assert_eq!(ssh.argv[ssh.argv.len() - 2], alias);
    assert_eq!(ssh.argv.last().unwrap(), "printf %s 'a b'");
    let copied = result(&ctx, &["cp", id, ":/workspace/file", "./local file"]);
    assert_eq!(copied.get("exit_code"), Some(&Value::Int(0)));
    assert!(runner
        .argvs()
        .iter()
        .any(|a| a[0] == "scp" && a.contains(&format!("{alias}:/workspace/file"))));
    let expiry = result(&ctx, &["expire", id, "show"]);
    let expiry = expiry.get("expiry").unwrap().as_dict().unwrap();
    assert_eq!(expiry.get("status"), Some(&Value::str("armed")));
    assert_eq!(expiry.get("deadlines").unwrap().as_list().unwrap().len(), 1);
    let stopped = result(&ctx, &["stop", id]);
    assert_eq!(stopped.get("state"), Some(&Value::str("stopped")));
    let destroyed = result(&ctx, &["down", id, "--yes"]);
    assert_eq!(destroyed.get("state"), Some(&Value::str("destroyed")));
    assert!(state.lock().unwrap().nodes.is_empty());
}

#[test]
fn unknown_tpu_ids_return_not_found_without_lifecycle_or_ssh_calls() {
    for args in [
        vec!["down", ID, "--yes"],
        vec!["stop", ID],
        vec!["expire", ID, "2h"],
        vec!["expire", ID, "show"],
        vec!["expire", ID, "cancel"],
        vec!["wait", ID],
        vec!["exec", ID, "--", "true"],
        vec!["cp", ID, "./file", ":/workspace/file"],
    ] {
        let (_env, ctx, runner, _state) = fixture();
        let mut args = args;
        args.insert(0, "--json");
        assert_eq!(main(&ctx, &args), 16, "{args:?}: {}", err(&ctx));
        let object = dict(&out(&ctx));
        assert_eq!(
            object.get("error").unwrap().as_dict().unwrap().get("code"),
            Some(&Value::str("not_found"))
        );
        assert!(object
            .get("result")
            .unwrap()
            .as_dict()
            .unwrap()
            .get("state")
            .is_none());
        assert!(runner.argvs().iter().all(|a| a[0] != "ssh"
            && a[0] != "scp"
            && !a.iter().any(
                |s| ["create", "create-http-task", "delete", "stop", "start"].contains(&s.as_str())
            )));
    }
}

#[test]
fn a_known_orphan_expiry_task_can_still_be_cancelled() {
    let (_env, ctx, _runner, state) = fixture();
    state.lock().unwrap().nodes.push(node(false));
    assert_eq!(main(&ctx, &["expire", ID, "2h"]), 0, "{}", err(&ctx));
    state.lock().unwrap().nodes.clear();
    assert_eq!(main(&ctx, &["expire", ID, "cancel"]), 0, "{}", err(&ctx));
    assert!(state.lock().unwrap().tasks.is_empty());
    assert_eq!(main(&ctx, &["expire", ID, "cancel"]), 16);
}

#[test]
fn down_of_a_recorded_but_absent_tpu_cancels_expiry_and_removes_its_alias() {
    let tpu = crate::gcp::TpuId::parse(ID).unwrap();
    for record in ["task", "stanza"] {
        let (env, ctx, runner, state) = fixture();
        if record == "task" {
            state.lock().unwrap().nodes.push(node(false));
            assert_eq!(main(&ctx, &["expire", ID, "2h"]), 0, "{}", err(&ctx));
            state.lock().unwrap().nodes.clear();
            assert!(!state.lock().unwrap().tasks.is_empty());
            out(&ctx);
        } else {
            std::fs::create_dir_all(env.home.join(".ssh")).unwrap();
            std::fs::write(
                env.home.join(".ssh/gpu_gcp_config"),
                format!(
                    "Host {}\n  HostName example\nHost other\n  HostName other\n",
                    crate::gcp_ssh::alias(&tpu)
                ),
            )
            .unwrap();
        }
        let destroyed = result(&ctx, &["down", ID, "--yes"]);
        assert_eq!(destroyed.get("state"), Some(&Value::str("already_absent")));
        assert!(state.lock().unwrap().tasks.is_empty(), "{record}");
        assert!(!crate::gcp_ssh::has(&ctx, &tpu).unwrap(), "{record}");
        if record == "stanza" {
            assert!(
                std::fs::read_to_string(env.home.join(".ssh/gpu_gcp_config"))
                    .unwrap()
                    .contains("Host other")
            );
        }
        assert!(runner
            .argvs()
            .iter()
            .all(|a| !a.iter().any(|s| s == "delete") || a.iter().any(|s| s == "tasks")));
        assert_eq!(main(&ctx, &["down", ID, "--yes"]), 16, "{record}");
    }
}

#[test]
fn tpu_wait_timeout_and_auth_and_setup_have_distinct_json_codes() {
    let (_env, ctx, runner, state) = fixture();
    let mut n = node(false);
    n.insert("state", Value::str("CREATING"));
    state.lock().unwrap().nodes.push(n);
    assert_eq!(main(&ctx, &["wait", ID, "--timeout", "1", "--json"]), 14);
    let object = dict(&out(&ctx));
    assert_eq!(
        object.get("error").unwrap().as_dict().unwrap().get("code"),
        Some(&Value::str("wait_timeout"))
    );
    assert_eq!(runner.calls().len(), 1);
    let auth = FakeRunner::new(|_| Ok(Output::failed(1, "credentials expired token=do-not-print")));
    let ctx = ctx.with_runner(auth);
    assert_eq!(main(&ctx, &["wait", ID, "--json"]), 13);
    assert!(!out(&ctx).contains("do-not-print"));
    let env = TestEnv::new();
    let ctx = env.ctx().build();
    assert_eq!(main(&ctx, &["wait", ID, "--json"]), 12);
    assert!(out(&ctx).contains("not_setup"));
}

#[test]
fn http_expired_auth_has_the_same_status_and_never_exposes_response_body() {
    let (_env, ctx, _runner, _state) = fixture();
    let runner = FakeRunner::new(|cmd| match cmd.program() {
        "gcloud" => ok("test-oauth-token"),
        "curl" => ok("{\"error\":\"private-sentinel\"}\n401"),
        _ => panic!("unexpected request"),
    });
    let ctx = ctx.with_runner(runner);
    let error = crate::gcp_http::get(
        &ctx,
        "test-project",
        "https://tpu.googleapis.com/v2/projects/test-project/locations/-/nodes",
    )
    .unwrap_err();
    assert_eq!(error.exit_code(), 13);
    assert!(!error.text().contains("private-sentinel"));
}

#[test]
fn tpu_spend_cap_refuses_before_arming_or_creating() {
    let (env, ctx, runner, _state) = fixture();
    super::super::gcp_prices::cache(&env);
    let mut cfg = dict(CONFIG);
    cfg.insert("max_session_spend", Value::Float(0.01));
    std::fs::write(crate::config::config_path(&ctx.env), value(cfg)).unwrap();
    let mut flags = UP.to_vec();
    flags.push("--json");
    assert_eq!(main(&ctx, &flags), 15, "{}", err(&ctx));
    assert!(out(&ctx).contains("spend_limit"));
    assert!(!runner
        .argvs()
        .iter()
        .flatten()
        .any(|a| a == "create" || a == "create-http-task"));
}

#[test]
fn tpu_json_launch_without_wait_reports_no_unconfigured_ssh_alias() {
    let (env, ctx, _runner, _state) = fixture();
    super::super::gcp_prices::cache(&env);
    let launch = result(&ctx, UP);
    assert!(crate::gcp::text(&launch, "id")
        .unwrap()
        .starts_with("gcp:test-project:us-east5-b:gpu-"));
    assert_eq!(launch.get("created"), Some(&Value::Bool(true)));
    assert_eq!(launch.get("ssh_alias"), Some(&Value::Null));
    assert!(!env.home.join(".ssh/gpu_gcp_config").exists());
}
