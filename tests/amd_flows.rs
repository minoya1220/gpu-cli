//! Offline binary coverage: no provider executable can fall through to an account.
mod common;
use common::Sandbox;
use gpu::pyjson::{self, Value};

const CLI: &str = r#"#!/bin/sh
set -eu
tool=$(basename "$0")
{ printf '%s' "$tool"; for arg in "$@"; do printf '\t%s' "$arg"; done; printf '\n'; } >> "$FAKE_CLI_LOG"
case "$tool $1 ${2:-} ${3:-}" in
  'runpodctl gpu list --include-unavailable')
    echo '[{"gpuId":"AMD Instinct MI300X OAM","secureCloud":true,"communityCloud":false,"securePricePerHr":2.39,"communityPricePerHr":0.5,"dataCenterAvailability":[{"dataCenterId":"US-TEST","stockStatus":"Low"}]}]' ;;
  'doctl compute size list') echo '[{"slug":"gpu-mi300x1-192gb","price_hourly":2.59,"available":true,"regions":["atl1"]}]' ;;
  'doctl compute image get') echo '[{"id":777,"regions":["atl1"]}]' ;;
  'runpodctl pod list --all')
    if [ -f "$HOME/deleted-runpod" ]; then echo '[]'; else echo '[{"id":"pod-test","runtimeStatus":"running","instance_api_key":"synthetic-private-canary"}]'; fi ;;
  'doctl compute droplet list')
    if [ -f "$HOME/deleted-do" ]; then echo '[]'; else echo '[{"id":42,"region":{"slug":"atl1"},"status":"active","size_slug":"gpu-mi300x1-192gb"}]'; fi ;;
  'runpodctl pod delete pod-test') touch "$HOME/deleted-runpod"; echo '{"deleted":true}' ;;
  'doctl compute droplet delete') touch "$HOME/deleted-do"; echo 'Deleted droplet (non-JSON success is not proof)' ;;
  'runpodctl pod get pod-test') echo '{"id":"pod-test","runtimeStatus":"running","ssh":{"ip":"203.0.113.4","port":22022,"ssh_command":"do not execute this"}}' ;;
  *) echo 'synthetic-private-canary unexpected provider call' >&2; exit 97 ;;
esac
"#;

fn sandbox() -> Sandbox {
    let s = Sandbox::new();
    for binary in ["runpodctl", "doctl"] {
        s.stub(binary, CLI);
    }
    s.stub("git", common::FORBIDDEN);
    let config = s.root.join("config/gpu");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.json"), r#"{"owner":"fixture","runpod":{"image":"rocm/test@sha256:fixture","datacenter":"US-TEST"},"do":{"region":"atl1","ssh_keys":"123"}}"#).unwrap();
    s
}
fn json(s: &Sandbox, args: &[&str]) -> Value {
    let out = s.command(args).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(!text.contains("synthetic-private-canary"));
    pyjson::loads(&text).unwrap()
}

#[test]
fn explicit_amd_catalogs_and_workspace_dry_runs_never_reach_create_or_vast() {
    for provider in ["runpod", "do"] {
        let s = sandbox();
        let value = json(&s, &["offers", "--provider", provider, "--json"]);
        assert_eq!(value.get("command"), Some(&Value::str("offers")));
        let value = json(
            &s,
            &[
                "up",
                "--provider",
                provider,
                "--gpu",
                "MI300X",
                "--no-repo",
                "--dry-run",
                "--json",
            ],
        );
        let result = value.get("result").unwrap();
        assert_eq!(result.get("provider"), Some(&Value::str(provider)));
        assert_eq!(result.get("created"), Some(&Value::Bool(false)));
        assert_eq!(result.get("expiry_supported"), Some(&Value::Bool(false)));
        assert!(s
            .calls()
            .iter()
            .all(|c| ["runpodctl", "doctl"].contains(&c[0].as_str())
                && !c.iter().any(|arg| arg == "create")));
    }
}

#[test]
fn amd_inventory_and_down_use_qualified_ids_and_fresh_absence_checks() {
    for (provider, id) in [("runpod", "runpod:pod-test"), ("do", "do:atl1:42")] {
        let s = sandbox();
        let value = json(&s, &["ls", "--provider", provider, "--json", "--envelope"]);
        let instances = value
            .get("result")
            .unwrap()
            .get("instances")
            .unwrap()
            .as_list()
            .unwrap();
        assert_eq!(instances[0].get("id"), Some(&Value::str(id)));
        let value = json(&s, &["down", id, "--yes", "--json"]);
        assert_eq!(
            value.get("result").unwrap().get("destroyed"),
            Some(&Value::Bool(true))
        );
        let calls = s.calls();
        assert!(calls.last().unwrap().iter().any(|s| s == "list"));
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.iter().any(|s| s == "delete"))
                .count(),
            1
        );
    }
}

#[test]
fn live_run_and_expiry_fail_before_any_subprocess() {
    let s = sandbox();
    for provider in ["runpod", "do"] {
        let out = s
            .command(&[
                "run",
                "--provider",
                provider,
                "--gpu",
                "MI300X",
                "--expire",
                "30m",
                "--yes",
                "--json",
                "--",
                "true",
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        let value = pyjson::loads(&String::from_utf8(out.stdout).unwrap()).unwrap();
        assert_eq!(value.get("ok"), Some(&Value::Bool(false)));
    }
    for id in ["runpod:pod-test", "do:atl1:42"] {
        assert_eq!(
            s.command(&["expire", id, "30m", "--json"])
                .output()
                .unwrap()
                .status
                .code(),
            Some(2)
        );
    }
    assert!(s.calls().is_empty());
}

#[test]
fn copy_uses_real_shared_sftp_argv_but_only_stub_transports() {
    let s = sandbox();
    for binary in ["ssh", "scp"] {
        s.stub(binary, r#"#!/bin/sh
{ printf '%s' "$(basename "$0")"; for arg in "$@"; do printf '\t%s' "$arg"; done; printf '\n'; } >> "$FAKE_CLI_LOG"
"#);
    }
    json(
        &s,
        &[
            "cp",
            "runpod:pod-test",
            "./input with spaces",
            ":/workspace/input with spaces",
            "--json",
        ],
    );
    let calls = s.calls();
    let copy = calls.iter().find(|c| c[0] == "scp").unwrap();
    assert!(copy.iter().any(|s| s == "-s"));
    assert!(copy
        .iter()
        .any(|s| s == "runpod-pod-test:/workspace/input with spaces"));
    assert!(!calls.iter().flatten().any(|s| s.contains("do not execute")));
}
