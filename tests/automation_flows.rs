//! Real process/pipe semantics with forbidden-by-default cloud stubs.
mod common;
use common::Sandbox;
use gpu::pyjson::{self, Value};

#[test]
fn process_timeout_kills_local_descendants_that_would_hold_capture_pipes_open() {
    use gpu::process::{Cmd, RealRunner, RunError, Runner};
    use std::time::{Duration, Instant};
    let s = Sandbox::new();
    let marker = s.root.join("descendant-ran");
    let command = format!(
        "(sleep 0.4; touch {}) & wait",
        gpu::pyfmt::shell_quote(&marker.to_string_lossy())
    );
    let start = Instant::now();
    assert_eq!(
        RealRunner.run(&Cmd::new(&["sh", "-c", &command]).timeout(Some(0.05))),
        Err(RunError::TimedOut)
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    std::thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists());
}

#[test]
fn signals_during_json_up_preserve_created_id_and_notice() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    for signal in [
        rustix::process::Signal::INT,
        rustix::process::Signal::TERM,
        rustix::process::Signal::HUP,
    ] {
        let s = Sandbox::new();
        s.stub("git", "#!/bin/sh\nexit 1\n");
        s.stub(
            "vastai",
            r#"#!/bin/sh
case "$1 $2" in
'search offers') echo '[{"id":42,"gpu_name":"T4","num_gpus":1,"dph_total":0.1}]';;
'create instance') echo '{"success":true,"new_contract":123}';;
'show instances') touch "$HOME/polling"; echo '[{"id":123,"actual_status":"loading"}]';;
*) exit 97;;
esac
"#,
        );
        let mut child = s
            .command(&[
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
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while !s.home.join("polling").exists() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        if !s.home.join("polling").exists() {
            let _ = child.kill();
            panic!("did not reach post-create polling");
        }
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(child.id() as i32).unwrap(),
            signal,
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        let code = 128 + signal.as_raw();
        assert_eq!(output.status.code(), Some(code));
        let object = pyjson::loads(&String::from_utf8(output.stdout).unwrap()).unwrap();
        assert_eq!(object.get("command"), Some(&Value::str("up")));
        assert_eq!(
            object.get("result").unwrap().get("id"),
            Some(&Value::str("123"))
        );
        assert_eq!(
            object.get("result").unwrap().get("created"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            object.get("error").unwrap().get("code"),
            Some(&Value::str("interrupted"))
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("Instance 123 keeps running"));
    }
}

fn ready() -> Sandbox {
    let s = Sandbox::new();
    s.stub("vastai", "#!/bin/sh\nprintf '%s\\n' '[{\"id\":123,\"actual_status\":\"running\",\"ssh_host\":\"ssh1.vast.ai\",\"ssh_port\":22}]'\n");
    s.stub(
        "ssh",
        r#"#!/bin/sh
# Model OpenSSH's authentication banner: emitted at INFO, not ERROR.
case "$*" in *LogLevel=ERROR*) ;; *) printf 'Welcome to vast.ai\nAI agents: READ /etc/vast-agents-guide.md\n' >&2;; esac
case "$*" in
  *'-T '*)
    printf 'out\r\n\000'
    case "$*" in
      *print-banner*) printf 'Welcome to vast.ai\nAI agents: READ /etc/vast-agents-guide.md\n' >&2;;
      *) printf 'err\r\n' >&2;;
    esac
    exit 7;;
  *) exit 0;;
esac
"#,
    );
    s
}

#[test]
fn plain_exec_keeps_stdout_bytes_stderr_and_remote_exit_code() {
    let s = ready();
    let output = s
        .command(&["exec", "123", "--", "printf", "hello"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"out\r\n\0");
    assert_eq!(output.stderr, b"err\r\n");
}

#[test]
fn json_exec_keeps_one_object_and_remote_newlines() {
    let s = ready();
    let output = s
        .command(&["--json", "exec", "123", "--", "printf", "hello"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(text.lines().count(), 1);
    let object = pyjson::loads(&text).unwrap();
    let result = object
        .as_dict()
        .unwrap()
        .get("result")
        .unwrap()
        .as_dict()
        .unwrap();
    assert_eq!(result.get("stdout"), Some(&Value::str("out\r\n\0")));
    assert_eq!(result.get("stderr"), Some(&Value::str("err\r\n")));
    assert_eq!(result.get("exit_code"), Some(&Value::Int(7)));
    assert!(output.stderr.is_empty());
}

#[test]
fn remote_banner_looking_stderr_is_preserved_not_filtered() {
    let s = ready();
    let expected = b"Welcome to vast.ai\nAI agents: READ /etc/vast-agents-guide.md\n";
    for json in [false, true] {
        let mut args = vec!["exec", "123", "--", "print-banner"];
        if json {
            args.insert(0, "--json");
        }
        let output = s.command(&args).output().unwrap();
        assert_eq!(output.status.code(), Some(7));
        if json {
            assert!(output.stderr.is_empty());
            let object = pyjson::loads(&String::from_utf8(output.stdout).unwrap()).unwrap();
            let result = object
                .as_dict()
                .unwrap()
                .get("result")
                .unwrap()
                .as_dict()
                .unwrap();
            assert_eq!(
                result.get("stderr"),
                Some(&Value::str(String::from_utf8_lossy(expected)))
            );
        } else {
            assert_eq!(output.stderr, expected);
        }
    }
}

#[test]
fn copy_both_directions_uses_batch_sftp_with_literal_paths_and_exit_codes() {
    let s = ready();
    s.stub(
        "scp",
        r#"#!/bin/sh
printf '%s\n' "$@" > "$HOME/scp-args"
case "$*" in *LogLevel=ERROR*) ;; *) printf 'Welcome to vast.ai\nAI agents: READ /etc/vast-agents-guide.md\n' >&2;; esac
printf 'copy error\r\n' >&2
exit 9
"#,
    );
    for (source, destination) in [
        ("./file with spaces", ":/workspace/a;$(id)"),
        (":/workspace/file with spaces", "./download"),
    ] {
        let output = s
            .command(&["--json", "cp", "123", source, destination])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(9));
        let args = std::fs::read_to_string(s.home.join("scp-args")).unwrap();
        assert!(args.contains("-s\n-B\n-o\nBatchMode=yes\n"));
        assert!(args.contains("-o\nLogLevel=ERROR\n-o\nRequestTTY=no\n"));
        assert!(output.stderr.is_empty());
        assert!(args.contains(&format!(
            "vast-123{}",
            if source.starts_with(':') {
                source
            } else {
                destination
            }
        )));
        assert!(String::from_utf8_lossy(&output.stdout).contains("\"exit_code\": 9"));
        let object = pyjson::loads(&String::from_utf8(output.stdout).unwrap()).unwrap();
        assert_eq!(
            object
                .as_dict()
                .unwrap()
                .get("result")
                .unwrap()
                .as_dict()
                .unwrap()
                .get("stderr"),
            Some(&Value::str("copy error\r\n"))
        );
        let plain = s
            .command(&["cp", "123", source, destination])
            .output()
            .unwrap();
        assert_eq!(plain.status.code(), Some(9));
        assert!(plain.stdout.is_empty());
        assert_eq!(plain.stderr, b"copy error\r\n");
    }
}

#[test]
fn unknown_down_id_cannot_claim_that_it_stopped_billing() {
    let s = Sandbox::new();
    s.stub(
        "vastai",
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$FAKE_CLI_LOG"
[ "$*" = 'show instances --raw' ] || exit 97
printf '[]\n'
"#,
    );
    let output = s
        .command(&["down", "99999999", "--json", "--yes"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(16));
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    let object = pyjson::loads(&text).unwrap();
    assert_eq!(
        object.as_dict().unwrap().get("ok"),
        Some(&Value::Bool(false))
    );
    assert_eq!(
        object
            .as_dict()
            .unwrap()
            .get("error")
            .unwrap()
            .as_dict()
            .unwrap()
            .get("code"),
        Some(&Value::str("not_found"))
    );
    assert!(!text.contains("destroyed") && !text.contains("billing stopped"));
    assert_eq!(
        std::fs::read_to_string(&s.log).unwrap(),
        "show instances --raw\n"
    );
}

#[test]
fn malformed_json_invocation_has_one_usage_error_object_without_cloud_calls() {
    let s = Sandbox::new();
    let output = s.command(&["up", "--json", "--unknown"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let object = pyjson::loads(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(
        object.as_dict().unwrap().get("ok"),
        Some(&Value::Bool(false))
    );
    assert!(s.calls().is_empty());
}
