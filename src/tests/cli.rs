//! Port of tests/test_cli.py.

use crate::config::{defaults, CONTAINER_ENV};
use crate::launch::{launch, UpArgs};
use crate::offers::{
    gpu_cache_path, gpu_category, known_gpu_models, read_gpu_cache, update_gpu_cache,
};
use crate::process::{run, Output};
use crate::prompt::{choose, choose_gpu, Choices};
use crate::pyjson::Value;
use crate::repos::Repo;
use crate::startup::startup_script;
use crate::testing::*;
use crate::vast::{argv, request, RequestArgs, Vast};

pub const EXPECTED_CONTAINER_SCRIPT: &str = r#"#!/bin/bash
source /etc/environment 2>/dev/null || true
export GH_TOKEN
apt update && apt install -y gh
touch /root/.no_auto_tmux
rm -rf /venv
mkdir -p /workspace
git config --global user.name minoya1220
git config --global user.email minoya1220@gmail.com
if [ -n "${GH_TOKEN:-}" ]; then gh auth setup-git; else echo "WARNING: GH_TOKEN is not set in Vast account environment; private GitHub repos may not clone." >&2; fi
[ -d /workspace/gpu-kernels ] || git clone https://github.com/minoya1220/gpu-kernels /workspace/gpu-kernels
if [ -f /workspace/gpu-kernels/pyproject.toml ]; then uv sync --directory /workspace/gpu-kernels; fi
sed -i 's|^cd \${WORKSPACE}$|[ "$PWD" = "$HOME" ] \&\& cd "${WORKSPACE:-/workspace}"|' /root/.bashrc
touch /root/.works
entrypoint.sh
"#;

fn repo(name: &str) -> Repo {
    Repo::from_arg(name)
}

/// The Python tests' `args()` defaults plus what `_launch` reads.
pub fn up_args() -> UpArgs {
    UpArgs {
        repo: Some(repo("gpu-kernels")),
        mode: Some("container".into()),
        num_gpus: 1,
        ..UpArgs::default()
    }
}

pub fn bash_n(script: &str) -> std::process::Output {
    use std::io::Write;
    let mut child = std::process::Command::new("bash")
        .arg("-n")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn search_answer(offers: &'static str) -> std::sync::Arc<FakeRunner> {
    FakeRunner::new(move |cmd| match strs(&cmd.argv)[..] {
        ["vastai", "search", "offers", ..] => ok(offers),
        _ => panic!("unexpected command: {:?}", cmd.argv),
    })
}

#[test]
fn test_container_script_uses_selected_repo_directory_only() {
    let script = startup_script(&repo("gpu-kernels"), "container", &defaults()).unwrap();
    assert_eq!(script, EXPECTED_CONTAINER_SCRIPT);
    assert!(script
        .contains("git clone https://github.com/minoya1220/gpu-kernels /workspace/gpu-kernels"));
    assert!(script.contains("uv sync --directory /workspace/gpu-kernels"));
    assert!(!script.contains("/workspace/cute-kernels"));
    assert!(script.contains("gh auth setup-git"));
    assert!(script.find("gh auth setup-git") < script.find("git clone"));
    assert!(script.contains(".no_auto_tmux"));
    assert_eq!(script.lines().last(), Some("entrypoint.sh"));
    assert!(!script.contains("set -e"));
    assert!(!script.contains("72299") && !script.contains("741641"));
    assert!(!CONTAINER_ENV.contains("72299") && !CONTAINER_ENV.contains("741641"));
}

#[test]
fn test_generated_startup_script_is_valid_bash() {
    for mode in ["container", "vm"] {
        let result = bash_n(&startup_script(&repo("gpu-kernels"), mode, &defaults()).unwrap());
        assert!(
            result.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn test_vm_script_preserves_toolchain_and_does_not_inject_secret() {
    let script = startup_script(&repo("gpu-kernels"), "vm", &defaults()).unwrap();
    assert!(script.contains("nsight-compute-2025.4.0") && script.contains("cuda-nvcc-12-8"));
    assert!(script.contains("source /etc/environment"));
    assert!(script.contains("GH_TOKEN") && script.contains("WARNING"));
    assert!(script.contains("gh auth setup-git"));
    assert!(script.contains("uv sync --directory /workspace/gpu-kernels"));
    assert!(!script.contains("vast_api_key"));
    assert!(!script.contains("/root/.no_auto_tmux"));
    assert!(!script.contains("set -e"));
    assert_eq!(script.lines().last(), Some("touch /root/.works"));
}

#[test]
fn test_launch_request_exact_image_env_script_and_offer() {
    let offer = dict(r#"{"id": 45669333, "dph_total": 0.46898148148148144}"#);
    let gpu_kernels = repo("gpu-kernels");
    let args = RequestArgs {
        repo: &gpu_kernels,
        mode: "container",
        interruptible: false,
        disk: None,
    };
    let actual = argv(&request(&offer, &args, &defaults()).unwrap()).unwrap();
    assert_eq!(
        strs(&actual),
        [
            "vastai",
            "create",
            "instance",
            "45669333",
            "--image",
            "vastai/base-image:@vastai-automatic-tag",
            "--disk",
            "20",
            "--onstart-cmd",
            EXPECTED_CONTAINER_SCRIPT,
            "--ssh",
            "--direct",
            "--label",
            "gpu-kernels",
            "--env",
            r#"-p 1111:1111 -p 6006:6006 -p 8080:8080 -p 8384:8384 -e OPEN_BUTTON_PORT=1111 -e OPEN_BUTTON_TOKEN=1 -e JUPYTER_DIR=/ -e DATA_DIRECTORY=/workspace/ -e PORTAL_CONFIG="localhost:1111:11111:/:Instance Portal|localhost:8080:18080:/:Jupyter|localhost:8080:8080:/terminals/1:Jupyter Terminal|localhost:8384:18384:/:Syncthing|localhost:6006:16006:/:Tensorboard""#,
            "--raw",
        ]
    );
    let vm_offer = dict(r#"{"id": 123, "min_bid": 0.2}"#);
    let vm_args = RequestArgs {
        repo: &gpu_kernels,
        mode: "vm",
        interruptible: true,
        disk: Some(130),
    };
    let request = argv(&request(&vm_offer, &vm_args, &defaults()).unwrap()).unwrap();
    assert_eq!(
        strs(&request[..8]),
        [
            "vastai",
            "create",
            "instance",
            "123",
            "--image",
            "docker.io/vastai/kvm",
            "--disk",
            "130"
        ]
    );
    assert!(!request.contains(&"--env".to_string()));
    assert_eq!(
        strs(&request[request.len() - 3..]),
        ["--bid_price", "0.2", "--raw"]
    );
    let script = &request[request.iter().position(|a| a == "--onstart-cmd").unwrap() + 1];
    assert!(script.contains("source /etc/environment"));
}

#[test]
fn test_adapter_search_generates_exact_read_only_cli_call() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|_| ok("[]"));
    let ctx = t.ctx().runner(&runner).build();
    assert_eq!(
        Vast::new(&ctx).offers("rentable=true", true).unwrap(),
        Value::List(vec![])
    );
    assert_eq!(
        runner.argvs(),
        [[
            "vastai",
            "search",
            "offers",
            "rentable=true",
            "--no-default",
            "--limit",
            "1000",
            "--order",
            "min_bid",
            "--interruptible",
            "--raw"
        ]]
    );
}

// Expected values from CPython 3.13: shlex.quote returns '' for falsy values
// and raises TypeError for other non-strings.
#[test]
fn test_non_string_git_identity_is_quoted_like_shlex() {
    let gpu_kernels = repo("gpu-kernels");
    let cases = [
        ("null", Ok("''")),
        ("0", Ok("''")),
        ("false", Ok("''")),
        ("[]", Ok("''")),
        ("5", Err("must be a string, got a number.")),
        ("true", Err("must be a string, got a boolean.")),
    ];
    for key in ["git_name", "git_email"] {
        let line = if key == "git_name" {
            "user.name"
        } else {
            "user.email"
        };
        for (value, expected) in &cases {
            let mut cfg = defaults();
            cfg.insert(key, json(value));
            let result = startup_script(&gpu_kernels, "container", &cfg);
            match expected {
                Ok(quoted) => {
                    let script = result.unwrap();
                    assert!(
                        script.contains(&format!("git config --global {line} {quoted}\n")),
                        "{key}={value}"
                    );
                }
                Err(text) => assert_eq!(
                    result.unwrap_err().text(),
                    format!("Config '{key}' {text}"),
                    "{key}={value}"
                ),
            }
        }
    }
}

// Python puts the raw image value into the argument list: json.dumps prints
// it in a dry run, and subprocess.run raises TypeError before starting vastai.
#[test]
fn test_non_string_image_is_printed_raw_and_never_reaches_vastai() {
    let mut cfg = defaults();
    cfg.insert("container_image", Value::Null);
    let args = UpArgs {
        gpu: Some("RTX 4090".into()),
        offer: Some(42),
        yes: true,
        ..up_args()
    };

    let t = TestEnv::new();
    let ctx = t.ctx().runner(&search_answer(RTX_OFFER)).build();
    let dry = UpArgs {
        dry_run: true,
        ..args.clone()
    };
    launch(&ctx, &dry, &cfg, None).unwrap();
    assert!(out(&ctx).contains("  \"--image\",\n  null,\n  \"--disk\","));

    let t = TestEnv::new();
    let runner = search_answer(RTX_OFFER);
    let ctx = t.ctx().runner(&runner).build();
    let err = launch(&ctx, &args, &cfg, None).unwrap_err();
    assert_eq!(
        err.text(),
        "Request argument must be text, got null. Check image and repository config."
    );
    assert!(runner.argvs().iter().all(|argv| argv[1] != "create"));
}

#[test]
fn test_up_validates_before_network_calls() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    let args = UpArgs {
        num_gpus: 0,
        ..up_args()
    };
    let err = launch(&ctx, &args, &defaults(), None).unwrap_err();
    assert!(err.text().contains("--num-gpus"));
}

const RTX_OFFER: &str = r#"[{"id": 42, "gpu_name": "RTX 4090", "num_gpus": 1, "dph_total": 0.4, "reliability": 0.99, "geolocation": "Test, XX", "verification": "verified"}]"#;

/// The JSON request printed after `DRY RUN: no instance created`.
pub fn dry_run_request(output: &str) -> Vec<String> {
    let text = output
        .split("DRY RUN: no instance created\n")
        .nth(1)
        .expect("dry run output");
    let end = text.find("\n]").map(|i| i + 2).unwrap_or(text.len());
    match json(&text[..end]) {
        Value::List(items) => items
            .into_iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect(),
        other => panic!("not a list: {other:?}"),
    }
}

#[test]
fn test_dry_run_launch_path_uses_mocked_api_and_exact_request() {
    let t = TestEnv::new();
    let runner = search_answer(RTX_OFFER);
    let ctx = t.ctx().runner(&runner).build();
    let args = UpArgs {
        gpu: Some("RTX 4090".into()),
        max_price: Some(0.5),
        offer: Some(42),
        dry_run: true,
        ..up_args()
    };
    launch(&ctx, &args, &defaults(), None).unwrap();
    let searches: Vec<String> = runner.argvs().into_iter().map(|a| a[3].clone()).collect();
    assert_eq!(
        searches,
        ["external=false rentable=true verified=true num_gpus=1 reliability>=0.95 disk_space>=20 gpu_name=RTX_4090 dph<=0.5"]
    );
    assert!(!runner.argvs()[0].contains(&"--interruptible".to_string()));
    let output = out(&ctx);
    assert_eq!(
        dry_run_request(&output),
        [
            "vastai",
            "create",
            "instance",
            "42",
            "--image",
            "vastai/base-image:@vastai-automatic-tag",
            "--disk",
            "20",
            "--onstart-cmd",
            EXPECTED_CONTAINER_SCRIPT,
            "--ssh",
            "--direct",
            "--label",
            "gpu-kernels",
            "--env",
            CONTAINER_ENV,
            "--raw",
        ]
    );
    assert!(output.contains("DRY RUN: no instance created"));
    assert!(!output.contains('\u{2014}'));
    assert!(output.contains("\"create\""));
}

const T4_OFFERS: &str = r#"[
    {"id": 20, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.20, "reliability": 0.99, "geolocation": "A", "verification": "verified"},
    {"id": 10, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.10, "reliability": 0.98, "geolocation": "B", "verification": "verified"}
]"#;

#[test]
fn test_yes_without_offer_auto_selects_cheapest_without_prompt() {
    let t = TestEnv::new();
    let runner = search_answer(T4_OFFERS);
    // No input lines: any prompt fails the test.
    let ctx = t.ctx().runner(&runner).build();
    let args = UpArgs {
        gpu: Some("Tesla T4".into()),
        yes: true,
        dry_run: true,
        ..up_args()
    };
    launch(&ctx, &args, &defaults(), None).unwrap();
    assert_eq!(dry_run_request(&out(&ctx))[3], "10");
}

#[test]
fn test_interactive_offer_selection_does_not_print_second_list() {
    let t = TestEnv::new();
    let runner = search_answer(
        r#"[{"id": 42, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.1, "reliability": 0.99, "geolocation": "Test", "verification": "verified"}]"#,
    );
    let ctx = t.ctx().runner(&runner).input(&["1"]).build();
    let args = UpArgs {
        gpu: Some("Tesla T4".into()),
        dry_run: true,
        ..up_args()
    };
    launch(&ctx, &args, &defaults(), None).unwrap();
    let output = out(&ctx);
    assert_eq!(output.matches("1. id=42").count(), 1);
    assert!(!output.contains("\n1. 42\n"));
}

#[test]
fn test_ls_table_and_json_output() {
    let t = TestEnv::new();
    let payload = r#"[{"id": "123", "gpu_name": "RTX 4090", "actual_status": "running", "dph_total": 0.4, "duration": 7342.81}]"#;
    let runner = FakeRunner::new(move |_| ok(payload));
    let ctx = t.ctx().runner(&runner).build();
    assert_eq!(main(&ctx, &["ls"]), 0);
    let output = out(&ctx);
    assert!(output.contains("ID") && output.contains("RTX 4090"));
    assert!(output.contains("Uptime") && output.contains("2h 02m"));
    assert!(!output.contains("7342"));
    assert_eq!(main(&ctx, &["ls", "--json"]), 0);
    assert_eq!(json(&out(&ctx)), json(payload));
    let empty = FakeRunner::new(|_| ok("[]"));
    let ctx = t.ctx().runner(&empty).build();
    assert_eq!(main(&ctx, &["ls"]), 0);
    assert_eq!(out(&ctx).trim(), "No instances. Launch one: gpu up");
}

#[test]
fn test_gpu_cache_honors_xdg_and_rejects_invalid_names() {
    let t = TestEnv::new();
    let env = t.env();
    let cache_path = gpu_cache_path(&env);
    assert_eq!(cache_path, t.cache.join("gpu").join("gpu_models.txt"));
    std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
    std::fs::write(
        &cache_path,
        format!("Tesla T4\nRTX 4090\n\\n\n{}\nbad\tname\n", "x".repeat(101)),
    )
    .unwrap();
    assert_eq!(read_gpu_cache(&env, None), ["RTX 4090", "Tesla T4"]);
    let items = dicts(&format!(
        r#"[{{"gpu_name": "B200"}}, {{"gpu_name": "line\nbreak"}}, {{"gpu_name": "{}"}}, {{"gpu_name": "bad\tname"}}]"#,
        "x".repeat(101)
    ));
    update_gpu_cache(&env, &items, None);
    assert_eq!(read_gpu_cache(&env, None), ["B200", "RTX 4090", "Tesla T4"]);
    let files: Vec<_> = std::fs::read_dir(cache_path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(files, [cache_path]);
    let known = known_gpu_models(&env);
    for name in ["B200", "RTX 4090", "Tesla T4"] {
        assert!(known.contains(&name.to_string()));
    }
}

#[test]
fn test_live_vast_gpu_fixture_has_few_other_models() {
    let names: Vec<&str> = include_str!("../../tests/golden/vast_gpu_models.txt")
        .lines()
        .collect();
    assert!(names.len() >= 70);
    assert!(names.iter().filter(|n| gpu_category(n) == "Other").count() <= 2);
    assert_eq!(gpu_category("RTX 6000Ada"), "Workstation/Pro");
    assert_eq!(gpu_category("Q RTX 8000"), "Workstation/Pro");
    assert_eq!(gpu_category("A800 PCIE"), "Datacenter Ampere/Ada/older");
    assert_eq!(gpu_category("Tesla P40"), "Datacenter Ampere/Ada/older");
}

#[test]
fn test_gpu_default_hint_only_appears_when_recent_pick_exists() {
    let t = TestEnv::new();
    let stats = [("H100 SXM".to_string(), (1.0, 1))].into_iter().collect();
    let ctx = t.ctx().input(&["1", "1"]).build();
    assert_eq!(
        choose_gpu(&ctx, &["H100 SXM".into()], &stats).unwrap(),
        "H100 SXM"
    );
    let output = out(&ctx);
    assert!(
        output.contains("Type a model substring to filter all GPUs, or choose a category.\nGPU: ")
    );
    assert!(!output.contains("GPU [Enter ="));
    assert!(output.contains("(1 offer)"));
    crate::history::record_picks(&t.env(), &[("gpu", "H100 SXM")]).unwrap();
    let ctx = t.ctx().input(&[""]).build();
    assert_eq!(
        choose_gpu(&ctx, &["H100 SXM".into()], &stats).unwrap(),
        "H100 SXM"
    );
    assert!(out(&ctx).contains("\nGPU [Enter = H100 SXM]: "));
}

#[test]
fn test_picker_selects_unique_text_match_without_second_enter() {
    let t = TestEnv::new();
    let ctx = t.ctx().input(&["gpu-k"]).build();
    let choices = Choices::strings(vec!["gpu-kernels".into(), "other".into()]);
    assert_eq!(
        choose(&ctx, &choices, "Repository", None, None).unwrap(),
        "gpu-kernels"
    );
}

#[test]
fn test_interactive_gpu_stats_and_offers_share_unverified_filter() {
    let t = TestEnv::new();
    let runner = search_answer(
        r#"[
        {"id": 1, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.149, "reliability": 0.99, "geolocation": "A", "verification": "verified"},
        {"id": 2, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.095, "reliability": 0.99, "geolocation": "B", "verification": "unverified"},
        {"id": 3, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.05, "reliability": 0.99, "geolocation": "C", "verification": "deverified"}
    ]"#,
    );
    let ctx = t.ctx().runner(&runner).input(&["1", "1", "1"]).build();
    let args = UpArgs {
        dry_run: true,
        ..up_args()
    };
    launch(&ctx, &args, &defaults(), None).unwrap();
    let output = out(&ctx);
    assert!(!runner.argvs()[0][3].contains("verified=true"));
    assert!(output.contains("$0.095/hr (2 offers)"));
    assert!(output.contains("verification=unverified"));
    assert!(!output.contains("deverified"));
    assert_eq!(dry_run_request(&output)[3], "2");
}

#[test]
fn test_picker_filters_and_paginates() {
    let t = TestEnv::new();
    let ctx = t.ctx().input(&["n", "GPU 1", "1"]).build();
    let names: Vec<String> = (0..25).map(|i| format!("GPU {i}")).collect();
    let choices = Choices::labelled(names.clone(), names);
    assert_eq!(choose(&ctx, &choices, "GPU", None, None).unwrap(), "GPU 1");
    let output = out(&ctx);
    assert!(output.contains("page 1/3") && output.contains("page 2/3"));
}

#[test]
fn test_picker_recents_use_frequency_weighted_recency_and_latest_default() {
    let t = TestEnv::new();
    std::fs::create_dir_all(t.history_path().parent().unwrap()).unwrap();
    std::fs::write(t.history_path(), r#"{"repo": ["b", "a", "a", "a"]}"#).unwrap();
    let ctx = t.ctx().input(&[""]).build();
    let choices = Choices::strings(vec!["a".into(), "b".into()]);
    assert_eq!(
        choose(&ctx, &choices, "Repository", Some("repo"), None).unwrap(),
        "b"
    );
    let output = out(&ctx);
    assert!(output.find("1. a").unwrap() < output.find("2. b").unwrap());
}

const T4_ONE: &str = r#"[{"id": 42, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.1, "reliability": 0.99, "geolocation": "Test", "verification": "verified"}]"#;

#[test]
fn test_history_is_written_only_after_successful_confirmed_launch() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|cmd| match strs(&cmd.argv)[..] {
        ["vastai", "search", "offers", ..] => ok(T4_ONE),
        ["vastai", "create", "instance", ..] => ok("created"),
        _ => panic!("unexpected command: {:?}", cmd.argv),
    });
    let ctx = t.ctx().runner(&runner).input(&["yes"]).build();
    let args = UpArgs {
        gpu: Some("Tesla T4".into()),
        offer: Some(42),
        ..up_args()
    };
    launch(&ctx, &args, &defaults(), None).unwrap();
    assert_eq!(
        json(&read(&t.history_path())),
        json(r#"{"mode": ["container"], "gpu": ["Tesla T4"]}"#)
    );
}

#[test]
fn test_unverified_flag_removes_verified_filter_and_lists_all_hosts() {
    let t = TestEnv::new();
    let runner = search_answer(
        r#"[
        {"id": 1, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.1, "reliability": 0.99, "geolocation": "Test", "verification": "deverified"},
        {"id": 2, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.2, "reliability": 0.99, "geolocation": "Test", "verification": "verified"},
        {"id": 3, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.3, "reliability": 0.99, "geolocation": "Test", "verification": "unverified"}
    ]"#,
    );
    let ctx = t.ctx().runner(&runner).build();
    let args = UpArgs {
        unverified: true,
        gpu: Some("Tesla T4".into()),
        yes: true,
        dry_run: true,
        ..up_args()
    };
    launch(&ctx, &args, &defaults(), None).unwrap();
    assert!(!runner.argvs()[0][3].contains("verified="));
    let output = out(&ctx);
    let listing = output.split("DRY RUN").next().unwrap();
    assert_eq!(listing.matches("Tesla T4").count(), 3);
    for verification in ["deverified", "verified", "unverified"] {
        assert!(output.contains(&format!("verification={verification}")));
    }
}

const FAKE_INSTANCE_KEY: &str = "fake-instance-key-8c1f0e";

/// `_confirmed_launch`: a confirmed, flag-driven launch answered by `response`.
fn confirmed_launch(t: &TestEnv, response: Result<Output, String>) -> (i32, String) {
    let runner = FakeRunner::new(move |cmd| match strs(&cmd.argv)[..] {
        ["vastai", "search", "offers", ..] => {
            ok(r#"[{"id": 42, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.1}]"#)
        }
        ["vastai", "create", "instance", ..] => Ok(response
            .clone()
            .unwrap_or_else(|stderr| Output::failed(1, stderr))),
        _ => panic!("unexpected command: {:?}", cmd.argv),
    });
    let ctx = t.ctx().runner(&runner).build();
    let args = UpArgs {
        gpu: Some("Tesla T4".into()),
        offer: Some(42),
        yes: true,
        ..up_args()
    };
    let status = match launch(&ctx, &args, &defaults(), None) {
        Ok(()) => 0,
        Err(err) => {
            ctx.eprintln(&format!("gpu: {}", err.text())).unwrap();
            1
        }
    };
    (status, out(&ctx) + &err(&ctx))
}

#[test]
fn test_up_never_prints_instance_api_key() {
    let raw = format!(
        r#"{{"success": true, "new_contract": 31337, "instance_api_key": "{FAKE_INSTANCE_KEY}"}}"#
    );
    let unparseable = format!("Started. {{'success': True, 'new_contract': 31337, 'instance_api_key': '{FAKE_INSTANCE_KEY}'}}");
    for response in [raw, unparseable] {
        let t = TestEnv::new();
        let (status, captured) = confirmed_launch(&t, Ok(Output::ok(response.clone())));
        assert_eq!(status, 0);
        assert!(!captured.contains(FAKE_INSTANCE_KEY));
        if response.starts_with('{') {
            assert!(captured.ends_with("Created instance 31337.\nConnect: gpu ssh 31337 --exec\n"));
        } else {
            assert!(captured.contains("gpu ls"));
        }
    }
}

#[test]
fn test_up_errors_never_print_instance_api_key() {
    let unsuccessful = Ok(Output::ok(format!(
        r#"{{"success": false, "msg": "bad api_key={FAKE_INSTANCE_KEY}"}}"#
    )));
    let command_failed = Err(format!(
        r#"{{"error": "boom", "instance_api_key": "{FAKE_INSTANCE_KEY}"}}"#
    ));
    for response in [unsuccessful, command_failed] {
        let t = TestEnv::new();
        let (status, captured) = confirmed_launch(&t, response);
        assert_eq!(status, 1);
        assert!(!captured.contains(FAKE_INSTANCE_KEY), "{captured}");
        assert!(!t.history_path().exists());
    }
}

#[test]
fn test_run_redacts_secrets_from_failed_command_output() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|_| {
        Ok(Output::failed(
            1,
            format!("'instance_api_key': '{FAKE_INSTANCE_KEY}', id 7"),
        ))
    });
    let ctx = t.ctx().runner(&runner).build();
    let err = run(&ctx, &["vastai", "create", "instance", "7"], None).unwrap_err();
    assert!(!err.text().contains(FAKE_INSTANCE_KEY));
    assert!(err.text().contains("id 7"));
}

#[test]
fn test_run_redacts_secrets_from_a_real_failing_process() {
    let t = TestEnv::new();
    let script = t.executable(
        "failing",
        &format!(
            "#!/bin/sh\necho \"'instance_api_key': '{FAKE_INSTANCE_KEY}', id 7\" >&2\nexit 1\n"
        ),
    );
    let ctx = t
        .ctx()
        .build()
        .with_runner(std::sync::Arc::new(crate::process::RealRunner));
    let err = run(&ctx, &[script.to_str().unwrap()], None).unwrap_err();
    assert!(!err.text().contains(FAKE_INSTANCE_KEY));
    assert!(err.text().contains("id 7"));
}

#[test]
fn test_real_timeout_does_not_wait_for_a_grandchild_holding_the_pipes() {
    use crate::process::{Cmd, RealRunner, RunError, Runner};
    let t = TestEnv::new();
    let script = t.executable("proxy", "#!/bin/sh\nsleep 5 &\nsleep 5\n");
    let started = std::time::Instant::now();
    let result = RealRunner.run(&Cmd::new(&[script.to_str().unwrap()]).timeout(Some(0.2)));
    assert!(matches!(result, Err(RunError::TimedOut)));
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
}

#[test]
fn test_real_timeout_bounds_the_pipe_drain_after_the_child_exits() {
    use crate::process::{Cmd, RealRunner, RunError, Runner};
    let t = TestEnv::new();
    let script = t.executable("helper", "#!/bin/sh\nsleep 5 &\nexit 0\n");
    let started = std::time::Instant::now();
    let result = RealRunner.run(&Cmd::new(&[script.to_str().unwrap()]).timeout(Some(0.2)));
    assert!(matches!(result, Err(RunError::TimedOut)));
    assert!(started.elapsed() < std::time::Duration::from_secs(3));
}

#[test]
fn test_ls_ssh_and_stop_redact_secrets() {
    let t = TestEnv::new();
    let instances = format!(
        r#"[{{"id": 31337, "actual_status": "stopped", "gpu_name": "RTX 4090", "instance_api_key": "{k}", "jupyter_token": "{k}", "extra_env": [["VAST_API_KEY", "{k}"]]}}]"#,
        k = FAKE_INSTANCE_KEY
    );
    let runner = FakeRunner::new(move |cmd| match cmd.argv[1].as_str() {
        "show" => ok(&instances),
        "ssh-url" => ok(&format!(
            "ssh://root@31337.example:22?token={FAKE_INSTANCE_KEY}"
        )),
        "stop" => ok(&format!(
            "stopping instance 31337. api_key: {FAKE_INSTANCE_KEY}"
        )),
        _ => panic!("unexpected command: {:?}", cmd.argv),
    });
    for argv in [
        &["ls"][..],
        &["ls", "--json"],
        &["ssh", "31337"],
        &["stop", "31337"],
    ] {
        let ctx = t.ctx().runner(&runner).build();
        assert_eq!(main(&ctx, argv), 0, "{argv:?}");
        let (stdout, stderr) = (out(&ctx), err(&ctx));
        assert!(
            !(stdout.clone() + &stderr).contains(FAKE_INSTANCE_KEY),
            "{argv:?}"
        );
        assert!(stdout.contains("31337"), "{argv:?}");
    }
}

#[test]
fn test_dry_run_request_contains_no_credentials() {
    let gpu_kernels = repo("gpu-kernels");
    let args = RequestArgs {
        repo: &gpu_kernels,
        mode: "container",
        interruptible: false,
        disk: None,
    };
    let request =
        argv(&request(&dict(r#"{"id": 42, "dph_total": 0.1}"#), &args, &defaults()).unwrap())
            .unwrap();
    for part in &request {
        for secret in ["api_key", "ghp_", "gho_"] {
            assert!(!part.contains(secret));
        }
    }
}
