//! Non-TTY end-to-end runs of the real binary under stub CLIs.

mod common;

use std::io::Read;
use std::process::Stdio;

use common::Sandbox;

fn run(sandbox: &Sandbox, args: &[&str]) -> std::process::Output {
    sandbox.command(args).stdin(Stdio::null()).output().unwrap()
}

#[test]
fn nonterminal_missing_flags_refuse_even_with_an_open_stdin_pipe() {
    let sandbox = Sandbox::new();
    let output = sandbox
        .command(&["up", "--dry-run"])
        .stdin(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--expire"));
    assert!(output.stdout.is_empty());
    assert!(sandbox.calls().is_empty());
}

#[test]
fn version_usage_and_help() {
    let sandbox = Sandbox::new();
    for flag in ["-v", "-V", "--version"] {
        let output = run(&sandbox, &[flag]);
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "0.1.0\n");
    }
    let output = run(&sandbox, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Usage: gpu"));
    let output = run(&sandbox, &["--help"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("GPU jobs and workspaces on Vast.ai, Google Cloud and Modal"));
    for bad in [
        &["up", "--num-gpus", "1.5"][..],
        &["up", "--mode", "metal"],
        &["edit", "--editor", "vim"],
        &["bogus"],
    ] {
        assert_eq!(run(&sandbox, bad).status.code(), Some(2), "{bad:?}");
    }
    assert!(sandbox.calls().is_empty());
}

#[test]
fn dry_run_with_flags_prints_the_exact_request() {
    let sandbox = Sandbox::new();
    // Long-option prefixes work as in argparse (`--dry` for `--dry-run`).
    let output = run(
        &sandbox,
        &[
            "up",
            "--no-repo",
            "--mode",
            "vm",
            "--gpu",
            "Tesla T4",
            "--yes",
            "--dry",
            "--expire",
            "2h",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("DRY RUN: no instance created\n[\n  \"vastai\",\n  \"create\",\n  \"instance\",\n  \"100\","));
    assert!(stdout.contains("\"docker.io/vastai/kvm\""));
    assert!(stdout.contains("\"no-repo\""));
    let calls = sandbox.calls();
    assert_eq!(calls[0][..3], ["gh", "api", "user"]);
    let calls = &calls[1..];
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0][..3], ["vastai", "search", "offers"]);
    assert!(calls[0][3].ends_with("disk_space>=130 vms_enabled=true gpu_name=Tesla_T4"));
    assert!(!sandbox.history_path().exists());
}

#[test]
fn handled_errors_are_redacted_and_exit_1() {
    let sandbox = Sandbox::new();
    sandbox.stub(
        "vastai",
        "#!/bin/sh\necho 'failure token=abc123secret' >&2\nexit 3\n",
    );
    let output = run(&sandbox, &["ls"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "gpu: failure token=REDACTED\n"
    );
}

#[test]
fn launch_names_one_connection_command() {
    let sandbox = Sandbox::new();
    sandbox.stub("vastai", r#"#!/bin/sh
case "$1 $2" in
    'search offers')
        echo '[{"id": 100, "gpu_name": "RTX 4090", "num_gpus": 1, "dph_total": 0.3, "reliability": 0.99, "geolocation": "Test, XX", "verification": "verified"}]'
        ;;
    'create instance')
        echo '{"success": true, "new_contract": 123, "instance_api_key": "secret-instance-key"}'
        ;;
    'show instances')
        echo '[{"id":123,"actual_status":"running","ssh_host":"ssh1.vast.ai","ssh_port":22}]'
        ;;
    *) exit 97 ;;
esac
"#);
    sandbox.stub("ssh", "#!/bin/sh\ncase \"$*\" in *ConnectTimeout=10*) echo '{\"status\":\"armed\",\"deadline\":4102444800}';; *) exit 0;; esac\n");
    let output = run(
        &sandbox,
        &[
            "up",
            "--no-repo",
            "--mode",
            "container",
            "--gpu",
            "RTX 4090",
            "--yes",
            "--expire",
            "2h",
        ],
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Created instance 123.\nConnect: gpu ssh 123 --exec\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("secret-instance-key"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-instance-key"));
}

#[test]
fn broken_pipe_on_stdout_is_a_handled_error() {
    let sandbox = Sandbox::new();
    sandbox.stub("vastai", "#!/bin/sh\nsleep 0.2\necho '[{\"id\": 1}]'\n");
    let mut child = sandbox
        .command(&["ls", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let status = child.wait().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert_eq!(status.code(), Some(1));
    assert!(
        stderr.starts_with("gpu: [Errno 32] Broken pipe"),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked"));
}

#[test]
fn empty_xdg_variables_count_as_unset() {
    let sandbox = Sandbox::new();
    let output = sandbox
        .command(&[
            "up",
            "--no-repo",
            "--mode",
            "container",
            "--gpu",
            "Tesla T4",
            "--yes",
            "--dry-run",
            "--expire",
            "2h",
        ])
        .env("XDG_CACHE_HOME", "")
        .env("XDG_STATE_HOME", "")
        .env("XDG_CONFIG_HOME", "")
        .current_dir(&sandbox.root)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert!(sandbox.home.join(".cache/gpu/gpu_models.txt").exists());
    assert!(!sandbox.root.join("gpu").exists());
}
