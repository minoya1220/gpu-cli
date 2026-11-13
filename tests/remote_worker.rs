//! Bash suites participate in the single cargo test gate. Linux workers need
//! /proc, flock and setsid; the virtual-clock keeper suite is portable.
fn remote_suite(script: &str) {
    let output = std::process::Command::new("bash")
        .arg(format!("{}/remote/{script}", env!("CARGO_MANIFEST_DIR")))
        .output()
        .expect("run bash suite");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{script}: {stdout}\n{stderr}");
    let passed = stdout
        .lines()
        .find_map(|line| line.strip_suffix(" passed, 0 failed"))
        .and_then(|number| number.parse::<usize>().ok())
        .unwrap_or(0);
    assert!(passed > 0, "{script}: no successful test summary: {stdout}");
}

#[test]
fn modal_keeper_suite() {
    remote_suite("test_modal_keeper.sh");
}

#[cfg(target_os = "linux")]
#[test]
fn remote_run_suite() {
    remote_suite("test_run_remote.sh");
}

#[cfg(target_os = "linux")]
#[test]
fn remote_worker_suite() {
    remote_suite("test_expiry_remote.sh");
}
