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
    assert_eq!(ssh.argv[6], alias);
    assert_eq!(ssh.argv[7], "printf %s 'a b'");
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
