mod common;
use common::Sandbox;
use gpu::pyjson::{self, Value};

const UV: &str = r#"#!/bin/sh
printf 'uv %s\n' "$*" >> "$FAKE_CLI_LOG"
[ "$1 $2 $3" = 'run --quiet --script' ] || exit 97
[ -f "$4" ] || exit 97
case "$5" in
rates) printf '%s\n' '{"gpu_hour_cost_t4":"0.59","cpu_hour_cost_sandbox":"0.1419","mem_gib_hour_cost_sandbox":"0.024"}' ;;
check) printf '%s\n' '{"client_version":"1.6.3","workspace":"offline-test","environment":"main"}' ;;
*) exit 97 ;;
esac
"#;

#[test]
fn modal_dry_run_and_setup_use_only_stubbed_uv() {
    let s = Sandbox::new();
    s.stub("uv", UV);
    let cwd = s.root.join("source");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("job.py"), "print('hi')\n").unwrap();
    let out = s
        .command(&[])
        .current_dir(&cwd)
        .env("TOKEN", "super-private-canary")
        .args([
            "run",
            "--provider",
            "modal",
            "--gpu",
            "t4",
            "--expire",
            "30m",
            "--dry-run",
            "--env",
            "TOKEN",
            "--json",
            "--",
            "python",
            "job.py",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("super-private-canary"));
    let value = pyjson::loads(&text).unwrap();
    let result = value.get("result").unwrap();
    assert_eq!(result.get("provider"), Some(&Value::str("modal")));
    assert_eq!(result.get("created"), Some(&Value::Bool(false)));
    let request = result.get("request").unwrap();
    assert_eq!(
        request.get("env_names"),
        Some(&Value::List(vec![Value::str("TOKEN")]))
    );
    assert_eq!(request.get("memory"), Some(&Value::Int(16384)));
    let Some(Value::Float(price)) = result.get("price_per_hour") else {
        panic!("missing price");
    };
    assert!((price - 1.5416).abs() < 1e-9);
    let calls = format!("{:?}", s.calls());
    assert!(calls.contains("uv"));
    assert!(!calls.contains("vastai") && !calls.contains("gcloud"));
    assert!(!calls.contains("super-private-canary"));
    assert!(!cwd.join("gpu-runs").exists());
    let out = s
        .command(&[])
        .args(["setup", "--provider", "modal", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let value = pyjson::loads(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let result = value.get("result").unwrap();
    assert_eq!(result.get("workspace"), Some(&Value::str("offline-test")));
    assert_eq!(result.get("ready"), Some(&Value::Bool(true)));
}

#[test]
fn modal_live_is_disabled_and_incompatible_flags_fail_before_uv() {
    for options in [
        vec!["--disk", "100", "--dry-run"],
        vec!["--offer", "2", "--dry-run"],
        vec!["--reliability", "0.9", "--dry-run"],
        vec!["--unverified", "--dry-run"],
        vec!["--yes"],
    ] {
        let s = Sandbox::new();
        s.stub("uv", common::FORBIDDEN);
        let out = s
            .command(&[])
            .args([
                "run",
                "--provider",
                "modal",
                "--gpu",
                "T4",
                "--expire",
                "30m",
                "--json",
            ])
            .args(options)
            .args(["--", "true"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert!(s.calls().is_empty());
    }
}

#[test]
fn modal_price_and_spend_caps_fail_without_creation() {
    for (cap, expected) in [(None, 11), (Some(0.01), 15)] {
        let s = Sandbox::new();
        s.stub("uv", UV);
        let cwd = s.root.join("source");
        std::fs::create_dir(&cwd).unwrap();
        if let Some(cap) = cap {
            let config = s.root.join("config/gpu");
            std::fs::create_dir(&config).unwrap();
            std::fs::write(
                config.join("config.json"),
                format!("{{\"max_session_spend\":{cap}}}"),
            )
            .unwrap();
        }
        let mut command = s.command(&[
            "run",
            "--provider",
            "modal",
            "--gpu",
            "T4",
            "--expire",
            "30m",
            "--dry-run",
            "--json",
        ]);
        command.current_dir(&cwd);
        if cap.is_none() {
            command.args(["--max-price", "1"]);
        }
        let out = command.args(["--", "true"]).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(expected),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(!format!("{:?}", s.calls()).contains("create"));
        assert!(!cwd.join("gpu-runs").exists());
    }
}

#[test]
fn python_helper_contract_against_stub_module() {
    let status = std::process::Command::new("python3")
        .args([
            "-B",
            &format!("{}/tests/modal_helper_test.py", env!("CARGO_MANIFEST_DIR")),
        ])
        .env(
            "HELPER_PATH",
            format!("{}/remote/modal_helper.py", env!("CARGO_MANIFEST_DIR")),
        )
        .status()
        .expect("python3 is required for the Modal helper contract tests");
    assert!(status.success());
}
