use crate::modal::*;
use crate::process::{Input, Output, RunError};
use crate::pyjson::Value;
use crate::testing::{self, FakeRunner, TestEnv};

#[test]
fn exec_preserves_argv_streams_exit_and_deadline_without_ssh() {
    for exit in [0, 7, 255] {
        let runner = FakeRunner::new(move |cmd| {
            assert_eq!(cmd.program(), "uv");
            match cmd.argv[5].as_str() {
                "status" => testing::ok(r#"{"running":true}"#),
                "exec" => {
                    assert_eq!(
                        &cmd.argv[6..],
                        ["sb-test", "--", "printf", "%s", "a b;$(no)"]
                    );
                    assert_eq!(cmd.timeout, Some(9.0));
                    assert_eq!(cmd.stdin, Input::Null);
                    assert!(cmd.capture && !cmd.normalize_newlines);
                    Ok(Output {
                        code: exit,
                        stdout: "hi\r\n".into(),
                        stderr: "err\r\n".into(),
                    })
                }
                _ => panic!("{cmd:?}"),
            }
        });
        let t = TestEnv::new();
        let ctx = t.ctx().runner(&runner).build().for_command(true);
        ctx.set_command_timeout(Some(9.0));
        let result = exec(
            &ctx,
            "modal:sb-test",
            &["printf".into(), "%s".into(), "a b;$(no)".into()],
            4.0,
        );
        assert_eq!(result.err().map_or(0, |e| e.exit_code()), exit);
        let fields = ctx.result_snapshot();
        assert_eq!(fields.get("exit_code"), Some(&Value::Int(exit.into())));
        assert_eq!(fields.get("stdout"), Some(&Value::str("hi\r\n")));
        assert_eq!(fields.get("stderr"), Some(&Value::str("err\r\n")));
        assert_eq!(fields.get("ssh_alias"), Some(&Value::Null));
        assert_eq!(fields.get("ready"), Some(&Value::Bool(true)));
        assert_eq!(runner.calls()[0].timeout, Some(4.0));
    }
}

#[test]
fn typed_exec_failures_and_timeout_map_like_the_run_transport() {
    for (output, expected) in [
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
        let runner = FakeRunner::new(move |cmd| match cmd.argv[5].as_str() {
            "status" => testing::ok(r#"{"running":true}"#),
            "exec" => output.clone(),
            _ => panic!("{cmd:?}"),
        });
        let t = TestEnv::new();
        let ctx = t.ctx().runner(&runner).build().for_command(true);
        ctx.protect_secret("private-canary");
        let error = exec(&ctx, "modal:sb-test", &["true".into()], 4.0).unwrap_err();
        assert_eq!(error.exit_code(), expected);
        assert!(!error.text().contains("private-canary"));
        assert!(
            !crate::pyjson::dumps(&Value::Dict(ctx.result_snapshot()), None)
                .contains("private-canary")
        );
    }
}

#[test]
fn copy_and_ssh_share_the_exec_op_and_never_create_resources() {
    let runner = FakeRunner::new(|cmd| {
        assert_eq!(cmd.program(), "uv");
        match cmd.argv[5].as_str() {
            "status" => testing::ok(r#"{"running":true}"#),
            "exec" => {
                if cmd.argv[7] == "--pty" {
                    assert_eq!(cmd.stdin, Input::Inherit);
                    assert!(!cmd.capture);
                    assert_eq!(&cmd.argv[8..], ["--", "bash", "-l"]);
                } else {
                    assert_eq!(&cmd.argv[7..], ["--copy", "recursive", "a b", ":/work/a b"]);
                }
                testing::ok("")
            }
            _ => panic!("{cmd:?}"),
        }
    });
    let t = TestEnv::new();
    let ctx = t.ctx().runner(&runner).build();
    copy(&ctx, "modal:sb-test", "a b", ":/work/a b", true, 3.0).unwrap();
    ssh(&ctx, "modal:sb-test", false).unwrap();
    assert_eq!(
        ctx.result_snapshot().get("ssh_command"),
        Some(&Value::str("gpu ssh modal:sb-test --exec"))
    );
    ssh(&ctx, "modal:sb-test", true).unwrap();
    assert!(runner
        .calls()
        .iter()
        .all(|c| ["exec", "status"].contains(&c.argv[5].as_str())));
}

#[test]
fn orphan_volumes_are_listed_recoverable_and_cleanup_is_verified() {
    let runner = FakeRunner::new(|cmd| match cmd.argv[5].as_str() {
        "list" => testing::ok(r#"{"sandboxes":[],"volumes":[{"name":"gpu-cli-run-test"}]}"#),
        "volume-copy" => {
            assert_eq!(&cmd.argv[6..], ["gpu-cli-run-test", "./recovered"]);
            testing::ok("")
        }
        "volume-delete" => {
            assert_eq!(cmd.argv[6], "gpu-cli-run-test");
            testing::ok(r#"{"finished":true}"#)
        }
        _ => panic!("{cmd:?}"),
    });
    let t = TestEnv::new();
    let ctx = t.ctx().runner(&runner).build().for_command(true);
    let items = list(&ctx).unwrap();
    assert_eq!(
        items[0].get("id"),
        Some(&Value::str("modal:volume:gpu-cli-run-test"))
    );
    assert_eq!(
        items[0].get("actual_status"),
        Some(&Value::str("outputs-only"))
    );
    copy(
        &ctx,
        "modal:volume:gpu-cli-run-test",
        ":/",
        "./recovered",
        true,
        3.0,
    )
    .unwrap();
    down_volume(&ctx, "modal:volume:gpu-cli-run-test", true).unwrap();
    assert_eq!(
        ctx.result_snapshot().get("destroyed"),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        crate::provider::parse("modal:volume:gpu-cli-run-test")
            .unwrap()
            .provider(),
        crate::provider::Provider::Modal
    );
}

#[test]
fn down_cleans_the_recorded_volume_even_if_sandbox_metadata_disappears() {
    let runner = FakeRunner::new(|cmd| match cmd.argv[5].as_str() {
        "status" => testing::ok(r#"{"running":true,"volume":"gpu-cli-run-race"}"#),
        "terminate" => Ok(Output {
            code: 1,
            stdout: r#"{"error":{"kind":"NotFoundError","message":"gone"}}"#.into(),
            stderr: String::new(),
        }),
        "volume-delete" => {
            assert_eq!(cmd.argv[6], "gpu-cli-run-race");
            testing::ok(r#"{"finished":true}"#)
        }
        _ => panic!("{cmd:?}"),
    });
    let t = TestEnv::new();
    let ctx = t.ctx().runner(&runner).build().for_command(true);
    down(&ctx, "modal:sb-race", true).unwrap();
    assert_eq!(
        ctx.result_snapshot().get("destroyed"),
        Some(&Value::Bool(true))
    );
    assert_eq!(runner.calls().last().unwrap().argv[5], "volume-delete");
}

#[test]
fn down_keeps_the_volume_while_termination_is_unverified() {
    let runner = FakeRunner::new(|cmd| match cmd.argv[5].as_str() {
        "status" => testing::ok(r#"{"running":true,"volume":"gpu-cli-run-live"}"#),
        "terminate" => testing::ok(r#"{"finished":false}"#),
        _ => panic!("{cmd:?}"),
    });
    let t = TestEnv::new();
    let ctx = t.ctx().runner(&runner).build().for_command(true);
    let error = down(&ctx, "modal:sb-live", true).unwrap_err();
    assert_eq!(error.exit_code(), 19);
    assert_eq!(
        ctx.result_snapshot().get("destroyed"),
        Some(&Value::Bool(false))
    );
}

#[test]
fn output_paths_write_through_to_the_volume_and_keep_uploaded_results() {
    use std::fs;
    let t = TestEnv::new();
    let work = t.root.join("working tree");
    let mount = t.root.join("volume mount");
    fs::create_dir_all(work.join("results/checkpoints")).unwrap();
    fs::create_dir(&mount).unwrap();
    fs::write(work.join("results/checkpoints/seed"), b"initial").unwrap();
    let command = prepare_outputs(
        &work.to_string_lossy(),
        "results/checkpoints",
        &mount.to_string_lossy(),
        "gpu-cli-run-test",
    );
    assert!(std::process::Command::new("bash")
        .args(["-c", &command])
        .status()
        .unwrap()
        .success());
    assert_eq!(fs::read(mount.join("outputs/seed")).unwrap(), b"initial");
    assert_eq!(
        fs::read(mount.join(".gpu-cli-claimed")).unwrap(),
        b"gpu-cli-run-test\n"
    );
    let visible: Vec<_> = fs::read_dir(work.join("results/checkpoints"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(visible, ["seed"]);
    fs::write(work.join("results/checkpoints/checkpoint"), b"next").unwrap();
    assert_eq!(fs::read(mount.join("outputs/checkpoint")).unwrap(), b"next");
    let operand =
        crate::run::RunBackend::archive_operand(&ModalBackend::default(), "results/checkpoints");
    let archive = t.root.join("live.tar");
    assert!(std::process::Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("-C")
        .arg(&work)
        .arg("-cf")
        .arg(&archive)
        .args(["--", &operand])
        .status()
        .unwrap()
        .success());
    let local = t.root.join("download");
    fs::create_dir(&local).unwrap();
    let ctx = t.ctx().build();
    crate::run_files::unpack(
        &ctx,
        &archive,
        &local,
        std::path::Path::new("results/checkpoints"),
    )
    .unwrap();
    assert_eq!(
        fs::read(local.join("results/checkpoints/checkpoint")).unwrap(),
        b"next"
    );
    assert!(!std::process::Command::new("bash")
        .args(["-c", &command])
        .status()
        .unwrap()
        .success());
    assert_eq!(fs::read(mount.join("outputs/checkpoint")).unwrap(), b"next");
}

#[test]
fn output_flush_requests_use_syncfs_with_a_portable_fallback() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    let t = TestEnv::new();
    let bin = t.root.join("sync-bin");
    fs::create_dir(&bin).unwrap();
    let sync = bin.join("sync");
    fs::write(
        &sync,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >>\"$LOG\"\n[ \"$1\" != -f ]\n",
    )
    .unwrap();
    fs::set_permissions(&sync, fs::Permissions::from_mode(0o700)).unwrap();
    let log = t.root.join("sync-calls");
    let output = std::process::Command::new("/bin/bash")
        .args(["-c", OUTPUTS_HOOK])
        .env("PATH", &bin)
        .env("LOG", &log)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "-f /gpu-volume\n/gpu-volume\n"
    );
    fs::remove_file(sync).unwrap();
    let output = std::process::Command::new("/bin/bash")
        .args(["-c", OUTPUTS_HOOK])
        .env("PATH", &bin)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("sync unavailable"));
}

#[test]
fn invalid_modal_ids_and_copy_paths_fail_without_dependency_calls() {
    let runner = FakeRunner::new(|cmd| panic!("{cmd:?}"));
    let t = TestEnv::new();
    let ctx = t.ctx().runner(&runner).build();
    for id in ["modal:sb-", "modal:sb-test;evil", "modal:volume:unrelated"] {
        assert_eq!(
            exec(&ctx, id, &["true".into()], 1.0)
                .unwrap_err()
                .exit_code(),
            2
        );
        assert!(crate::provider::parse(id).is_err());
    }
    assert_eq!(
        copy(&ctx, "modal:sb-test", "./a", "./b", true, 1.0)
            .unwrap_err()
            .exit_code(),
        2
    );
    assert!(runner.calls().is_empty());
}
