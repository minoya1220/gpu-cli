use super::*;

const PUBLIC: &str = "ssh-ed25519 c3ludGhldGlj fixture\n";

fn public_key(t: &TestEnv) {
    std::fs::create_dir_all(t.home.join(".ssh")).unwrap();
    std::fs::write(t.home.join(".ssh/id_ed25519.pub"), PUBLIC).unwrap();
}

fn ready(cmd: &Cmd) -> std::result::Result<Output, crate::process::RunError> {
    if cmd.argv.get(1).map(String::as_str) == Some("version") {
        return testing::ok(if cmd.program() == "runpodctl" {
            "runpodctl v3.1.1\n"
        } else {
            "doctl version 2.4.0\n"
        });
    }
    if cmd.argv.contains(&"list-keys".into()) {
        return testing::ok(
            r#"{"keys":[{"name":"fixture","type":"ssh-ed25519","key":"ssh-ed25519 c3ludGhldGlj\n","fingerprint":"SHA256:fixture"}]}"#,
        );
    }
    if cmd.argv.get(2).map(String::as_str) == Some("ssh-key") {
        return testing::ok(
            r#"[{"id":123,"public_key":"ssh-ed25519 c3ludGhldGlj fixture","fingerprint":"ab:cd"}]"#,
        );
    }
    fixture(cmd)
}

#[test]
fn setup_checks_cli_version_auth_and_actual_public_key_registration() {
    for provider in [Provider::Runpod, Provider::DigitalOcean] {
        let t = TestEnv::new();
        public_key(&t);
        let runner = FakeRunner::new(ready);
        let ctx = t.ctx().runner(&runner).build().for_command(true);
        check(&ctx, provider).unwrap();
        assert_eq!(ctx.result_snapshot().get("ready"), Some(&Value::Bool(true)));
        assert_eq!(
            ctx.result_snapshot().get("expiry_supported"),
            Some(&Value::Bool(false))
        );
        assert!(runner
            .calls()
            .iter()
            .all(|c| !c.argv.contains(&"create".into())));
    }
}

#[test]
fn stale_cli_or_unregistered_key_fails_before_create() {
    for provider in [Provider::Runpod, Provider::DigitalOcean] {
        for stale in [true, false] {
            let t = TestEnv::new();
            public_key(&t);
            let runner = FakeRunner::new(move |cmd| {
                if stale && cmd.argv[1] == "version" {
                    return testing::ok("v0.0.1 synthetic-private-canary");
                }
                if cmd.argv.contains(&"list-keys".into()) {
                    return testing::ok(r#"{"keys":[]}"#);
                }
                if cmd.argv.get(2).map(String::as_str) == Some("ssh-key") {
                    return testing::ok("[]");
                }
                ready(cmd)
            });
            let ctx = t.ctx().runner(&runner).build();
            let error = check(&ctx, provider).unwrap_err();
            assert_eq!(error.exit_code(), 12);
            assert!(!error.text().contains("synthetic-private-canary"));
            assert!(runner
                .calls()
                .iter()
                .all(|c| !c.argv.contains(&"create".into())));
        }
    }
}

#[test]
fn paid_create_is_once_and_records_ownership_before_readiness() {
    for provider in ["runpod", "do"] {
        let t = TestEnv::new();
        public_key(&t);
        let runner = FakeRunner::new(|cmd| {
            if cmd.argv.contains(&"create".into()) {
                assert!(!cmd.argv.contains(&"--wait".into()));
                if cmd.program() == "runpodctl" {
                    return testing::ok(r#"{"id":"pod-test"}"#);
                }
                let path = &cmd.argv[cmd
                    .argv
                    .iter()
                    .position(|a| a == "--user-data-file")
                    .unwrap()
                    + 1];
                let script = std::fs::read_to_string(path).unwrap();
                assert!(script.contains(".gpu-setup"));
                assert!(!script.contains("ACCESS_TOKEN"));
                // Region may be omitted in a partial but accepted create reply.
                return testing::ok(r#"[{"id":42}]"#);
            }
            ready(cmd)
        });
        let ctx = t.ctx().runner(&runner).build().for_command(true);
        let mut f = flags(provider, &[]);
        f.dry_run = false;
        f.yes = true;
        let launched = up(&ctx, &f, &cfg()).unwrap();
        assert_eq!(
            launched.id.as_deref(),
            Some(if provider == "runpod" {
                "runpod:pod-test"
            } else {
                "do:atl1:42"
            })
        );
        assert_eq!(
            ctx.result_snapshot().get("created"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            ctx.result_snapshot().get("expiry_armed"),
            Some(&Value::Bool(false))
        );
        assert!(ctx.interrupt_notice().unwrap().contains("gpu down"));
        assert_eq!(
            runner
                .calls()
                .iter()
                .filter(|c| c.argv.contains(&"create".into()))
                .count(),
            1
        );
        assert!(runner
            .calls()
            .iter()
            .all(|c| !c.argv.contains(&"get".into()) || c.argv.contains(&"image".into())));
    }
}

#[test]
fn uncertain_create_is_not_retried_and_does_not_expose_cli_responses() {
    for provider in ["runpod", "do"] {
        for reply in [
            Ok(Output::ok("synthetic-private-canary")),
            Ok(Output::failed(1, "synthetic-private-canary")),
            Err(crate::process::RunError::TimedOut),
        ] {
            let t = TestEnv::new();
            public_key(&t);
            let runner = FakeRunner::new(move |cmd| {
                if cmd.argv.contains(&"create".into()) {
                    return reply.clone();
                }
                ready(cmd)
            });
            let ctx = t.ctx().runner(&runner).build().for_command(true);
            let mut f = flags(provider, &[]);
            f.dry_run = false;
            f.yes = true;
            let error = up(&ctx, &f, &cfg()).unwrap_err();
            assert!(!error.text().contains("synthetic-private-canary"));
            assert_eq!(
                runner
                    .calls()
                    .iter()
                    .filter(|c| c.argv.contains(&"create".into()))
                    .count(),
                1
            );
            assert_eq!(ctx.result_snapshot().get("created"), Some(&Value::Null));
            assert!(ctx.interrupt_notice().unwrap().contains("inspect gpu ls"));
        }
    }
}

#[test]
fn selected_do_key_must_match_local_identity_before_any_paid_request() {
    let t = TestEnv::new();
    public_key(&t);
    let runner = FakeRunner::new(ready);
    let ctx = t.ctx().runner(&runner).build();
    let wrong = testing::dict(r#"{"do":{"ssh_keys":"999"}}"#);
    let mut f = flags("do", &[]);
    f.dry_run = false;
    f.yes = true;
    assert_eq!(up(&ctx, &f, &wrong).unwrap_err().exit_code(), 12);
    assert!(!runner
        .calls()
        .iter()
        .any(|c| c.argv.contains(&"create".into())));
}

#[test]
fn dispatcher_routes_new_ids_and_rejects_configured_expiry_before_calls() {
    let t = TestEnv::new();
    let runner = FakeRunner::forbid();
    let ctx = t.ctx().runner(&runner).build();
    let mut configured = cfg();
    configured.insert("default_expire", Value::str("30m"));
    testing::write_config(&t, &configured);
    for args in [
        vec![
            "gpu",
            "up",
            "--provider",
            "runpod",
            "--gpu",
            "MI300X",
            "--no-repo",
            "--dry-run",
            "--json",
        ],
        vec!["gpu", "expire", "runpod:pod-test", "30m", "--json"],
        vec!["gpu", "expire", "do:atl1:42", "show", "--json"],
    ] {
        assert_eq!(
            crate::main_with(
                &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                &ctx
            ),
            2
        );
    }
    assert!(runner.calls().is_empty());
}
