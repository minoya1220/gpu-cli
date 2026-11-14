//! Fake-only job lifecycle regressions. Files and archives are real; cloud/SSH are not.
use crate::cli::RunFlags;
use crate::process::{Cmd, Input};
use crate::pyjson::{self, Value};
use crate::testing::{self, FakeClock, FakeRunner, TestEnv};
use std::sync::{Arc, Mutex};

fn flags(extra: &[&str]) -> RunFlags {
    let mut options = vec!["--gpu", "RTX 4090", "--yes"];
    options.extend_from_slice(extra);
    crate::testing::run_flags(&options, &["python", "train.py", "a b", "$(false)"])
}

#[derive(Clone, Copy)]
enum End {
    Success,
    Failure,
    Timeout,
    Lost,
    SupervisorLost,
    CleanupFailure,
    Cancel,
    CancelFlakyStatus,
    CancelTwice,
    UploadFailure,
    ArmFailure,
    Reconnect,
    DownloadFailure,
    WaitTimeout,
    WaitTimeoutCleanupFailure,
}

fn flow(end: End) -> (i32, Value, String, Vec<Cmd>) {
    flow_with_flags(end, flags(&["--env", "TOKEN"]))
}

fn flow_with_flags(end: End, flags: RunFlags) -> (i32, Value, String, Vec<Cmd>) {
    flow_with_statuses(end, flags, Vec::new())
}

fn flow_with_statuses(
    end: End,
    flags: RunFlags,
    statuses: Vec<String>,
) -> (i32, Value, String, Vec<Cmd>) {
    let env = TestEnv::new();
    let cwd = env.root.join("project");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("train.py"), "print('hi')").unwrap();
    std::fs::write(cwd.join("requirements.txt"), "").unwrap();
    let destroyed = Arc::new(Mutex::new(false));
    let status_calls = Arc::new(Mutex::new(0));
    let holder = Arc::new(Mutex::new(None::<crate::ctx::Ctx>));
    let holder2 = holder.clone();
    let runner = FakeRunner::new(move |cmd| {
        assert!(
            !cmd.argv.iter().any(|s| s.contains("test-secret")),
            "secret in argv"
        );
        match cmd.program() {
            "git" => testing::status(128),
            "vastai" => match cmd.argv[1..3]
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .as_slice()
            {
                ["search", "offers"] => testing::ok(
                    r#"[{"id":100,"gpu_name":"RTX 4090","dph_total":0.3,"num_gpus":1,"reliability":0.99}]"#,
                ),
                ["create", "instance"] => {
                    assert!(cmd.argv.iter().any(|s| s == "--cancel-unavail"));
                    testing::ok(r#"{"success":true,"new_contract":123}"#)
                }
                ["show", "instances"] => {
                    if *destroyed.lock().unwrap() {
                        testing::ok("[]")
                    } else if matches!(end, End::WaitTimeout | End::WaitTimeoutCleanupFailure) {
                        testing::ok(r#"[{"id":123,"actual_status":"loading"}]"#)
                    } else {
                        testing::ok(
                            r#"[{"id":123,"actual_status":"running","ssh_host":"fake","ssh_port":1234}]"#,
                        )
                    }
                }
                ["destroy", "instance"] => {
                    if matches!(end, End::CleanupFailure | End::WaitTimeoutCleanupFailure) {
                        return testing::status(1);
                    }
                    *destroyed.lock().unwrap() = true;
                    testing::ok("")
                }
                _ => panic!("unexpected {:?}", cmd.argv),
            },
            "ssh" => {
                let remote = cmd.argv.last().unwrap();
                if remote == "true" {
                    return testing::ok("");
                }
                if matches!(&cmd.stdin, Input::Text(t) if t.starts_with("GPU_EXPIRE_SOURCE=")) {
                    return if matches!(end, End::ArmFailure) {
                        testing::status(1)
                    } else {
                        testing::ok(r#"{"status":"armed","deadline":2000000000}"#)
                    };
                }
                if let Input::File(path) = &cmd.stdin {
                    let gzip = flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap());
                    let mut tar = tar::Archive::new(gzip);
                    let files: Vec<_> = tar
                        .entries()
                        .unwrap()
                        .map(|e| e.unwrap().path().unwrap().into_owned())
                        .collect();
                    assert_eq!(files.len(), 2);
                    return if matches!(end, End::UploadFailure) {
                        testing::status(1)
                    } else {
                        testing::ok("")
                    };
                }
                if matches!(&cmd.stdin, Input::Text(t) if t.starts_with("GPU_RUN_SOURCE=")) {
                    let Input::Text(payload) = &cmd.stdin else {
                        unreachable!()
                    };
                    assert!(payload.contains("test-secret"));
                    assert!(payload.contains("uv venv --system-site-packages"));
                    if matches!(end, End::Cancel | End::CancelFlakyStatus | End::CancelTwice) {
                        let lock = holder2.lock().unwrap();
                        let ctx = lock.as_ref().unwrap();
                        ctx.interrupt();
                        if matches!(end, End::CancelTwice) {
                            ctx.interrupt();
                        }
                    }
                    return testing::ok("started\n");
                }
                if remote.contains(" status ") {
                    let mut n = status_calls.lock().unwrap();
                    *n += 1;
                    if let Some(status) = statuses.get(*n - 1) {
                        return testing::ok(status);
                    }
                    if matches!(end, End::Lost) || (matches!(end, End::Reconnect) && *n == 1) {
                        // Lost is detected from the next provider inventory read.
                        if matches!(end, End::Lost) {
                            *destroyed.lock().unwrap() = true;
                        }
                        return testing::status(255);
                    }
                    if matches!(end, End::SupervisorLost) {
                        return testing::ok("lost\n");
                    }
                    if matches!(end, End::CancelFlakyStatus) && *n <= 2 {
                        return if *n == 1 {
                            testing::status(255)
                        } else {
                            testing::ok("job\t-\t0\t12\n")
                        };
                    }
                    let (code, timed) = match end {
                        End::Failure => (7, 0),
                        End::Timeout => (17, 1),
                        _ => (0, 0),
                    };
                    return testing::ok(&format!("job\t{code}\t{timed}\t12\n"));
                }
                if remote.contains(" stop ") {
                    return testing::ok("");
                }
                if remote.starts_with("dd ") {
                    std::fs::write(cmd.stdout_file.as_ref().unwrap(), b"hello world\n").unwrap();
                    return testing::ok("");
                }
                if remote.starts_with("tar -C ") {
                    if matches!(end, End::DownloadFailure) {
                        return testing::status(255);
                    }
                    let mut tar = tar::Builder::new(
                        std::fs::File::create(cmd.stdout_file.as_ref().unwrap()).unwrap(),
                    );
                    let mut header = tar::Header::new_gnu();
                    header.set_size(6);
                    header.set_mode(0o600);
                    header.set_cksum();
                    tar.append_data(&mut header, "outputs/result.txt", &b"result"[..])
                        .unwrap();
                    tar.finish().unwrap();
                    return testing::ok("");
                }
                panic!("unexpected SSH {:?}", cmd.argv);
            }
            _ => panic!("unexpected tool {:?}", cmd.argv),
        }
    });
    let mut builder = env
        .ctx()
        .runner(&runner)
        .clock(&FakeClock::at("2026-11-07T12:00:00Z".parse().unwrap()));
    builder.env = builder.env.with("TOKEN", "test-secret");
    let ctx = builder.build().for_command(true);
    *holder.lock().unwrap() = Some(ctx.clone());
    let outcome = crate::run::run(&ctx, &flags, &crate::config::defaults(), &cwd);
    let code = outcome.as_ref().err().map_or(0, |e| e.exit_code());
    let result = crate::output::envelope(&ctx, "run", &outcome);
    let dir = cwd.join("gpu-runs/20261107T120000Z-123");
    assert_eq!(
        pyjson::loads(&std::fs::read_to_string(dir.join("result.json")).unwrap()).unwrap(),
        result
    );
    let r = result.get("result").unwrap();
    if !matches!(end, End::CleanupFailure | End::WaitTimeoutCleanupFailure) {
        assert_eq!(r.get("destroyed"), Some(&Value::Bool(true)));
    }
    if matches!(
        end,
        End::Success
            | End::Failure
            | End::Timeout
            | End::Cancel
            | End::CancelFlakyStatus
            | End::Reconnect
    ) {
        assert_eq!(
            std::fs::read(dir.join("outputs/result.txt")).unwrap(),
            b"result"
        );
        assert_eq!(
            std::fs::read(dir.join("log.txt")).unwrap(),
            b"hello world\n"
        );
    }
    if matches!(end, End::CancelTwice) {
        assert!(!dir.join("outputs").exists());
    }
    let logs = testing::err(&ctx);
    assert!(!logs.contains("test-secret"));
    (code, result, logs, runner.calls())
}

// Generate legal setup -> job -> terminal traces, with malformed reads between
// states. The real run loop must reconnect without changing the final outcome,
// duplicating logs, or skipping download/verified cleanup (all tools are fakes).
proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(16))]

    #[test]
    fn generated_run_transitions_preserve_outcome_and_cleanup(
        setup in proptest::collection::vec(proptest::bool::ANY, 0..3),
        job in proptest::collection::vec(proptest::bool::ANY, 0..3),
        ending in 0u8..3
    ) {
        let mut statuses = Vec::new();
        for (phase, reads) in [("setup", setup), ("job", job)] {
            for malformed in reads {
                if malformed {
                    statuses.push("unavailable".into());
                }
                statuses.push(format!("{phase}\t-\t0\t0\n"));
            }
        }
        let (end, expected) = match ending {
            0 => (End::Success, 0),
            1 => (End::Failure, 7),
            _ => (End::Timeout, 17),
        };
        let (code, result, _, calls) = flow_with_statuses(end, flags(&["--env", "TOKEN"]), statuses);
        proptest::prop_assert_eq!(code, expected);
        let result = result.get("result").unwrap();
        proptest::prop_assert_eq!(result.get("destroyed"), Some(&Value::Bool(true)));
        proptest::prop_assert_eq!(result.get("phase"), Some(&Value::str(if expected == 0 { "done" } else { "job" })));
        let download = calls.iter().position(|c| c.program() == "ssh" && c.argv.last().unwrap().starts_with("tar -C ")).unwrap();
        let destroy = calls.iter().position(|c| c.argv.starts_with(&["vastai".into(), "destroy".into()])).unwrap();
        proptest::prop_assert!(download < destroy);
        proptest::prop_assert!(calls[destroy + 1..].iter().any(|c| c.argv.starts_with(&["vastai".into(), "show".into(), "instances".into()])));
    }
}

#[test]
fn job_endings_always_download_then_verify_destroy() {
    for (end, code) in [
        (End::Success, 0),
        (End::Failure, 7),
        (End::Timeout, 17),
        (End::Lost, 18),
        (End::SupervisorLost, 18),
        (End::CleanupFailure, 19),
        (End::Cancel, 130),
        (End::CancelFlakyStatus, 130),
        (End::CancelTwice, 130),
        (End::UploadFailure, 1),
        (End::ArmFailure, 1),
        (End::DownloadFailure, 1),
    ] {
        let (actual, _, _, calls) = flow(end);
        assert_eq!(actual, code);
        assert!(calls.iter().any(|c| c.argv.starts_with(&[
            "vastai".into(),
            "show".into(),
            "instances".into()
        ])));
    }
}

#[test]
fn readiness_timeout_after_verified_cleanup_suggests_retry_not_reconnecting() {
    let (code, result, _, _) = flow_with_flags(
        End::WaitTimeout,
        flags(&["--env", "TOKEN", "--timeout", "2"]),
    );
    assert_eq!(code, 14);
    let error = result.get("error").unwrap();
    assert_eq!(error.get("code"), Some(&Value::str("wait_timeout")));
    let message = error.get("message").unwrap().as_str().unwrap();
    assert!(message.contains("Instance 123 was destroyed."), "{message}");
    assert!(message.contains("Next: retry gpu run"), "{message}");
    assert!(!message.contains("gpu wait"), "{message}");
    assert!(!message.contains("limited to"), "{message}");
    assert_eq!(
        result.get("result").unwrap().get("destroyed"),
        Some(&Value::Bool(true))
    );
    // An unverified destroy still takes precedence and must not claim success.
    let (code, result, _, _) = flow_with_flags(
        End::WaitTimeoutCleanupFailure,
        flags(&["--env", "TOKEN", "--timeout", "2"]),
    );
    assert_eq!(code, 19);
    assert!(!result
        .get("error")
        .unwrap()
        .get("message")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("was destroyed"));
}

#[test]
fn expiry_limited_readiness_reports_the_cap_before_wait_and_in_the_error() {
    let (code, result, stderr, _) = flow_with_flags(
        End::WaitTimeout,
        flags(&["--env", "TOKEN", "--expire", "15m"]),
    );
    assert_eq!(code, 14);
    let message = result
        .get("error")
        .unwrap()
        .get("message")
        .unwrap()
        .as_str()
        .unwrap();
    for text in [
        "SSH wait limited to 300s by --expire",
        "requested --timeout 600s",
        "10m reserved for results",
        "Use a longer --expire",
    ] {
        assert!(message.contains(text), "{message}");
        assert!(stderr.contains(text), "{stderr}");
    }
}

#[test]
fn nonterminal_run_requires_gpu_even_with_an_explicit_offer() {
    let env = TestEnv::new();
    let runner = FakeRunner::new(|cmd| panic!("unexpected {:?}", cmd.argv));
    let ctx = env.ctx().runner(&runner).build();
    let mut args = flags(&[]);
    args.gpu = None;
    args.offer = Some(100);
    let error = crate::run::run(&ctx, &args, &crate::config::defaults(), &env.root).unwrap_err();
    assert_eq!(error.exit_code(), 2);
    assert!(
        error.text().contains("requires --gpu MODEL"),
        "{}",
        error.text()
    );
    assert!(runner.calls().is_empty());
}

#[test]
fn reconnect_resumes_log_without_duplicates_and_json_stays_clean() {
    let (code, result, logs, _) = flow(End::Reconnect);
    assert_eq!(code, 0);
    assert!(logs.contains("reconnecting"));
    assert_eq!(logs.matches("hello world").count(), 1);
    assert_eq!(result.get("ok"), Some(&Value::Bool(true)));
}

#[test]
fn invalid_preflight_never_searches_or_creates() {
    let env = TestEnv::new();
    let ctx = env.ctx().build();
    for f in [
        flags(&["--env", "MISSING"]),
        flags(&["--max-upload", "0"]),
        flags(&["--outputs", "../escape"]),
        flags(&["--expire", "5m"]),
    ] {
        assert!(crate::run::run(&ctx, &f, &crate::config::defaults(), &env.root).is_err());
    }
}

#[test]
fn attached_download_restores_the_full_deadline_after_the_linger_hook() {
    let (code, _, _, calls) = flow(End::Success);
    assert_eq!(code, 0);
    let arms: Vec<_> = calls
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(&c.stdin, Input::Text(t) if t.starts_with("GPU_EXPIRE_SOURCE=")))
        .collect();
    assert_eq!(arms.len(), 2);
    assert_eq!(arms[0].1.argv, arms[1].1.argv);
    let position = |prefix: &str| {
        calls
            .iter()
            .position(|c| c.program() == "ssh" && c.argv.last().unwrap().starts_with(prefix))
            .unwrap()
    };
    let finished = calls
        .iter()
        .rposition(|c| c.program() == "ssh" && c.argv.last().unwrap().contains(" status "))
        .unwrap();
    assert!(finished < arms[1].0 && arms[1].0 < position("tar -C "));
}

#[test]
fn cancelled_download_rearms_only_after_the_final_status_despite_status_failures() {
    let (code, _, _, calls) = flow(End::CancelFlakyStatus);
    assert_eq!(code, 130);
    let status = |c: &Cmd| c.program() == "ssh" && c.argv.last().unwrap().contains(" status ");
    let rearm = calls
        .iter()
        .rposition(|c| matches!(&c.stdin, Input::Text(t) if t.starts_with("GPU_EXPIRE_SOURCE=")))
        .unwrap();
    assert_eq!(calls[..rearm].iter().filter(|c| status(c)).count(), 3);
    let download = calls
        .iter()
        .position(|c| c.program() == "ssh" && c.argv.last().unwrap().starts_with("tar -C "))
        .unwrap();
    assert!(rearm < download);
}

#[test]
fn pre_start_failures_do_not_wait_for_a_supervisor() {
    for end in [End::UploadFailure, End::SupervisorLost] {
        let (_, _, _, calls) = flow(end);
        let status = calls
            .iter()
            .filter(|c| c.program() == "ssh" && c.argv.last().unwrap().contains(" status "))
            .count();
        let stops = calls
            .iter()
            .filter(|c| c.program() == "ssh" && c.argv.last().unwrap().contains(" stop "))
            .count();
        match end {
            End::UploadFailure => assert_eq!((status, stops), (1, 0)),
            _ => assert_eq!((status, stops), (3, 1)),
        }
    }
}
