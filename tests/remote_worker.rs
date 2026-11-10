//! Runs the remote expiry worker's own test suite, so `cargo test` stays the
//! single gate. The worker needs Linux (`/proc`, `flock`, `setsid`).

#[cfg(target_os = "linux")]
#[test]
fn remote_run_suite() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/remote/test_run_remote.sh");
    let output = std::process::Command::new("bash")
        .arg(script)
        .output()
        .expect("run bash");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|line| line == "11 passed, 0 failed"),
        "{stdout}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn remote_worker_suite() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/remote/test_expiry_remote.sh");
    let output = std::process::Command::new("bash")
        .arg(script)
        .output()
        .expect("run bash");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    let passed: usize = stdout
        .lines()
        .find_map(|line| line.strip_suffix(" passed, 0 failed"))
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    assert!(passed > 0, "{stdout}");
}
