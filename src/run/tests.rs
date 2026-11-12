use super::*;
use crate::testing::{self, FakeClock, FakeRunner, TestEnv};
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn cleanup_always_sends_destroy_before_any_inventory_and_retries_bad_reads() {
    for bad_reads in [0, 2] {
        let env = TestEnv::new();
        let shows = Arc::new(AtomicUsize::new(0));
        let calls = shows.clone();
        let runner = FakeRunner::new(move |cmd| match cmd.argv.get(1).map(String::as_str) {
            Some("destroy") => testing::ok(""),
            Some("show") => {
                if calls.fetch_add(1, Ordering::SeqCst) < bad_reads {
                    testing::ok("<html>502</html>")
                } else {
                    testing::ok("[]")
                }
            }
            _ => panic!("{:?}", cmd.argv),
        });
        let ctx = env.ctx().runner(&runner).build();
        crate::vast::ssh_config::update_host(
            &ctx.env,
            &testing::dict(r#"{"id":123,"ssh_host":"fake","ssh_port":22}"#),
        )
        .unwrap();
        crate::vast::instance::destroy_created(&ctx, "123").unwrap();
        assert_eq!(
            &runner.calls()[0].argv,
            &["vastai", "destroy", "instance", "123", "--yes"]
        );
        assert_eq!(shows.load(Ordering::SeqCst), bad_reads + 1);
        assert!(!crate::vast::ssh_config::has_host(&ctx.env, "123").unwrap());
    }
}

#[test]
fn unverified_cleanup_never_claims_success_and_still_removes_alias() {
    let env = TestEnv::new();
    let clock = FakeClock::now_real();
    let runner = FakeRunner::new(|cmd| {
        if cmd.argv[1] == "destroy" {
            testing::ok("")
        } else {
            testing::ok("garbage")
        }
    });
    let ctx = env.ctx().runner(&runner).clock(&clock).build();
    crate::vast::ssh_config::update_host(
        &ctx.env,
        &testing::dict(r#"{"id":123,"ssh_host":"fake","ssh_port":22}"#),
    )
    .unwrap();
    assert_eq!(
        crate::vast::instance::destroy_created(&ctx, "123")
            .unwrap_err()
            .exit_code(),
        19
    );
    assert_eq!(clock.elapsed(), 60.0);
    assert!(!crate::vast::ssh_config::has_host(&ctx.env, "123").unwrap());
}

#[test]
fn cleanup_resends_destroy_while_the_instance_stays_listed() {
    let env = TestEnv::new();
    let clock = FakeClock::now_real();
    let sends = Arc::new(AtomicUsize::new(0));
    let counter = sends.clone();
    let runner = FakeRunner::new(move |cmd| {
        if cmd.argv[1] == "destroy" {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                return testing::status(1);
            }
            return testing::ok("");
        }
        if sends.load(Ordering::SeqCst) < 2 {
            testing::ok(r#"[{"id":123,"actual_status":"running"}]"#)
        } else {
            testing::ok("[]")
        }
    });
    let ctx = env.ctx().runner(&runner).clock(&clock).build();
    crate::vast::instance::destroy_created(&ctx, "123").unwrap();
    let destroys = runner
        .calls()
        .iter()
        .filter(|cmd| cmd.argv[1] == "destroy")
        .count();
    assert_eq!(destroys, 2);
    assert!(clock.elapsed() < 60.0);
}

#[test]
fn bounded_cleanup_resends_destroy_a_fixed_number_of_times() {
    let env = TestEnv::new();
    let clock = FakeClock::now_real();
    let runner = FakeRunner::new(|cmd| {
        if cmd.argv[1] == "destroy" {
            testing::status(1)
        } else {
            testing::ok(r#"[{"id":123,"actual_status":"running"}]"#)
        }
    });
    let ctx = env.ctx().runner(&runner).clock(&clock).build();
    assert_eq!(
        crate::vast::instance::destroy_created(&ctx, "123")
            .unwrap_err()
            .exit_code(),
        19
    );
    let destroys = runner
        .calls()
        .iter()
        .filter(|cmd| cmd.argv[1] == "destroy")
        .count();
    assert_eq!(destroys, 3);
    assert!(clock.elapsed() <= 60.0 + 1e-9);
}

#[test]
fn remote_step_errors_include_status_and_bounded_safe_cause() {
    let env = TestEnv::new();
    let ctx = env.ctx().build();
    ctx.protect_secret("very-private-value");
    let output = Output::failed(
        28,
        format!(
            "{}\nDisk full, very-private-value api_key=another-secret",
            "x".repeat(9000)
        ),
    );
    let error = step_result(&ctx, Ok(output), "Upload failed.").unwrap_err();
    assert!(error.text().contains("Exit status 28") && error.text().contains("Disk full"));
    assert!(
        !error.text().contains("very-private-value") && !error.text().contains("another-secret")
    );
    assert!(error.text().len() < 2200);
    let error = step_result(
        &ctx,
        Err(crate::process::RunError::TimedOut),
        "Upload failed.",
    )
    .unwrap_err();
    assert!(error.text().contains("timed out"));
}

fn flags(extra: &[&str]) -> RunFlags {
    let mut options = vec!["--yes"];
    options.extend_from_slice(extra);
    crate::testing::run_flags(&options, &["true"])
}

#[test]
fn unwritable_result_destination_fails_before_paid_create_and_names_path() {
    let env = TestEnv::new();
    let cwd = env.root.join("project");
    fs::create_dir(&cwd).unwrap();
    fs::write(cwd.join("train.py"), b"pass").unwrap();
    fs::write(cwd.join("gpu-runs"), b"not a directory").unwrap();
    let runner = FakeRunner::new(|cmd| match cmd.program() {
        "git" => testing::status(128),
        "vastai" if cmd.argv[1] == "search" => {
            testing::ok(r#"[{"id":42,"gpu_name":"T4","dph_total":0.1,"num_gpus":1}]"#)
        }
        _ => panic!("paid request before result preflight: {:?}", cmd.argv),
    });
    let ctx = env.ctx().runner(&runner).build();
    let error = run(&ctx, &flags(&[]), &crate::config::defaults(), &cwd).unwrap_err();
    assert!(error
        .text()
        .contains(cwd.join("gpu-runs").to_str().unwrap()));
    assert!(runner
        .calls()
        .iter()
        .all(|cmd| !cmd.argv.iter().any(|word| word == "create")));
}

#[test]
fn secret_looking_uploads_require_explicit_override_before_offer_search() {
    for path in [
        ".env",
        ".env.local",
        ".envrc",
        "id_rsa",
        ".netrc",
        ".aws/credentials",
        ".ssh/id_ed25519",
        "account.pem",
        "credentials-prod.json",
    ] {
        let env = TestEnv::new();
        let cwd = env.root.join("project");
        fs::create_dir(&cwd).unwrap();
        let secret = cwd.join(path);
        fs::create_dir_all(secret.parent().unwrap()).unwrap();
        fs::write(&secret, b"secret").unwrap();
        let runner = FakeRunner::new(|cmd| match cmd.program() {
            "git" => testing::status(128),
            "vastai" if cmd.argv[1] == "search" => {
                testing::ok(r#"[{"id":42,"gpu_name":"T4","dph_total":0.1,"num_gpus":1}]"#)
            }
            _ => panic!("{:?}", cmd.argv),
        });
        let ctx = env.ctx().runner(&runner).build().for_command(true);
        let error = run(
            &ctx,
            &flags(&["--dry-run"]),
            &crate::config::defaults(),
            &cwd,
        )
        .unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.text().contains(path));
        assert!(runner.calls().iter().all(|cmd| cmd.program() == "git"));
        run(
            &ctx,
            &flags(&["--dry-run", "--allow-secrets"]),
            &crate::config::defaults(),
            &cwd,
        )
        .unwrap();
        assert_eq!(
            ctx.result_snapshot().get("files"),
            Some(&Value::List(vec![Value::str(path)]))
        );
        assert_eq!(
            testing::err(&ctx)
                .matches("WARNING: uploading secret-looking files")
                .count(),
            1
        );
    }
}

#[test]
fn committed_env_templates_upload_without_override_or_warning() {
    for path in [".env.example", ".env.sample", ".env.template", ".env.dist"] {
        let env = TestEnv::new();
        let cwd = env.root.join("project");
        fs::create_dir(&cwd).unwrap();
        fs::write(cwd.join(path), b"KEY=").unwrap();
        let runner = FakeRunner::new(|cmd| match cmd.program() {
            "git" => testing::status(128),
            "vastai" if cmd.argv[1] == "search" => {
                testing::ok(r#"[{"id":42,"gpu_name":"T4","dph_total":0.1,"num_gpus":1}]"#)
            }
            _ => panic!("{:?}", cmd.argv),
        });
        let ctx = env.ctx().runner(&runner).build().for_command(true);
        run(
            &ctx,
            &flags(&["--dry-run"]),
            &crate::config::defaults(),
            &cwd,
        )
        .unwrap();
        assert_eq!(
            ctx.result_snapshot().get("files"),
            Some(&Value::List(vec![Value::str(path)]))
        );
        assert!(!testing::err(&ctx).contains("WARNING"));
    }
}

#[test]
fn results_download_retries_then_unpacks_without_implicit_linger() {
    let env = TestEnv::new();
    let local = env.root.join("results");
    fs::create_dir(&local).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let runner = FakeRunner::new(move |cmd| {
        if counter.fetch_add(1, Ordering::SeqCst) < 2 {
            return testing::status(255);
        }
        let mut tar =
            tar::Builder::new(fs::File::create(cmd.stdout_file.as_ref().unwrap()).unwrap());
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o600);
        header.set_size(2);
        header.set_cksum();
        tar.append_data(&mut header, "outputs/result", &b"ok"[..])
            .unwrap();
        tar.finish().unwrap();
        testing::ok("")
    });
    let ctx = env.ctx().runner(&runner).build();
    download(
        &ctx,
        &VastBackend {
            alias: "vast-123".into(),
            ..VastBackend::default()
        },
        "/root/state",
        "/workspace/job",
        "outputs",
        &local,
        ctx.clock.now() + jiff::Span::new().minutes(5),
    )
    .unwrap();
    assert_eq!(fs::read(local.join("outputs/result")).unwrap(), b"ok");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(flags(&[]).linger, "0");
}

fn download_with(runner: &Arc<FakeRunner>, clock: &Arc<FakeClock>, env: &TestEnv) -> Result<()> {
    let local = env.root.join("results");
    fs::create_dir_all(&local).unwrap();
    let ctx = env.ctx().runner(runner).clock(clock).build();
    download(
        &ctx,
        &VastBackend {
            alias: "vast-123".into(),
            ..VastBackend::default()
        },
        "/root/state",
        "/workspace/job",
        "outputs",
        &local,
        ctx.clock.now() + jiff::Span::new().hours(2),
    )
}

#[test]
fn remote_tar_failure_is_not_retried() {
    let env = TestEnv::new();
    let clock = FakeClock::now_real();
    let runner = FakeRunner::new(|_| testing::status(2));
    let error = download_with(&runner, &clock, &env).unwrap_err();
    assert!(error.text().contains("Exit status 2"));
    assert_eq!(runner.calls().len(), 1);
    assert_eq!(clock.elapsed(), 0.0);
}

#[test]
fn persistent_transport_failure_stops_after_a_small_budget() {
    let env = TestEnv::new();
    let clock = FakeClock::now_real();
    let runner = FakeRunner::new(|_| testing::status(255));
    download_with(&runner, &clock, &env).unwrap_err();
    assert_eq!(runner.calls().len(), DOWNLOAD_RETRY_DELAYS.len() + 1);
    assert!(clock.elapsed() <= 120.0);
}

mod expiry_prompt;
mod lifecycle;
