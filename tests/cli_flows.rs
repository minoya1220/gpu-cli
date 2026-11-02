//! Non-TTY end-to-end runs of the real binary under stub CLIs.

mod common;

use std::io::Read;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::Sandbox;

fn run(sandbox: &Sandbox, args: &[&str]) -> std::process::Output {
    sandbox.command(args).stdin(Stdio::null()).output().unwrap()
}

#[test]
fn test_ctrl_c_at_a_line_prompt_cancels_without_traceback() {
    let sandbox = Sandbox::new();
    let mut child = sandbox
        .command(&["up", "--dry-run"])
        .env("FAKE_CLI_DELAY", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let mut pipe = child.stdout.take().unwrap();
    let collected = stdout.clone();
    let reader = std::thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        while let Ok(n) = pipe.read(&mut buffer) {
            if n == 0 {
                break;
            }
            collected.lock().unwrap().extend_from_slice(&buffer[..n]);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !String::from_utf8_lossy(&stdout.lock().unwrap()).contains("Enter = No repository: ") {
        assert!(
            Instant::now() < deadline,
            "no prompt: {}",
            String::from_utf8_lossy(&stdout.lock().unwrap())
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::INT).unwrap();
    let status = child.wait().unwrap();
    reader.join().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let stdout = String::from_utf8_lossy(&stdout.lock().unwrap()).into_owned();
    assert_eq!(status.code(), Some(130));
    assert!(stdout.ends_with("\nCancelled.\n"), "{stdout:?}");
    assert!(!stderr.contains("panicked"), "{stderr}");
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
    assert!(String::from_utf8_lossy(&output.stdout).contains("Launch GPU workspaces on Vast.ai"));
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
fn paid_launch_names_the_cost_and_one_connection_command() {
    let sandbox = Sandbox::new();
    sandbox.stub("vastai", r#"#!/bin/sh
case "$1 $2" in
    'search offers')
        echo '[{"id": 100, "gpu_name": "RTX 4090", "num_gpus": 1, "dph_total": 0.3, "reliability": 0.99, "geolocation": "Test, XX", "verification": "verified"}]'
        ;;
    'create instance')
        echo '{"success": true, "new_contract": 123, "instance_api_key": "secret-instance-key"}'
        ;;
    *) exit 97 ;;
esac
"#);
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
        stdout
            .contains("Create paid container: 1 x RTX 4090, No repository, offer 100, $0.300/hr."),
        "{stdout}"
    );
    assert!(
        stdout.ends_with("Created instance 123.\nConnect: gpu ssh 123 --exec\n"),
        "{stdout}"
    );
    assert!(!stdout.contains("secret-instance-key"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-instance-key"));
}

#[test]
fn destruction_confirmation_names_disk_deletion_before_any_request() {
    use std::io::Write;

    let sandbox = Sandbox::new();
    let mut child = sandbox
        .command(&["down", "123"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"no\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "Delete instance 123 and its disk? Type 'yes' to destroy: Cancelled.\n"
    );
    assert!(sandbox.calls().is_empty());
}

#[test]
fn help_explains_paid_and_destructive_flags_without_dependency_calls() {
    let sandbox = Sandbox::new();
    let output = run(&sandbox, &["up", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("Skip paid-launch confirmation"));
    assert!(help.contains("Print the request without renting"));
    let output = run(&sandbox, &["expire", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("Scheduling authorizes deletion without another confirmation."));
    assert!(help.contains("gpu expire 12345678 show"));
    assert!(sandbox.calls().is_empty());
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
