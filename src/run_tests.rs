//! Fake-only job lifecycle regressions. Files and archives are real; cloud/SSH are not.
use crate::cli::{Cli, Command, RunFlags};
use crate::process::{Cmd, Input};
use crate::pyjson::{self, Value};
use crate::testing::{self, FakeClock, FakeRunner, TestEnv};
use clap::Parser;
use std::sync::{Arc, Mutex};

fn flags(extra: &[&str]) -> RunFlags {
    let mut argv = vec![
        "gpu", "run", "--gpu", "RTX 4090", "--expire", "30m", "--yes",
    ];
    argv.extend_from_slice(extra);
    argv.extend(["--", "python", "train.py", "a b", "$(false)"]);
    match Cli::try_parse_from(argv).unwrap().command.unwrap() {
        Command::Run(f) => f,
        _ => unreachable!(),
    }
}

#[derive(Clone, Copy)]
enum End {
    Success,
    Failure,
    Timeout,
    Lost,
    CleanupFailure,
    Cancel,
    CancelTwice,
    UploadFailure,
    ArmFailure,
    Reconnect,
    DownloadFailure,
}

fn flow(end: End) -> (i32, Value, String, Vec<Cmd>) {
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
                    } else {
                        testing::ok(
                            r#"[{"id":123,"actual_status":"running","ssh_host":"fake","ssh_port":1234}]"#,
                        )
                    }
                }
                ["destroy", "instance"] => {
                    if matches!(end, End::CleanupFailure) {
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
                    if matches!(end, End::Cancel | End::CancelTwice) {
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
                    if matches!(end, End::Lost) || (matches!(end, End::Reconnect) && *n == 1) {
                        // Lost is detected from the next provider inventory read.
                        if matches!(end, End::Lost) {
                            *destroyed.lock().unwrap() = true;
                        }
                        return testing::status(255);
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
    let outcome = super::run::run(
        &ctx,
        &flags(&["--env", "TOKEN"]),
        &crate::config::defaults(),
        &cwd,
    );
    let code = outcome.as_ref().err().map_or(0, |e| e.exit_code());
    let result = crate::output::envelope(&ctx, "run", &outcome);
    let dir = cwd.join("gpu-runs/20261107T120000Z-123");
    assert_eq!(
        pyjson::loads(&std::fs::read_to_string(dir.join("result.json")).unwrap()).unwrap(),
        result
    );
    let r = result.get("result").unwrap();
    if !matches!(end, End::CleanupFailure) {
        assert_eq!(r.get("destroyed"), Some(&Value::Bool(true)));
    }
    if matches!(
        end,
        End::Success | End::Failure | End::Timeout | End::Cancel | End::Reconnect
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

#[test]
fn job_endings_always_download_then_verify_destroy() {
    for (end, code) in [
        (End::Success, 0),
        (End::Failure, 7),
        (End::Timeout, 17),
        (End::Lost, 18),
        (End::CleanupFailure, 19),
        (End::Cancel, 130),
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
        assert!(super::run::run(&ctx, &f, &crate::config::defaults(), &env.root).is_err());
    }
}
