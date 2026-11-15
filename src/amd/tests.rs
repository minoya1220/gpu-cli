mod lifecycle;

use super::*;
use crate::process::Output;
use crate::testing::{self, FakeRunner, TestEnv};
use clap::Parser;
use std::sync::atomic::{AtomicUsize, Ordering};

const RP_CATALOG: &str = r#"[{"gpuId":"AMD Instinct MI300X OAM","secureCloud":true,"communityCloud":false,"securePricePerHr":2.39,"communityPricePerHr":0.5,"dataCenterAvailability":[{"dataCenterId":"US-TEST","stockStatus":"Low"}]}]"#;
const DO_CATALOG: &str = r#"[{"slug":"gpu-mi300x1-192gb","price_hourly":2.59,"available":true,"regions":["atl1"]},{"slug":"gpu-mi300x8-1536gb","price_hourly":20.72,"available":true,"regions":["atl1"]}]"#;
const DO_ROW: &str = r#"[{"id":42,"name":"gpu-cli-test","region":{"slug":"atl1"},"size_slug":"gpu-mi300x1-192gb","status":"active","networks":{"v4":[{"type":"public","ip_address":"203.0.113.4"}]}}]"#;
const RP_ROW: &str = r#"[{"id":"pod-test","name":"gpu-cli-test","runtimeStatus":"initializing","desiredStatus":"RUNNING","gpuId":"AMD Instinct MI300X OAM","gpuCount":1,"costPerHr":2.39,"instance_api_key":"synthetic-private-canary"}]"#;
const RP_ENDPOINT: &str = r#"{"id":"pod-test","runtimeStatus":"running","ssh":{"ip":"203.0.113.4","port":22022,"ssh_command":"malicious command never executed"}}"#;

fn cfg() -> Dict {
    testing::dict(
        r#"{"owner":"fixture","runpod":{"image":"rocm/test@sha256:fixture","datacenter":"US-TEST"},"do":{"region":"atl1","ssh_keys":"123"}}"#,
    )
}
fn flags(provider: &str, extra: &[&str]) -> UpFlags {
    let mut argv = vec![
        "gpu",
        "up",
        "--provider",
        provider,
        "--gpu",
        "MI300X",
        "--no-repo",
        "--dry-run",
    ];
    argv.extend_from_slice(extra);
    match crate::cli::Cli::parse_from(argv).command.unwrap() {
        crate::cli::Command::Up(flags) => flags,
        _ => unreachable!(),
    }
}
fn fixture(cmd: &Cmd) -> std::result::Result<Output, crate::process::RunError> {
    if cmd.program() == "git" {
        return Ok(Output::status(128));
    }
    match (
        cmd.program(),
        cmd.argv.get(1).map(String::as_str),
        cmd.argv.get(2).map(String::as_str),
        cmd.argv.get(3).map(String::as_str),
    ) {
        ("runpodctl", Some("gpu"), Some("list"), _) => testing::ok(RP_CATALOG),
        ("runpodctl", Some("pod"), Some("list"), _) => testing::ok(RP_ROW),
        ("runpodctl", Some("pod"), Some("get"), _) => testing::ok(RP_ENDPOINT),
        ("doctl", Some("compute"), Some("size"), Some("list")) => testing::ok(DO_CATALOG),
        ("doctl", Some("compute"), Some("image"), Some("get")) => {
            testing::ok(r#"[{"id":777,"regions":["atl1"]}]"#)
        }
        ("doctl", Some("compute"), Some("droplet"), Some("list" | "get")) => testing::ok(DO_ROW),
        ("ssh", _, _, _) => testing::ok(""),
        _ => panic!("unexpected subprocess {cmd:?}"),
    }
}

#[test]
fn provider_ids_are_explicit_and_numeric_stays_vast() {
    for (id, provider) in [
        ("42", Provider::Vast),
        ("runpod:pod-test", Provider::Runpod),
        ("do:atl1:42", Provider::DigitalOcean),
    ] {
        assert_eq!(crate::provider::parse(id).unwrap().provider(), provider);
    }
    for id in [
        "runpod:",
        "runpod:--bad",
        "runpod:a:b",
        "do:atl1:abc",
        "do::42",
        "do:atl1:4;rm",
        "do:atl1:042",
        "do:atl1:0",
        "other:42",
    ] {
        // Leading '-' is rejected at the ID boundary, not left to the CLI.
        assert!(crate::provider::parse(id).is_err(), "{id}");
    }
}

#[test]
fn unsupported_up_flags_and_live_runs_fail_without_dependency_calls() {
    let t = TestEnv::new();
    let runner = FakeRunner::forbid();
    let ctx = t.ctx().runner(&runner).build();
    for provider in ["runpod", "do"] {
        for args in [
            vec!["--expire", "30m"],
            vec!["--num-gpus", "2"],
            vec!["--spot"],
            vec!["--offer", "42"],
            vec!["--reliability", "0.9"],
            vec!["--unverified"],
            vec!["--gpu", "MI355X"],
        ] {
            assert!(up(&ctx, &flags(provider, &args), &cfg()).is_err());
        }
        let f = testing::run_flags(
            &["--provider", provider, "--gpu", "MI300X", "--yes"],
            &["true"],
        );
        let error = crate::run::run(&ctx, &f, &cfg(), &t.root).unwrap_err();
        assert!(error.text().contains("expiry is not validated"));
    }
    assert!(runner.calls().is_empty());
}

#[test]
fn catalogs_use_secure_not_community_price_and_only_single_gpu_sizes() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(fixture);
    let ctx = t.ctx().runner(&runner).build();
    let rp = catalog(&ctx, Provider::Runpod).unwrap();
    assert_eq!(rp.len(), 1);
    assert_eq!(rp[0].rate, 2.39);
    assert!(rp[0].available);
    let ocean = catalog(&ctx, Provider::DigitalOcean).unwrap();
    assert_eq!(ocean.len(), 1);
    assert_eq!(ocean[0].rate, 2.59);
    assert!(runner.calls()[0]
        .argv
        .contains(&"--include-unavailable".into()));
}

#[test]
fn absent_stock_null_price_and_unknown_stock_fail_closed() {
    for reply in [
        "[]".to_string(),
        RP_CATALOG.replace("Low", "None"),
        RP_CATALOG.replace("Low", "UNKNOWN"),
        RP_CATALOG.replace("2.39", "null"),
        RP_CATALOG.replace("true", "false"),
    ] {
        let t = TestEnv::new();
        let runner = FakeRunner::new(move |_| testing::ok(&reply));
        let ctx = t.ctx().runner(&runner).build();
        assert!(up(&ctx, &flags("runpod", &[]), &cfg()).is_err());
        assert!(runner
            .calls()
            .iter()
            .all(|c| !c.argv.contains(&"create".into())));
    }
}

#[test]
fn exact_dry_run_requests_are_amd_only_and_include_storage_in_guardrails() {
    for provider in ["runpod", "do"] {
        let t = TestEnv::new();
        let runner = FakeRunner::new(fixture);
        let ctx = t.ctx().runner(&runner).build().for_command(true);
        let mut f = flags(provider, &[]);
        up(&ctx, &f, &cfg()).unwrap();
        let result = ctx.result_snapshot();
        assert_eq!(result.get("created"), Some(&Value::Bool(false)));
        assert_eq!(result.get("expiry_armed"), Some(&Value::Bool(false)));
        let request = crate::pyjson::dumps(result.get("request").unwrap(), None);
        assert!(!request.contains("CUDA") && !request.contains("entrypoint.sh"));
        assert!(!request.contains("API_KEY") && !request.contains("ACCESS_TOKEN"));
        if provider == "runpod" {
            assert!(request.contains("22/tcp"));
            assert!(request.contains("--volume-in-gb"));
            assert!(number(result.get("price_per_hour").unwrap()).unwrap() > 2.39);
        } else {
            assert!(request.contains("777"));
            assert!(!request.contains("gpu-amd-base"));
        }
        assert!(runner
            .calls()
            .iter()
            .all(|c| !c.argv.contains(&"create".into())));
        f.max_price = Some(1.0);
        assert_eq!(up(&ctx, &f, &cfg()).unwrap_err().exit_code(), 11);
        let mut capped = cfg();
        capped.insert("max_session_spend", Value::Int(5));
        assert_eq!(
            up(&ctx, &flags(provider, &[]), &capped)
                .unwrap_err()
                .exit_code(),
            15
        );
    }
}

#[test]
fn shared_run_dry_run_keeps_upload_checks_and_never_uses_vast() {
    for provider in ["runpod", "do"] {
        let t = TestEnv::new();
        let runner = FakeRunner::new(fixture);
        let ctx = t.ctx().runner(&runner).build().for_command(true);
        std::fs::write(t.root.join("job.py"), "print('ok')").unwrap();
        let f = testing::run_flags(
            &[
                "--provider",
                provider,
                "--gpu",
                "MI300X",
                "--dry-run",
                "--no-setup",
            ],
            &["python", "job.py"],
        );
        crate::run::run(&ctx, &f, &cfg(), &t.root).unwrap();
        assert_eq!(
            ctx.result_snapshot().get("created"),
            Some(&Value::Bool(false))
        );
        assert!(runner
            .calls()
            .iter()
            .all(|c| ["runpodctl", "doctl", "git"].contains(&c.program())
                && !c.argv.contains(&"create".into())));
        std::fs::write(t.root.join(".env"), "SYNTHETIC=secret").unwrap();
        assert!(crate::run::run(&ctx, &f, &cfg(), &t.root).is_err());
    }
}

#[test]
fn inventory_whitelists_fields_and_does_not_mistake_desired_running_for_ready() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(fixture);
    let ctx = t.ctx().runner(&runner).build();
    let list = list(&ctx, Provider::Runpod).unwrap();
    assert_eq!(
        list[0].get("actual_status"),
        Some(&Value::str("initializing"))
    );
    assert!(!crate::pyjson::dumps(&Value::Dict(list[0].clone()), None)
        .contains("synthetic-private-canary"));
}

#[test]
fn copy_exec_and_editor_use_direct_safe_ssh_alias() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|cmd| {
        if cmd.program() == "scp" {
            return testing::ok("");
        }
        fixture(cmd)
    });
    let ctx = t.ctx().runner(&runner).build().for_command(true);
    crate::remote::copy(
        &ctx,
        "runpod:pod-test",
        "./hello",
        ":/workspace/hello",
        false,
        3.0,
    )
    .unwrap();
    let calls = runner.calls();
    let scp = calls.iter().find(|c| c.program() == "scp").unwrap();
    assert!(scp
        .argv
        .contains(&"runpod-pod-test:/workspace/hello".into()));
    assert!(scp.argv.contains(&"-s".into()));
    let config = std::fs::read_to_string(t.home.join(".ssh/gpu_amd/runpod-pod-test")).unwrap();
    assert!(config.contains("HostName 203.0.113.4"));
    assert!(config.contains("Port 22022"));
    assert!(!config.contains("malicious"));
    crate::remote::exec(
        &ctx,
        "do:atl1:42",
        &["printf".into(), "a b;$(no)".into()],
        3.0,
    )
    .unwrap();
    assert!(runner
        .calls()
        .last()
        .unwrap()
        .argv
        .contains(&"printf 'a b;$(no)'".into()));
}

#[test]
fn proxy_or_malicious_ssh_data_never_reaches_shell_or_config() {
    for reply in [RP_ENDPOINT.replace("203.0.113.4", "host\\nProxyCommand bad"), RP_ENDPOINT.replace("22022", "0"), RP_ENDPOINT.replace("203.0.113.4", "127.0.0.1"), RP_ENDPOINT.replace("203.0.113.4", "169.254.169.254"), r#"{"id":"pod-test","runtimeStatus":"running","ssh":{"ssh_command":"ssh proxy@ssh.runpod.io"}}"#.into()] {
        let t = TestEnv::new();
        let runner = FakeRunner::new(move |cmd| {
            assert_eq!(cmd.program(), "runpodctl");
            testing::ok(&reply)
        });
        let ctx = t.ctx().runner(&runner).build();
        assert_eq!(Id::parse("runpod:pod-test").unwrap().wait_ssh(&ctx, 1.0).unwrap_err().exit_code(), 14);
        assert!(!t.home.join(".ssh/config").exists());
    }
}

#[test]
fn deletion_is_idempotent_verified_and_region_bound() {
    for (provider, id, reply) in [
        (Provider::Runpod, "runpod:pod-test", RP_ROW),
        (Provider::DigitalOcean, "do:atl1:42", DO_ROW),
    ] {
        let t = TestEnv::new();
        let lists = AtomicUsize::new(0);
        let runner = FakeRunner::new(move |cmd| {
            if cmd.argv.contains(&"delete".into()) {
                return Ok(Output::status(1));
            }
            assert!(cmd.argv.contains(&"list".into()));
            // Eventual consistency: still present once after DELETE.
            if lists.fetch_add(1, Ordering::SeqCst) < 2 {
                testing::ok(reply)
            } else {
                testing::ok("[]")
            }
        });
        let ctx = t.ctx().runner(&runner).build().for_command(true);
        Id::parse(id).unwrap().down(&ctx, &cfg(), true).unwrap();
        assert_eq!(
            ctx.result_snapshot().get("destroyed"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            runner
                .calls()
                .iter()
                .filter(|c| c.argv.contains(&"delete".into()))
                .count(),
            1
        );
        assert!(runner.calls().iter().all(|c| c.program()
            == if provider == Provider::Runpod {
                "runpodctl"
            } else {
                "doctl"
            }));
        Id::parse(id).unwrap().down(&ctx, &cfg(), true).unwrap();
        assert_eq!(
            runner
                .calls()
                .iter()
                .filter(|c| c.argv.contains(&"delete".into()))
                .count(),
            1
        );
    }
    let t = TestEnv::new();
    let runner = FakeRunner::new(fixture);
    let ctx = t.ctx().runner(&runner).build();
    assert!(Id::parse("do:wrong:42")
        .unwrap()
        .down(&ctx, &cfg(), true)
        .is_err());
    assert!(!runner
        .calls()
        .iter()
        .any(|c| c.argv.contains(&"delete".into())));
}

#[test]
fn authentication_failure_is_never_absence_or_deletion_success() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|_| {
        Ok(Output {
            code: 1,
            stdout: "synthetic-private-canary".into(),
            stderr: "synthetic-private-canary".into(),
        })
    });
    let ctx = t.ctx().runner(&runner).build().for_command(true);
    for id in ["runpod:pod-test", "do:atl1:42"] {
        let error = Id::parse(id).unwrap().down(&ctx, &cfg(), true).unwrap_err();
        assert!(!error.text().contains("synthetic-private-canary"));
        assert_eq!(
            ctx.result_snapshot().get("destroyed"),
            Some(&Value::Bool(false))
        );
    }
    assert!(runner
        .calls()
        .iter()
        .all(|c| !c.argv.contains(&"delete".into())));
}

#[test]
fn accepted_delete_without_absence_is_cleanup_unverified() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|cmd| {
        if cmd.argv.contains(&"delete".into()) {
            return testing::ok("{}");
        }
        testing::ok(RP_ROW)
    });
    let ctx = t.ctx().runner(&runner).build().for_command(true);
    assert_eq!(
        Id::parse("runpod:pod-test")
            .unwrap()
            .down(&ctx, &cfg(), true)
            .unwrap_err()
            .exit_code(),
        19
    );
    assert_eq!(
        ctx.result_snapshot().get("destroyed"),
        Some(&Value::Bool(false))
    );
}
