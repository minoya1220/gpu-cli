//! Binary coverage: gcloud is always a logging PATH stub, never the real CLI.
mod common;
use common::Sandbox;
use std::process::Stdio;

fn configured() -> Sandbox {
    let s = Sandbox::new();
    std::fs::create_dir_all(s.root.join("config/gpu")).unwrap();
    std::fs::write(s.root.join("config/gpu/config.json"), r#"{"gcp":{"project":"test-project","zone":"us-east5-b","runtime":"v2-alpha-tpuv6e","expiry":{"queue":"gpu-expiry","location":"us-central1","service_account":"reaper@test-project.iam.gserviceaccount.com"}}}"#).unwrap();
    s
}

#[test]
fn gcp_dry_run_is_offline_and_outputs_only_json_requests_after_banner() {
    let s = configured();
    let output = s
        .command(&[
            "up",
            "--provider",
            "gcp",
            "--tpu",
            "v6e-1",
            "--no-repo",
            "--expire",
            "2h",
            "--spot",
            "--dry-run",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let (_, requests) = stdout.split_once('\n').unwrap();
    let parsed = gpu::pyjson::loads(requests).unwrap();
    assert_eq!(parsed.as_list().unwrap().len(), 2);
    assert!(requests.contains("--method=DELETE"));
    assert!(requests.contains("--spot"));
    assert!(s.calls().is_empty());
}

#[test]
fn config_lookup_and_setup_errors_use_xdg_or_dot_config_on_every_platform() {
    for xdg in [None, Some(""), Some("override")] {
        let s = Sandbox::new();
        let path = if xdg == Some("override") {
            s.root.join("config/gpu/config.json")
        } else {
            s.home.join(".config/gpu/config.json")
        };
        // A macOS Application Support file must not override the documented
        // XDG lookup; run the real binary to prove which config is consumed.
        let mac_path = s.home.join("Library/Application Support/gpu");
        std::fs::create_dir_all(&mac_path).unwrap();
        std::fs::write(mac_path.join("config.json"), "not the active config").unwrap();
        let launch = || {
            let mut cmd = s.command(&[
                "up",
                "--tpu",
                "v6e-1",
                "--no-repo",
                "--expire",
                "2h",
                "--dry-run",
            ]);
            match xdg {
                None => {
                    cmd.env_remove("XDG_CONFIG_HOME");
                }
                Some("") => {
                    cmd.env("XDG_CONFIG_HOME", "");
                }
                _ => {}
            }
            cmd.stdin(Stdio::null()).output().unwrap()
        };
        let missing = launch();
        assert_eq!(missing.status.code(), Some(1));
        let error = String::from_utf8_lossy(&missing.stderr);
        assert!(error.contains("Set up Google Cloud before renting TPUs"));
        assert!(
            error.contains(&format!("Config file: {}", path.display())),
            "{error}"
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"gcp":{"project":"test-project","zone":"us-east5-b","runtime":"v2-alpha-tpuv6e","expiry":{"queue":"gpu-expiry","location":"us-central1","service_account":"reaper@test-project.iam.gserviceaccount.com"}}}"#).unwrap();
        let configured = launch();
        assert!(
            configured.status.success(),
            "{}",
            String::from_utf8_lossy(&configured.stderr)
        );
        assert!(String::from_utf8_lossy(&configured.stdout).contains("--project=test-project"));
        assert!(s.calls().is_empty());
    }
}

#[test]
fn gcp_list_and_non_executing_ssh_never_use_vast() {
    let s = configured();
    s.stub("gcloud", r#"#!/bin/sh
{ printf 'gcloud'; for arg in "$@"; do printf '\t%s' "$arg"; done; printf '\n'; } >> "$FAKE_CLI_LOG"
case "$1 $2 $3 $4" in
  'compute tpus locations list') printf '%s\n' '[{"locationId":"us-east5-b"}]' ;;
  'compute tpus tpu-vm list') printf '%s\n' '[{"name":"projects/test-project/locations/us-east5-b/nodes/test-tpu","acceleratorType":"v6e-1","state":"READY"}]' ;;
  *) exit 97 ;;
esac
"#);
    let output = s
        .command(&["ls", "--provider", "gcp", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("gcp:test-project:us-east5-b:test-tpu")
    );
    let output = s
        .command(&["ssh", "gcp:test-project:us-east5-b:test-tpu"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let calls = s.calls();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|a| a[0] == "gcloud"
        && a.contains(&"--project=test-project".into())
        && a.contains(&"--quiet".into())));
}

#[test]
fn paused_queue_prevents_any_paid_resource_request() {
    let s = configured();
    s.stub("gcloud", r#"#!/bin/sh
{ printf 'gcloud'; for arg in "$@"; do printf '\t%s' "$arg"; done; printf '\n'; } >> "$FAKE_CLI_LOG"
case "$1 $2 $3" in
  'tasks queues describe') printf '%s\n' '{"state":"PAUSED","retryConfig":{"maxAttempts":-1,"maxBackoff":"60s"}}' ;;
  *) exit 97 ;;
esac
"#);
    let output = s
        .command(&[
            "up",
            "--provider",
            "gcp",
            "--tpu",
            "v6e-1",
            "--no-repo",
            "--expire",
            "2h",
            "--yes",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("must be RUNNING"));
    assert_eq!(s.calls().len(), 1);
    assert_eq!(s.calls()[0][1..4], ["tasks", "queues", "describe"]);
}
