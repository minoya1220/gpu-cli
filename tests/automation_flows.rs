//! Real process/pipe semantics with forbidden-by-default cloud stubs.
mod common;
use common::Sandbox;
use gpu::pyjson::{self, Value};

fn ready() -> Sandbox {
    let s = Sandbox::new();
    s.stub("vastai", "#!/bin/sh\nprintf '%s\\n' '[{\"id\":123,\"actual_status\":\"running\",\"ssh_host\":\"ssh1.vast.ai\",\"ssh_port\":22}]'\n");
    s.stub(
        "ssh",
        r#"#!/bin/sh
case "$*" in *'-T '*) printf 'out\r\n\000'; printf 'err\r\n' >&2; exit 7;; *) exit 0;; esac
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
    assert!(output.stderr.ends_with(b"err\r\n"));
    assert!(!output.stderr.windows(4).any(|w| w == b"gpu:"));
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
}

#[test]
fn copy_both_directions_uses_batch_sftp_with_literal_paths_and_exit_codes() {
    let s = ready();
    s.stub(
        "scp",
        r#"#!/bin/sh
printf '%s\n' "$@" > "$HOME/scp-args"
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
        assert!(args.contains(&format!(
            "vast-123{}",
            if source.starts_with(':') {
                source
            } else {
                destination
            }
        )));
        assert!(String::from_utf8_lossy(&output.stdout).contains("\"exit_code\": 9"));
    }
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
