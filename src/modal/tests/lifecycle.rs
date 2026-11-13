//! Offline live-path regressions: every external call is a FakeRunner call.
use crate::modal::*;
use crate::process::Output;
use crate::testing::{self, FakeClock, FakeRunner, TestEnv};
use std::sync::{Arc, Mutex};

fn flags() -> RunFlags {
    crate::testing::run_flags(
        &["--provider", "modal", "--yes", "--env", "TOKEN"],
        &["python", "job.py"],
    )
}
const RATES: &str = r#"{"gpu_hour_cost_t4":"0.59","cpu_hour_cost_sandbox":"0.1419","mem_gib_hour_cost_sandbox":"0.024"}"#;

#[derive(Clone, Copy, Debug)]
enum End {
    Success,
    Failure,
    Timeout,
    Lost,
    UploadFailure,
    CleanupFailure,
    VolumeCleanupFailure,
    Cancel,
    CancelTwice,
    DownloadFailure,
    LostDuringDownload,
}

fn write_outputs(path: &std::path::Path) -> std::result::Result<Output, RunError> {
    let mut tar = tar::Builder::new(std::fs::File::create(path).unwrap());
    let mut header = tar::Header::new_gnu();
    header.set_size(3);
    header.set_mode(0o600);
    header.set_cksum();
    tar.append_data(&mut header, "outputs/result", &b"ok\n"[..])
        .unwrap();
    tar.finish().unwrap();
    testing::ok("")
}

#[test]
fn shared_run_flow_over_modal_exec() {
    for end in [
        End::Success,
        End::Failure,
        End::Timeout,
        End::Lost,
        End::UploadFailure,
        End::CleanupFailure,
        End::VolumeCleanupFailure,
        End::Cancel,
        End::CancelTwice,
        End::DownloadFailure,
        End::LostDuringDownload,
    ] {
        let t = TestEnv::new();
        let cwd = t.root.join("source");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(cwd.join("job.py"), "print('hi')").unwrap();
        let holder = Arc::new(Mutex::new(None::<Ctx>));
        let control = holder.clone();
        let tag = Arc::new(Mutex::new(String::new()));
        let tag_copy = tag.clone();
        let runner = FakeRunner::new(move |cmd| {
            assert!(!format!("{:?}", cmd.argv).contains("private-canary"));
            if cmd.program() == "git" {
                return testing::status(128);
            }
            assert_eq!(cmd.program(), "uv");
            assert!(std::path::Path::new(&cmd.argv[4]).is_file());
            assert!(cmd.timeout.is_some());
            match cmd.argv[5].as_str() {
                "rates" => testing::ok(RATES),
                "create" => {
                    let Input::Text(spec) = &cmd.stdin else {
                        panic!()
                    };
                    assert!(!spec.contains("private-canary"));
                    let spec = pyjson::loads(spec).unwrap();
                    assert_eq!(spec.get("timeout"), Some(&Value::Int(1800)));
                    let tags = spec.get("tags").unwrap();
                    *tag_copy.lock().unwrap() =
                        tags.get("run_id").unwrap().as_str().unwrap().into();
                    assert_eq!(tags.get("gpu-cli"), Some(&Value::str("true")));
                    testing::ok(r#"{"id":"sb-offline"}"#)
                }
                "exec" => {
                    assert_eq!(&cmd.argv[6..10], ["sb-offline", "--", "bash", "-c"]);
                    assert!(!cmd.normalize_newlines);
                    let remote = cmd.argv.last().unwrap();
                    if let Input::File(path) = &cmd.stdin {
                        let gzip = flate2::read::GzDecoder::new(std::fs::File::open(path).unwrap());
                        assert_eq!(tar::Archive::new(gzip).entries().unwrap().count(), 1);
                        return if matches!(end, End::UploadFailure) {
                            testing::status(1)
                        } else {
                            testing::ok("")
                        };
                    }
                    if let Input::Text(payload) = &cmd.stdin {
                        assert!(remote.starts_with("bash -s -- start "));
                        assert!(payload.contains("private-canary"));
                        assert!(payload.contains("WAIT_FOR_STARTUP=0"));
                        assert!(payload.contains("touch /tmp/gpu-modal-keeper/started"));
                        let guard = control.lock().unwrap();
                        let ctx = guard.as_ref().unwrap();
                        if matches!(end, End::Cancel | End::CancelTwice) {
                            ctx.interrupt();
                        }
                        if matches!(end, End::CancelTwice) {
                            ctx.interrupt();
                        }
                        return testing::ok("started\n");
                    }
                    if remote.contains(" status ") {
                        if matches!(end, End::Lost) {
                            return testing::status(255);
                        }
                        let (rc, timed) = match end {
                            End::Failure => (7, 0),
                            End::Timeout => (17, 1),
                            _ => (0, 0),
                        };
                        return testing::ok(&format!("job\t{rc}\t{timed}\t4\n"));
                    }
                    if remote.contains(" stop ") || remote.starts_with("printf ") {
                        return testing::ok("");
                    }
                    if remote.starts_with("tar -C ") {
                        assert!(remote
                            .contains(" -cf - --exclude=outputs/./.gpu-cli-claimed -- outputs/. "));
                        if matches!(end, End::DownloadFailure | End::LostDuringDownload) {
                            return testing::status(255);
                        }
                        return write_outputs(cmd.stdout_file.as_ref().unwrap());
                    }
                    if remote.starts_with("dd ") {
                        std::fs::write(cmd.stdout_file.as_ref().unwrap(), b"hi\r\n").unwrap();
                        return testing::ok("");
                    }
                    panic!("unexpected exec {cmd:?}");
                }
                "volume-get" => {
                    assert!(matches!(end, End::Lost | End::LostDuringDownload));
                    assert!(cmd.argv[6].starts_with("gpu-cli-run-"));
                    assert_eq!(cmd.argv[7], "outputs");
                    write_outputs(cmd.stdout_file.as_ref().unwrap())
                }
                "volume-delete" => {
                    assert!(!matches!(end, End::CleanupFailure));
                    assert!(cmd.argv[6].starts_with("gpu-cli-run-"));
                    testing::ok(if matches!(end, End::VolumeCleanupFailure) {
                        r#"{"finished":false}"#
                    } else {
                        r#"{"finished":true}"#
                    })
                }
                "status" => testing::ok(if matches!(end, End::Lost | End::LostDuringDownload) {
                    r#"{"running":false,"returncode":137}"#
                } else {
                    r#"{"running":true,"returncode":null}"#
                }),
                "terminate" => testing::ok(if matches!(end, End::CleanupFailure) {
                    r#"{"finished":false}"#
                } else {
                    r#"{"finished":true}"#
                }),
                _ => panic!("unexpected helper {cmd:?}"),
            }
        });
        let mut builder = t.ctx().runner(&runner).clock(&FakeClock::now_real());
        builder.env = builder.env.with("TOKEN", "private-canary");
        let ctx = builder.build().for_command(true);
        *holder.lock().unwrap() = Some(ctx.clone());
        let outcome = crate::run::run(&ctx, &flags(), &crate::config::defaults(), &cwd);
        let code = outcome.as_ref().err().map_or(0, Error::exit_code);
        assert_eq!(
            code,
            match end {
                End::Success => 0,
                End::Failure => 7,
                End::Timeout => 17,
                End::Lost | End::LostDuringDownload => 18,
                End::CleanupFailure | End::VolumeCleanupFailure => 19,
                End::Cancel | End::CancelTwice => 130,
                _ => 1,
            },
            "{end:?}: {outcome:?}"
        );
        let result = ctx.result_snapshot();
        assert_eq!(result.get("id"), Some(&Value::str("modal:sb-offline")));
        assert_eq!(result.get("expiry_armed"), Some(&Value::Bool(true)));
        assert_eq!(
            result.get("destroyed"),
            Some(&Value::Bool(!matches!(
                end,
                End::CleanupFailure | End::VolumeCleanupFailure
            )))
        );
        assert_eq!(
            result.get("run_id"),
            Some(&Value::str(tag.lock().unwrap().clone()))
        );
        let local = cwd
            .join("gpu-runs")
            .join(result.get("run_id").unwrap().as_str().unwrap());
        assert!(local.join("result.json").is_file());
        if matches!(
            end,
            End::Success | End::Failure | End::Timeout | End::Cancel
        ) {
            assert_eq!(
                std::fs::read(local.join("outputs/result")).unwrap(),
                b"ok\n"
            );
            assert_eq!(std::fs::read(local.join("log.txt")).unwrap(), b"hi\r\n");
        }
        if matches!(end, End::LostDuringDownload) {
            assert_eq!(
                std::fs::read(local.join("outputs/result")).unwrap(),
                b"ok\n"
            );
        }
        if matches!(end, End::Lost) {
            assert_eq!(
                std::fs::read(local.join("outputs/result")).unwrap(),
                b"ok\n"
            );
            assert_eq!(std::fs::read(local.join("log.txt")).unwrap(), b"");
        }
        if matches!(end, End::CancelTwice) {
            assert!(!local.join("outputs").exists());
            assert!(runner.calls().iter().all(|cmd| !cmd
                .argv
                .last()
                .unwrap()
                .starts_with("tar -C ")));
        }
        assert!(!pyjson::dumps(&Value::Dict(result), None).contains("private-canary"));
    }
}

#[test]
fn lost_create_reply_reconciles_only_its_run_tag() {
    for (reply, expected) in [
        (Err(RunError::TimedOut), 20),
        (Ok(Output::ok("broken reply")), 1),
        (
            Ok(Output::ok(
                r#"{"error":{"kind":"InvalidError","message":"Check Modal account/billing settings and the GPU/image request."}}"#,
            )),
            2,
        ),
        (
            Ok(Output::ok(
                r#"{"error":{"kind":"SchedulingTimeout","message":"capacity"}}"#,
            )),
            10,
        ),
        (
            Ok(Output::ok(
                r#"{"error":{"kind":"ResourceExhaustedError","message":"capacity"}}"#,
            )),
            10,
        ),
    ] {
        for cleanup_confirmed in [true, false] {
            let runner = FakeRunner::new({
                let reply = reply.clone();
                move |cmd| match cmd.argv[5].as_str() {
                    "create" => reply.clone(),
                    "terminate" => {
                        assert_eq!(cmd.argv[6], "--tag");
                        assert!(cmd.argv[7].starts_with("run_id="));
                        testing::ok(if cleanup_confirmed {
                            r#"{"finished":true}"#
                        } else {
                            r#"{"finished":false}"#
                        })
                    }
                    "volume-delete" => testing::ok(r#"{"finished":true}"#),
                    _ => panic!("{cmd:?}"),
                }
            });
            let t = TestEnv::new();
            let ctx = t.ctx().runner(&runner).build();
            let mut backend = ModalBackend::new(None);
            let deadline = ctx.clock.now() + jiff::Span::new().minutes(30);
            let request = pyjson::loads(r#"{"gpu":"T4","tags":{}}"#).unwrap();
            let error = backend
                .launch(&ctx, &request, deadline, 120.0, &mut |_, _| {
                    panic!("no ID received")
                })
                .unwrap_err();
            assert_eq!(
                error.exit_code(),
                if cleanup_confirmed { expected } else { 19 }
            );
            let calls = runner.calls();
            let Input::Text(spec) = &calls[0].stdin else {
                panic!()
            };
            let spec = pyjson::loads(spec).unwrap();
            let tag = spec
                .get("tags")
                .unwrap()
                .get("run_id")
                .unwrap()
                .as_str()
                .unwrap();
            assert_eq!(calls[1].argv[7], format!("run_id={tag}"));
            assert_eq!(calls[0].timeout, Some(150.0));
        }
    }
}

#[test]
fn termination_requires_combined_sandbox_and_volume_confirmation() {
    for (finished, expected) in [(true, 0), (false, 19)] {
        let runner = FakeRunner::new(move |cmd| match cmd.argv[5].as_str() {
            "terminate" => testing::ok(if finished {
                r#"{"finished":true}"#
            } else {
                r#"{"finished":false}"#
            }),
            "status" => testing::ok(if finished {
                r#"{"running":false,"returncode":0}"#
            } else {
                r#"{"running":true,"returncode":null}"#
            }),
            _ => panic!(),
        });
        let t = TestEnv::new();
        let ctx = t.ctx().runner(&runner).build();
        assert_eq!(
            terminate(&ctx, "modal:sb-test")
                .err()
                .map_or(0, |e| e.exit_code()),
            expected
        );
        assert_eq!(runner.calls()[0].argv[5], "terminate");
        assert_eq!(runner.calls().len(), if finished { 1 } else { 3 });
    }
}

#[test]
fn exec_failures_keep_typed_exit_codes_and_redact_details() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    ctx.protect_secret("private-canary");
    let backend = ModalBackend::new(None);
    for (failure, code) in [
        (Err(RunError::TimedOut), 20),
        (Err(RunError::NotFound), 12),
        (
            Ok(Output::failed(
                255,
                r#"{"error":{"kind":"AuthError","message":"private-canary"}}"#,
            )),
            13,
        ),
    ] {
        let error = backend
            .remote_result(&ctx, failure, "Upload failed.")
            .unwrap_err();
        assert_eq!(error.exit_code(), code);
        assert!(!error.text().contains("private-canary"));
    }
}

#[test]
fn cancelled_create_still_reconciles_before_returning_130() {
    let runner = FakeRunner::new(|cmd| match cmd.argv[5].as_str() {
        "create" => Err(RunError::TimedOut),
        "terminate" | "volume-delete" => testing::ok(r#"{"finished":true}"#),
        _ => panic!(),
    });
    let t = TestEnv::new();
    let ctx = t.ctx().runner(&runner).build();
    ctx.begin_run();
    ctx.interrupt();
    let deadline = ctx.clock.now() + jiff::Span::new().minutes(30);
    let request = pyjson::loads(r#"{"tags":{}}"#).unwrap();
    let error = ModalBackend::new(None)
        .launch(&ctx, &request, deadline, 120.0, &mut |_, _| panic!())
        .unwrap_err();
    assert_eq!(error.exit_code(), 130);
    assert_eq!(runner.calls().last().unwrap().argv[5], "volume-delete");
    ctx.end_run();
}

#[test]
fn modal_ids_are_qualified_and_validated_locally() {
    use crate::provider::{parse, Provider};
    assert_eq!(
        parse("modal:sb-abc_123").unwrap().provider(),
        Provider::Modal
    );
    for id in [
        "modal:",
        "modal:sb-",
        "modal:nope",
        "modal:sb-foo:bar",
        "modal:sb-foo\n",
        "modal:sb-$(id)",
    ] {
        assert!(parse(id).is_err(), "{id}");
    }
    assert_eq!(parse("123").unwrap().provider(), Provider::Vast);
}
