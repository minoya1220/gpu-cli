//! No test in this module can invoke a real cloud command.
#[path = "gcp_automation.rs"]
mod automation;
use crate::ctx::Ctx;
use crate::gcp::TpuId;
use crate::process::{Cmd, Output};
use crate::pyjson::{self, Dict, Value};
use crate::testing::*;
use std::sync::{Arc, Mutex};

const CONFIG: &str = r#"{"gcp":{"setup":{"version":1,"project":"test-project"},"project":"test-project","zone":"us-east5-b","runtime":"v2-alpha-tpuv6e","expiry":{"queue":"gpu-expiry","location":"us-central1","service_account":"reaper@test-project.iam.gserviceaccount.com"}}}"#;
const ID: &str = "gcp:test-project:us-east5-b:test-tpu";
const QUEUE: &str = "projects/test-project/locations/us-central1/queues/gpu-expiry";
const UP: &[&str] = &[
    "up",
    "--provider",
    "gcp",
    "--tpu",
    "v6e-1",
    "--no-repo",
    "--expire",
    "2h",
    "--yes",
];

#[derive(Default)]
struct State {
    tasks: Vec<Dict>,
    nodes: Vec<Dict>,
    fail_create: bool,
    reject_creates: usize,
    ambiguous_create: bool,
    fail_presence: bool,
    empty_types: bool,
    fail_arm: bool,
    fail_delete_task: bool,
    corrupt_task: bool,
    paused: bool,
    finite_retries: bool,
    vast_fail: bool,
    creating_reads: usize,
    ssh_failures: usize,
    deny_ssh: bool,
}
fn value(d: Dict) -> String {
    pyjson::dumps(&Value::Dict(d), None)
}
fn list(ds: Vec<Dict>) -> String {
    pyjson::dumps(
        &Value::List(ds.into_iter().map(Value::Dict).collect()),
        None,
    )
}
fn arg(cmd: &Cmd, flag: &str) -> String {
    cmd.argv
        .iter()
        .find_map(|s| s.strip_prefix(&format!("--{flag}=")))
        .unwrap_or_else(|| panic!("missing {flag}: {:?}", cmd.argv))
        .into()
}
fn node(spot: bool) -> Dict {
    let mut d = dict(
        r#"{"name":"projects/test-project/locations/us-east5-b/nodes/test-tpu","state":"READY","acceleratorType":"v6e-1","metadata":{"gpu-expiry-queue":"projects/test-project/locations/us-central1/queues/gpu-expiry","gpu-workspace":"/workspace/demo"}}"#,
    );
    let mut schedule = Dict::new();
    schedule.insert("spot", Value::Bool(spot));
    d.insert("schedulingConfig", Value::Dict(schedule));
    d
}
fn fixture() -> (TestEnv, Ctx, Arc<FakeRunner>, Arc<Mutex<State>>) {
    let env = TestEnv::new();
    let path = env.config.join("gpu");
    std::fs::create_dir(&path).unwrap();
    std::fs::write(path.join("config.json"), CONFIG).unwrap();
    let state = Arc::new(Mutex::new(State::default()));
    let shared = state.clone();
    let native = move |cmd: &Cmd| {
        let mut state = shared.lock().unwrap();
        let a = strs(&cmd.argv);
        if a[0] == "vastai" {
            assert_eq!(a, ["vastai", "show", "instances", "--raw"]);
            if state.vast_fail {
                return Ok(Output::failed(1, "private token=not-for-output"));
            }
            return ok(
                r#"[{"id":123,"gpu_name":"RTX 4090","actual_status":"running","instance_api_key":"not-for-output"}]"#,
            );
        }
        if a[0] == "gcloud" && a.get(1) == Some(&"auth") {
            return Ok(Output::failed(
                1,
                "test deliberately has no Billing credentials",
            ));
        }
        if ["code", "cursor", "zed", "ssh"].contains(&a[0]) {
            return ok("");
        }
        assert_eq!(a[0], "gcloud", "unexpected command: {a:?}");
        assert!(a.contains(&"--quiet"));
        assert!(a.iter().any(|s| s.starts_with("--project=")));
        if a[1] == "tasks" {
            assert_eq!(arg(cmd, "project"), "test-project");
            assert_eq!(arg(cmd, "location"), "us-central1");
            match a[2] {
                "queues" => {
                    return ok(&format!(
                        r#"{{"state":"{}","retryConfig":{{"maxAttempts":{},"maxRetryDuration":"0s","maxBackoff":"60s"}}}}"#,
                        if state.paused { "PAUSED" } else { "RUNNING" },
                        if state.finite_retries { 100 } else { -1 }
                    ))
                }
                "create-http-task" => {
                    if state.fail_arm {
                        return Ok(Output::failed(1, "Permission denied secret=do-not-print"));
                    }
                    let name = a.iter().find(|s| s.starts_with("gpu-expire-")).unwrap();
                    let mut task = Dict::new();
                    task.insert("name", Value::str(format!("{QUEUE}/tasks/{name}")));
                    task.insert("scheduleTime", Value::str(arg(cmd, "schedule-time")));
                    let mut http = Dict::new();
                    http.insert("url", Value::str(arg(cmd, "url")));
                    http.insert("httpMethod", Value::str(arg(cmd, "method")));
                    let mut oauth = Dict::new();
                    oauth.insert(
                        "serviceAccountEmail",
                        Value::str(arg(cmd, "oauth-service-account-email")),
                    );
                    http.insert("oauthToken", Value::Dict(oauth));
                    task.insert("httpRequest", Value::Dict(http));
                    state.tasks.push(task.clone());
                    return ok(&value(task));
                }
                "describe" => {
                    let mut task = state.tasks.last().unwrap().clone();
                    if state.corrupt_task {
                        task.insert("scheduleTime", Value::str("2020-01-01T00:00:00Z"));
                    }
                    return ok(&value(task));
                }
                "list" => return ok(&list(state.tasks.clone())),
                "delete" => {
                    if state.fail_delete_task {
                        return Ok(Output::failed(1, "cannot delete"));
                    }
                    let name = a.iter().find(|s| s.starts_with("gpu-expire-")).unwrap();
                    state
                        .tasks
                        .retain(|t| !crate::gcp::text(t, "name").unwrap().ends_with(*name));
                    return ok("");
                }
                _ => panic!("unexpected task request: {a:?}"),
            }
        }
        assert_eq!(a[1..3], ["compute", "tpus"]);
        if a[3] == "locations" {
            return ok(r#"[{"locationId":"us-east5-b"},{"locationId":"us-central1-a"}]"#);
        }
        assert_eq!(a[3], "tpu-vm");
        match a[4] {
            "accelerator-types" => if state.empty_types { ok("[]") } else { ok(r#"[{"type":"v6e-1"},{"type":"v5p-8"},{"type":"v5p-16"}]"#) },
            "list" if state.fail_presence => Ok(Output::failed(1,"PERMISSION_DENIED")),
            "list" => ok(&list(if ["us-east5-b","-"].contains(&arg(cmd,"zone").as_str()) {state.nodes.clone()} else {Vec::new()})),
            "describe" if state.fail_presence => Ok(Output::failed(1,"PERMISSION_DENIED: secret")),
            "describe" => match state.nodes.iter().find(|n| crate::gcp::text(n,"name").is_some_and(|name| name.ends_with(&format!("/{}", a[5])))) {
                Some(n) => {let mut n=n.clone();if state.creating_reads>0 {state.creating_reads-=1;n.insert("state",Value::str("CREATING"));}ok(&value(n))}, None => Ok(Output::failed(1,"NOT_FOUND: node does not exist")),
            },
            "create" => {
                assert!(!state.tasks.is_empty(), "create without a deletion task");
                let reject=state.fail_create || state.reject_creates>0;
                state.reject_creates=state.reject_creates.saturating_sub(1);
                if reject { return Ok(Output::failed(1, if state.ambiguous_create { "unknown transport failure" } else { "RESOURCE_EXHAUSTED capacity stockout token=do-not-print" })); }
                let mut n = node(a.contains(&"--spot"));
                n.insert("name", Value::str(format!("projects/test-project/locations/{}/nodes/{}",arg(cmd,"zone"), a[5])));
                state.nodes.push(n);
                ok("{}")
            },
            "stop" => {
                state.nodes[0].insert("state", Value::str("STOPPED"));
                ok("{}")
            },
            "delete" => { state.nodes.clear(); ok("{}") },
            "ssh" if a.contains(&"--dry-run") => ok("/usr/bin/ssh -t -i /tmp/test-key -o CheckHostIP=no -o HostKeyAlias=compute.123 -o UserKnownHostsFile=/tmp/known_hosts developer@192.0.2.10"),
            "ssh" => {
                assert!(a.contains(&"--command=true"));
                if state.deny_ssh {return Ok(Output::failed(1,"PERMISSION_DENIED token=do-not-print"));}
                if state.ssh_failures>0 {state.ssh_failures-=1;return Ok(Output::failed(1,"ssh exited with return code [255]: Permission denied (publickey) token=do-not-print"));}
                ok("")
            },
            _ => panic!("unexpected cloud request: {a:?}"),
        }
    };
    let runner = FakeRunner::new(move |cmd| {
        super::gcp_transport::adapt(cmd, &native).unwrap_or_else(|| native(cmd))
    });
    let ctx = env
        .ctx()
        .runner(&runner)
        .clock(&FakeClock::at("2026-11-03T00:00:00Z".parse().unwrap()))
        .build();
    tpu_facts(&ctx);
    (env, ctx, runner, state)
}

#[test]
fn launch_arms_and_verifies_before_create_and_never_uses_vast_or_ssh() {
    let (_env, ctx, runner, state) = fixture();
    let mut flags = UP.to_vec();
    flags.push("--spot");
    assert_eq!(main(&ctx, &flags), 0, "{}", err(&ctx));
    let calls = runner.argvs();
    let task = calls
        .iter()
        .position(|a| a.get(2).is_some_and(|s| s == "create-http-task"))
        .unwrap();
    let paid = calls
        .iter()
        .position(|a| a.get(4).is_some_and(|s| s == "create"))
        .unwrap();
    assert!(task < paid && calls[task + 1][2] == "describe");
    assert!(calls[paid].contains(&"--accelerator-type=v6e-1".into()));
    assert!(calls[paid].contains(&"--spot".into()));
    assert!(!calls[paid].iter().any(|a| a.contains("reaper@")));
    assert_eq!(state.lock().unwrap().tasks.len(), 1);
    assert!(out(&ctx).contains("Next: gpu ssh gcp:test-project:us-east5-b:gpu-"));
}

#[test]
fn dry_run_is_offline_aliases_v5e_and_rejects_missing_or_invalid_safety_flags() {
    let (_env, ctx, runner, _state) = fixture();
    let mut flags = UP.to_vec();
    flags.extend(["--tpu", "v5e-1", "--dry-run"]);
    assert_eq!(main(&ctx, &flags), 0, "{}", err(&ctx));
    let printed = out(&ctx);
    assert!(printed.contains("--accelerator-type=v5litepod-1"));
    assert!(printed.contains("--method=DELETE"));
    assert!(printed.contains("--metadata=^|^startup-script="));
    assert!(runner.calls().is_empty());
    for extra in [
        vec!["--expire", "30s"],
        vec!["--expire", "30d"],
        vec!["--gpu", "H100"],
        vec!["--runtime", "bad|value"],
        vec!["--zone", "../zone"],
    ] {
        let mut flags = UP.to_vec();
        flags.extend(extra);
        assert_ne!(main(&ctx, &flags), 0);
    }
    assert_eq!(
        main(
            &ctx,
            &[
                "up",
                "--provider",
                "gcp",
                "--tpu",
                "v6e-1",
                "--no-repo",
                "--yes"
            ]
        ),
        2
    );
    assert!(runner.calls().is_empty());
}

#[test]
fn runtime_is_chosen_from_each_generation_not_the_legacy_config() {
    let (_env, ctx, runner, _state) = fixture(); // legacy runtime is v6e
    for (tpu, runtime) in [
        ("v2-8", "tpu-ubuntu2204-base"),
        ("v3-8", "tpu-ubuntu2204-base"),
        ("v4-8", "tpu-ubuntu2204-base"),
        ("v5e-1", "v2-alpha-tpuv5-lite"),
        ("v5litepod-8", "v2-alpha-tpuv5-lite"),
        ("v5p-8", "v2-alpha-tpuv5"),
        ("v6e-1", "v2-alpha-tpuv6e"),
    ] {
        let mut args = UP.to_vec();
        args.extend(["--tpu", tpu, "--dry-run"]);
        assert_eq!(main(&ctx, &args), 0, "{}", err(&ctx));
        assert!(out(&ctx).contains(&format!("--version={runtime}")), "{tpu}");
    }
    assert!(runner.calls().is_empty());
}

#[test]
fn unknown_generations_need_an_explicit_runtime_and_known_defaults_can_be_overridden() {
    let (_env, ctx, runner, _state) = fixture();
    let mut args = UP.to_vec();
    args.extend(["--tpu", "v99-1", "--dry-run"]);
    assert_eq!(main(&ctx, &args), 1);
    let message = err(&ctx);
    assert!(message.contains("Unknown runtime"), "{message}");
    assert!(message.contains("--runtime <image>"));
    for tpu in ["v99-1", "v5e-1"] {
        let mut args = UP.to_vec();
        args.extend(["--tpu", tpu, "--runtime", "custom-image", "--dry-run"]);
        assert_eq!(main(&ctx, &args), 0, "{}", err(&ctx));
        assert!(out(&ctx).contains("--version=custom-image"));
    }
    assert!(runner.calls().is_empty());
}

#[test]
fn tpu_flags_infer_gcp_but_never_override_an_explicit_vast_provider() {
    for provider in [None, Some("vast")] {
        let (_env, ctx, runner, _state) = fixture();
        let mut flags = vec![
            "up",
            "--tpu",
            "v6e-1",
            "--no-repo",
            "--expire",
            "2h",
            "--dry-run",
        ];
        if let Some(provider) = provider {
            flags.extend(["--provider", provider]);
        }
        assert_eq!(main(&ctx, &flags), if provider.is_some() { 1 } else { 0 });
        if provider.is_some() {
            assert!(err(&ctx).contains("TPU flags do not apply to Vast"));
        } else {
            assert!(out(&ctx).contains("DRY RUN: no TPU or expiry task created"));
        }
        assert!(runner.calls().is_empty());
    }
}

#[test]
fn tpu_picker_drops_ui_mode_but_preserves_explicit_vast_flags_for_rejection() {
    use crate::launch::UpArgs;
    use crate::repos::Repo;
    use crate::wizard::{Picks, TpuChoice};
    let (_env, ctx, runner, _state) = fixture();
    let cfg = crate::config::load_config(&ctx.env).unwrap();
    let deadline = ctx
        .clock
        .now()
        .checked_add(std::time::Duration::from_secs(7200))
        .unwrap();
    let picks = Picks {
        tpu: Some(TpuChoice::Type("v6e-1".into())),
        ..Picks::new(Some(Repo::NoRepo), Some("container"), None)
    };
    let args = UpArgs {
        num_gpus: 1,
        dry_run: true,
        ..UpArgs::default()
    };
    crate::gcp::launch_from_picker(&ctx, &args, &picks, &cfg, Some(deadline)).unwrap();
    assert!(out(&ctx).contains("DRY RUN: no TPU or expiry task created"));
    for args in [
        UpArgs {
            mode: Some("container".into()),
            ..args.clone()
        },
        UpArgs {
            max_price: Some(0.5),
            ..args.clone()
        },
        UpArgs {
            offer: Some(100),
            ..args.clone()
        },
        UpArgs {
            disk: Some(30),
            ..args.clone()
        },
        UpArgs {
            interruptible: true,
            ..args.clone()
        },
    ] {
        let error =
            crate::gcp::launch_from_picker(&ctx, &args, &picks, &cfg, Some(deadline)).unwrap_err();
        assert!(error.text().contains("flags do not apply to TPUs"));
    }
    assert!(runner.calls().is_empty());
}

#[test]
fn queue_or_task_failures_prevent_paid_create() {
    for failure in 0..4 {
        let (_env, ctx, runner, state) = fixture();
        {
            let mut s = state.lock().unwrap();
            match failure {
                0 => s.paused = true,
                1 => s.finite_retries = true,
                2 => s.fail_arm = true,
                _ => s.corrupt_task = true,
            }
        }
        assert_eq!(main(&ctx, UP), 1);
        assert!(!runner
            .argvs()
            .iter()
            .any(|a| a.get(4).is_some_and(|s| s == "create")));
        assert!(state.lock().unwrap().nodes.is_empty());
        assert!(!err(&ctx).contains("do-not-print"));
    }
}

#[test]
fn stockout_with_verified_absence_cleans_its_task_and_reports_one_next_command() {
    let (_env, ctx, _runner, state) = fixture();
    state.lock().unwrap().fail_create = true;
    assert_eq!(main(&ctx, UP), 10);
    let message = err(&ctx);
    assert!(
        message.contains("No on-demand capacity in us-east5-b"),
        "{message}"
    );
    assert!(message.contains("Other v6e-1 zones:"));
    assert!(!message
        .split("Next:")
        .nth(1)
        .unwrap()
        .contains("--zone us-east5-b"));
    assert_eq!(message.matches("Next:").count(), 1);
    assert!(!message.contains("do-not-print"));
    assert!(message.contains("Deletion task removed"));
    assert!(state.lock().unwrap().tasks.is_empty());
}

#[test]
fn lists_are_provider_labelled_and_gcp_only_does_not_require_vast() {
    let (_env, ctx, runner, state) = fixture();
    state.lock().unwrap().nodes.push(node(false));
    assert_eq!(main(&ctx, &["ls"]), 0, "{}", err(&ctx));
    let printed = out(&ctx);
    assert!(printed.contains("Provider"));
    assert!(printed.contains("vast") && printed.contains("gcp"));
    assert!(printed.contains("test-tpu"));
    assert!(!printed.contains(ID));
    assert_eq!(printed.lines().count(), 3);
    assert!(!printed.contains("not-for-output"));
    let start = runner.calls().len();
    assert_eq!(main(&ctx, &["ls", "--json", "--provider", "gcp"]), 0);
    assert!(runner.argvs()[start..]
        .iter()
        .all(|a| ["gcloud", "curl"].contains(&a[0].as_str())));
    assert!(out(&ctx).contains("\"provider\": \"gcp\""));
}

#[test]
fn unavailable_vast_does_not_hide_tpus_but_listing_is_nonzero() {
    let (_env, ctx, _runner, state) = fixture();
    state.lock().unwrap().vast_fail = true;
    state.lock().unwrap().nodes.push(node(false));
    assert_eq!(main(&ctx, &["ls"]), 1);
    assert!(out(&ctx).contains("test-tpu"));
    assert!(err(&ctx).contains("gpu ls --provider vast"));
}

#[test]
fn tpu_listing_uses_cached_slice_prices_and_create_time_without_price_requests() {
    for spot in [false, true] {
        let (env, ctx, runner, state) = fixture();
        super::gcp_prices::cache(&env);
        let mut n = node(spot);
        n.insert("createTime", Value::str("2026-11-02T23:53:00Z"));
        state.lock().unwrap().nodes.push(n);
        assert_eq!(main(&ctx, &["ls", "--provider", "gcp"]), 0, "{}", err(&ctx));
        let output = out(&ctx);
        assert!(
            output.contains(if spot { "$0.75/hr" } else { "$2.75/hr" }),
            "{output}"
        );
        assert!(
            output.contains("ready") && output.contains("7m"),
            "{output}"
        );
        assert!(!output.contains(ID));
        assert_eq!(
            runner.calls().len(),
            3,
            "listing must keep its constant inventory-only call count"
        );
        assert_eq!(main(&ctx, &["ls", "--provider", "gcp", "--long"]), 0);
        assert!(out(&ctx).contains(ID));
        assert_eq!(main(&ctx, &["ls", "--provider", "gcp", "--json"]), 0);
        let listing = json(&out(&ctx));
        let listed = listing
            .as_dict()
            .unwrap()
            .get("result")
            .unwrap()
            .as_dict()
            .unwrap()
            .get("instances")
            .unwrap()
            .as_list()
            .unwrap()[0]
            .as_dict()
            .unwrap();
        assert_eq!(listed.get("id"), Some(&Value::str(ID)));
        assert_eq!(listed.get("duration"), Some(&Value::Int(420)));
        assert_eq!(
            listed.get("dph_total"),
            Some(&Value::Float(if spot { 0.75 } else { 2.75 }))
        );
        assert!(!runner
            .argvs()
            .iter()
            .flatten()
            .any(|a| a.contains("cloudbilling")));
    }
}

#[test]
fn unknown_tpu_create_times_stay_unknown_and_v5litepod_has_its_public_name() {
    for created in ["bad timestamp", "2026-11-04T00:00:00Z"] {
        let (_env, ctx, _, state) = fixture();
        let mut n = node(false);
        n.insert("createTime", Value::str(created));
        n.insert("acceleratorType", Value::str("v5litepod-1"));
        state.lock().unwrap().nodes.push(n);
        assert_eq!(main(&ctx, &["ls", "--provider", "gcp", "--json"]), 0);
        let listing = json(&out(&ctx));
        let listed = listing
            .as_dict()
            .unwrap()
            .get("result")
            .unwrap()
            .as_dict()
            .unwrap()
            .get("instances")
            .unwrap()
            .as_list()
            .unwrap()[0]
            .as_dict()
            .unwrap();
        assert_eq!(listed.get("gpu_name"), Some(&Value::str("v5e-1")));
        assert!(listed.get("duration").is_none());
        assert!(listed.get("dph_total").is_none());
    }
}

#[test]
fn short_tpu_ids_resolve_freshly_for_ssh_edit_expire_stop_and_down() {
    let (env, ctx, runner, state) = fixture();
    env.executable("code", "#!/bin/sh\nexit 0\n");
    env.executable("cursor", "#!/bin/sh\nexit 0\n");
    let mut n = node(false);
    n.insert(
        "name",
        Value::str("projects/test-project/locations/us-east5-b/nodes/gpu-123-uwhcp7"),
    );
    state.lock().unwrap().nodes.push(n);
    for args in [
        vec!["ssh", "gpu-uwhcp7"],
        vec!["edit", "gpu-uwhcp7", "--editor", "code"],
        vec!["cursor", "gpu-uwhcp7"],
        vec!["expire", "gpu-uwhcp7", "2h"],
        vec!["stop", "gpu-uwhcp7"],
        vec!["down", "gpu-uwhcp7", "--yes"],
    ] {
        assert_eq!(main(&ctx, &args), 0, "{args:?}: {}", err(&ctx));
        let _ = out(&ctx);
    }
    assert!(state.lock().unwrap().nodes.is_empty());
    assert!(state.lock().unwrap().tasks.is_empty());
    assert_eq!(main(&ctx, &["ssh", "gpu-uwhcp7"]), 1);
    assert!(err(&ctx).contains("was not found in test-project"));
    assert!(runner.argvs().iter().all(|a| a[0] != "vastai"));
    assert!(runner
        .argvs()
        .iter()
        .any(|a| a.get(4).is_some_and(|s| s == "delete")
            && a.get(5).is_some_and(|s| s == "gpu-123-uwhcp7")));
}

#[test]
fn ambiguous_short_ids_never_pick_a_node_or_mutate_and_full_ids_still_work() {
    let (_env, ctx, runner, state) = fixture();
    for (zone, name) in [
        ("us-east5-b", "gpu-100-same"),
        ("us-central1-a", "gpu-200-same"),
    ] {
        let mut n = node(false);
        n.insert(
            "name",
            Value::str(format!(
                "projects/test-project/locations/{zone}/nodes/{name}"
            )),
        );
        state.lock().unwrap().nodes.push(n);
    }
    assert_eq!(main(&ctx, &["down", "gpu-same", "--yes"]), 1);
    let error = err(&ctx);
    assert!(error.contains("ambiguous") && error.contains("gpu ls --provider gcp --long"));
    assert_eq!(state.lock().unwrap().nodes.len(), 2);
    assert!(runner
        .argvs()
        .iter()
        .all(|a| a[0] == "curl" || a.get(1).is_some_and(|s| s == "auth")));
    let before = runner.calls().len();
    assert_eq!(
        main(
            &ctx,
            &["ssh", "gcp:test-project:us-central1-a:gpu-200-same"]
        ),
        0
    );
    assert!(out(&ctx).contains("--zone=us-central1-a"));
    assert_eq!(runner.calls().len(), before, "full IDs bypass alias lookup");
    assert_eq!(main(&ctx, &["ssh", "gpu-100-same"]), 0);
    assert!(out(&ctx).contains("ssh gpu-100-same"));
}

#[test]
fn short_tpu_names_before_setup_never_run_gcloud() {
    let env = TestEnv::new();
    let runner = FakeRunner::forbid();
    let ctx = env.ctx().runner(&runner).build();
    assert_eq!(main(&ctx, &["ssh", "gpu-short"]), 12);
    assert!(err(&ctx).contains("Next: gpu setup"));
    assert!(runner.calls().is_empty());
}

#[test]
fn explicit_ssh_routes_without_listing_and_exec_uses_gcloud() {
    let (_env, ctx, runner, _state) = fixture();
    assert_eq!(main(&ctx, &["ssh", ID]), 0);
    assert!(out(&ctx).contains("gcloud compute tpus tpu-vm ssh test-tpu --project=test-project --zone=us-east5-b --quiet --worker=0"));
    assert!(runner.calls().is_empty());
    assert_eq!(main(&ctx, &["ssh", ID, "--exec"]), 1); // fake exec returns, real exec replaces process
    assert_eq!(runner.argvs()[0][0], "gcloud");
}

#[test]
fn edit_reuses_gcloud_user_keys_and_workspace_in_temp_home() {
    let (env, ctx, runner, state) = fixture();
    env.executable("code", "#!/bin/sh\nexit 0\n");
    state.lock().unwrap().nodes.push(node(false));
    assert_eq!(
        main(&ctx, &["edit", ID, "--editor", "code"]),
        0,
        "{}",
        err(&ctx)
    );
    let ssh = read(&env.home.join(".ssh/gpu_gcp_config"));
    assert!(ssh.contains("User \"developer\""));
    assert!(ssh.contains("IdentityFile \"/tmp/test-key\""));
    assert!(read(&env.home.join(".ssh/config")).starts_with("Include ~/.ssh/gpu_gcp_config\n"));
    let calls = runner.argvs();
    assert_eq!(
        calls.last().unwrap(),
        &vec![
            "code".to_string(),
            "--remote".into(),
            "ssh-remote+gcp_test-project_us-east5-b_test-tpu".into(),
            "/workspace/demo".into()
        ]
    );
}

#[test]
fn every_tpu_editor_waits_for_creating_and_ssh_key_propagation_without_mutating_lifecycle() {
    for editor in ["cursor", "code", "zed", "nvim"] {
        let (env, ctx, runner, state) = fixture();
        env.executable(editor, "#!/bin/sh\nexit 0\n");
        {
            let mut s = state.lock().unwrap();
            s.nodes.push(node(false));
            s.creating_reads = 2;
            s.ssh_failures = 2;
        }
        assert_eq!(
            main(&ctx, &["edit", ID, "--editor", editor]),
            0,
            "{}",
            err(&ctx)
        );
        let progress = err(&ctx);
        assert!(progress.contains("CREATING"));
        assert!(progress.contains("READY"));
        assert!(progress.contains("waiting for SSH"));
        assert!(!progress.contains("do-not-print"));
        assert!(out(&ctx).contains("/workspace/demo"));
        for call in runner.calls() {
            if call.argv[0] == "gcloud" {
                assert!(call.timeout.is_some_and(|t| t <= 15.0));
                assert!(!call.argv.iter().any(|s| [
                    "create",
                    "create-http-task",
                    "delete",
                    "stop",
                    "start"
                ]
                .contains(&s.as_str())));
            }
        }
        assert_eq!(
            crate::commands::last_editor(&ctx.env).as_deref(),
            Some(editor)
        );
    }
}
#[test]
fn tpu_editor_readiness_timeout_terminal_state_and_iam_failure_do_not_open_or_change_expiry() {
    for mode in ["timeout", "stopped", "denied", "malformed"] {
        let (env, ctx, runner, state) = fixture();
        env.executable("code", "#!/bin/sh\nexit 0\n");
        let mut n = node(false);
        if mode == "timeout" {
            n.insert("state", Value::str("CREATING"));
        }
        if mode == "stopped" {
            n.insert("state", Value::str("STOPPED"));
        }
        if mode == "malformed" {
            n.insert("state", Value::str("token=do-not-print"));
        }
        {
            let mut s = state.lock().unwrap();
            s.nodes.push(n);
            s.deny_ssh = mode == "denied";
        }
        assert_eq!(
            main(&ctx, &["edit", ID, "--editor", "code"]),
            if mode == "timeout" { 14 } else { 1 }
        );
        let error = err(&ctx);
        assert!(!error.contains("do-not-print"));
        if mode == "timeout" {
            assert!(error.contains("Timed out waiting for TPU"));
            assert!(error.contains("expiry is unchanged"));
        }
        assert!(!runner.argvs().iter().any(|a| a[0] == "code"
            || a.iter().any(
                |s| ["create", "create-http-task", "delete", "stop", "start"].contains(&s.as_str())
            )));
        assert!(!env.history_path().exists());
        assert!(!env.home.join(".ssh/gpu_gcp_config").exists());
    }
}

#[test]
fn stop_preserves_expiry_and_spot_is_rejected_without_mutation() {
    for spot in [false, true] {
        let (_env, ctx, runner, state) = fixture();
        state.lock().unwrap().nodes.push(node(spot));
        assert_eq!(
            main(&ctx, &["stop", ID]),
            if spot { 1 } else { 0 },
            "{}",
            err(&ctx)
        );
        let calls = runner.argvs();
        assert!(calls.iter().all(|a| a[1] == "compute"));
        assert_eq!(calls.iter().any(|a| a[4] == "stop"), !spot);
    }
}

#[test]
fn down_verifies_disappearance_before_cancelling_tasks_and_is_repeatable() {
    let (_env, ctx, runner, state) = fixture();
    state.lock().unwrap().nodes.push(node(false));
    assert_eq!(main(&ctx, &["expire", ID, "2h"]), 0, "{}", err(&ctx));
    let start = runner.calls().len();
    assert_eq!(main(&ctx, &["down", ID, "--yes"]), 0, "{}", err(&ctx));
    let calls = &runner.argvs()[start..];
    let paid = calls
        .iter()
        .position(|a| a.get(4).is_some_and(|s| s == "delete"))
        .unwrap();
    assert_eq!(calls[paid + 1][4], "list");
    let task_delete = calls
        .iter()
        .position(|a| a.get(2).is_some_and(|s| s == "delete"))
        .unwrap();
    assert!(paid < task_delete);
    assert!(state.lock().unwrap().tasks.is_empty());
    assert_eq!(main(&ctx, &["down", ID, "--yes"]), 0);
}

#[test]
fn expiry_replaces_safely_shows_and_cancels_without_guest_access() {
    let (_env, ctx, runner, state) = fixture();
    state.lock().unwrap().nodes.push(node(false));
    for when in ["2h", "3h", "show", "cancel", "cancel"] {
        assert_eq!(
            main(&ctx, &["expire", ID, when]),
            0,
            "{when}: {}",
            err(&ctx)
        );
    }
    assert!(state.lock().unwrap().tasks.is_empty());
    assert!(runner
        .argvs()
        .iter()
        .all(|a| ["gcloud", "curl"].contains(&a[0].as_str()) && !a.contains(&"ssh".into())));
    assert_eq!(main(&ctx, &["expire", ID, "show"]), 1);
    assert!(err(&ctx).contains("unarmed"));
}

#[test]
fn failed_replacement_never_cancels_the_old_deadline() {
    let (_env, ctx, _runner, state) = fixture();
    state.lock().unwrap().nodes.push(node(false));
    assert_eq!(main(&ctx, &["expire", ID, "2h"]), 0);
    state.lock().unwrap().fail_arm = true;
    assert_eq!(main(&ctx, &["expire", ID, "3h"]), 1);
    assert_eq!(state.lock().unwrap().tasks.len(), 1);
    state.lock().unwrap().fail_arm = false;
    state.lock().unwrap().fail_delete_task = true;
    assert_eq!(main(&ctx, &["expire", ID, "4h"]), 1);
    assert_eq!(state.lock().unwrap().tasks.len(), 2);
    assert!(err(&ctx).contains("earlier deadline"));
}

#[test]
fn cancel_can_clean_up_a_failed_create_with_no_node() {
    let (_env, ctx, _runner, state) = fixture();
    {
        let mut s = state.lock().unwrap();
        s.fail_create = true;
        s.ambiguous_create = true;
    }
    assert_eq!(main(&ctx, UP), 1);
    let url = {
        let s = state.lock().unwrap();
        crate::gcp::text(
            s.tasks[0].get("httpRequest").unwrap().as_dict().unwrap(),
            "url",
        )
        .unwrap()
        .to_string()
    };
    let name = url.rsplit('/').next().unwrap();
    assert_eq!(
        main(
            &ctx,
            &[
                "expire",
                &format!("gcp:test-project:us-east5-b:{name}"),
                "cancel"
            ]
        ),
        0,
        "{}",
        err(&ctx)
    );
    assert!(state.lock().unwrap().tasks.is_empty());
}

#[test]
fn tpu_flag_routes_without_provider_and_catalog_jobs_use_real_keys() {
    let (env, _ctx, runner, _state) = fixture();
    let ctx = env.ctx().runner(&runner).interactive(true).build();
    assert_eq!(
        main(
            &ctx,
            &[
                "up",
                "--tpu",
                "v6e-1",
                "--no-repo",
                "--expire",
                "2h",
                "--on-demand",
                "--zone",
                "us-east5-b",
                "--dry-run"
            ]
        ),
        0,
        "{}",
        err(&ctx)
    );
    assert_eq!(ctx.ui.remaining(), 0);
    assert!(runner.calls().is_empty());
    // Scripted EOF cancels a pending loader; drive async catalogs with a
    // gated Runner and wait for rendered choices before sending Enter.
    use crate::picker::{testing::*, JobState, Outcome};
    use crate::process::Runner;
    let settings = crate::gcp::Settings::load(&dict(CONFIG)).unwrap();
    for (kind, title, expected) in [
        ("locations", "TPU zone", "us-east5-b"),
        ("accelerator-types", "TPU type", "v6e-1"),
    ] {
        let _ = std::fs::remove_file(env.cache.join("gpu/tpu-catalog-test-project.json"));
        let gate = super::picker::Gate::default();
        let worker_gate = gate.clone();
        let inner = runner.clone();
        let gated_runner = FakeRunner::new(move |cmd| {
            assert!(worker_gate.wait(), "test never released catalog loader");
            inner.run(cmd)
        });
        let ctx = env.ctx().runner(&gated_runner).build();
        let job = crate::gcp_wizard::catalog_job(&ctx, &settings, Some("us-east5-b"), kind, None);
        let observer = job.observer();
        let release = gate.clone();
        let result = drive(
            crate::gcp_wizard::catalog_screen(job, title, crate::picker::Next::Done),
            move |keys| {
                keys.wait_for("the catalog job", || observer.state() == JobState::Running);
                assert!(keys.labels().is_empty());
                release.set();
                keys.wait(on(title));
                keys.wait(|s| s.labels.contains(&expected.to_string()));
                keys.press(RIGHT);
            },
        );
        gate.set();
        assert_eq!(result, Outcome::Done(expected.to_string()));
    }
    assert_eq!(runner.calls().len(), 5);
}

#[test]
fn catalog_choices_share_a_stack_and_back_does_not_refetch() {
    use crate::picker::{testing::*, Next, Outcome};
    let (_env, ctx, runner, _state) = fixture();
    let settings = crate::gcp::Settings::load(&dict(CONFIG)).unwrap();
    let Next::Screen(first) = crate::gcp_wizard::next(ctx, settings, Default::default()) else {
        panic!("missing first screen")
    };
    let result = drive(first, move |keys| {
        keys.wait(on("TPU generation"));
        assert!(keys.labels().len() <= 7);
        keys.type_text("v6e");
        keys.press_until(RIGHT, on("v6e size"));
        assert!(keys.labels().iter().all(|l| l.contains("chip")));
        keys.press_until(RIGHT, on("TPU capacity"));
        keys.press_until(RIGHT, on("TPU zone"));
        keys.wait(|s| s.labels == ["us-east     1  zone", "us-central  1  zone"]);
        keys.press_until(RIGHT, on("us-east zones"));
        assert!(!keys.labels().iter().any(|l| l.contains('\u{2014}')));
        keys.press_until(RIGHT, on("Destroy after (e.g. 2h)"));
        keys.press_until(BACK, on("us-east zones"));
        keys.press_until(BACK, on("TPU zone"));
        keys.press_until(BACK, on("TPU capacity"));
        keys.press_until(BACK, on("v6e size"));
        keys.press_until(BACK, on("TPU generation"));
        keys.press(CTRL_C);
    });
    assert_eq!(result, Outcome::Cancelled(130));
    assert_eq!(
        runner
            .calls()
            .iter()
            .filter(|c| c.program() == "gcloud" && c.argv.get(1).is_some_and(|a| a == "auth"))
            .count(),
        1
    );
}

#[test]
fn expiry_entry_has_duration_help_validation_and_back_to_capacity() {
    use crate::picker::{testing::*, Next, Outcome};
    let (_env, ctx, runner, _state) = fixture();
    let settings = crate::gcp::Settings::load(&dict(CONFIG)).unwrap();
    let Next::Screen(first) = crate::gcp_wizard::next(
        ctx,
        settings,
        crate::gcp_wizard::Selection {
            zone: Some("us-east5-b".into()),
            tpu: Some("v6e-1".into()),
            ..Default::default()
        },
    ) else {
        panic!("missing capacity screen")
    };
    let result = drive(first, |keys| {
        // This direct capacity entry has no prefetched job. Wait for its one
        // price job to finish so the call-count assertion is deterministic.
        keys.wait(|s| s.footer.contains("Ctrl-R retry"));
        keys.press_until(RIGHT, on("Destroy after (e.g. 2h)"));
        assert!(keys
            .snapshot()
            .body
            .iter()
            .any(|s| s.contains("30m, 2h or 1d")));
        assert!(!keys.snapshot().body.iter().any(|s| s.contains("HTTPS")));
        assert!(keys.footer().contains("back"));
        assert!(!keys.footer().contains("quit (twice)"));
        keys.type_text("30s");
        keys.press(RIGHT);
        assert!(keys
            .snapshot()
            .body
            .iter()
            .any(|s| s.contains("1 minute to 29 days")));
        keys.press_until(BACK, on("TPU capacity"));
        keys.press_until(RIGHT, on("Destroy after (e.g. 2h)"));
        keys.type_text("1d");
        keys.press(RIGHT);
    });
    let Outcome::Done(selection) = result else {
        panic!("{result:?}")
    };
    assert_eq!(
        selection.expiry_at.unwrap().to_string(),
        "2026-11-04T00:00:00Z"
    );
    // Exactly one read-only job, not another fetch when returning to capacity.
    // The fixture allows token auth but denies the two Billing reads.
    let calls = runner.calls();
    assert_eq!(calls.len(), 3, "actual calls: {:?}", runner.argvs());
    // The SKU refresh runs beside pricing from cached SKUs, so the services
    // read and the batch price read may start in either order.
    let parallel = |c: &crate::process::Cmd| c.argv.iter().any(|a| a == "--parallel");
    let (refresh, batch) = if parallel(&calls[1]) {
        (&calls[2], &calls[1])
    } else {
        (&calls[1], &calls[2])
    };
    assert_eq!(
        strs(&calls[0].argv),
        [
            "gcloud",
            "auth",
            "print-access-token",
            "--project=test-project",
            "--quiet"
        ]
    );
    let curl = [
        "curl",
        "--disable",
        "--silent",
        "--show-error",
        "--noproxy",
        "*",
        "--proto",
        "=https",
        "--max-time",
        "15",
    ];
    assert_eq!(
        strs(&refresh.argv),
        [
            curl.as_slice(),
            &[
                "--config",
                "-",
                "--write-out",
                "\n%{http_code}",
                "https://cloudbilling.googleapis.com/v1/services?pageSize=5000&pageToken="
            ]
        ]
        .concat()
    );
    assert_eq!(
        strs(&batch.argv),
        [
            curl.as_slice(),
            &[
                "--parallel",
                "--parallel-max",
                "8",
                "--config",
                "-",
                "--write-out",
                "%{filename_effective}\t%{http_code}\t%{time_total}\n"
            ]
        ]
        .concat()
    );
    let crate::process::Input::Text(config) = &batch.stdin else {
        panic!("missing batch price URLs")
    };
    let urls = config
        .lines()
        .filter_map(|l| l.strip_prefix("url = \""))
        .collect::<Vec<_>>();
    assert!(!urls.is_empty());
    assert!(urls.iter().all(|url| url
        .starts_with("https://cloudbilling.googleapis.com/v1beta/skus/")
        && url.ends_with("/price?currencyCode=USD\"")));
}

#[test]
fn real_clock_precision_does_not_break_task_verification() {
    let (env, _ctx, runner, state) = fixture();
    let ctx = env
        .ctx()
        .runner(&runner)
        .clock(&FakeClock::at(
            "2026-11-03T00:00:00.123456789Z".parse().unwrap(),
        ))
        .build();
    assert_eq!(main(&ctx, UP), 0, "{}", err(&ctx));
    assert_eq!(
        crate::gcp::text(&state.lock().unwrap().tasks[0], "scheduleTime"),
        Some("2026-11-03T02:00:00Z")
    );
}

#[test]
fn declining_creation_or_destruction_never_creates_paid_resources() {
    let (env, _ctx, runner, _state) = fixture();
    let ctx = env.ctx().runner(&runner).input(&["no", "no"]).build();
    let flags: Vec<_> = UP.iter().copied().filter(|s| *s != "--yes").collect();
    assert_eq!(main(&ctx, &flags), 2);
    assert_eq!(main(&ctx, &["down", ID]), 2);
    assert!(!runner
        .argvs()
        .iter()
        .any(|a| a.get(4).is_some_and(|s| s == "create")));
}

#[test]
fn malformed_node_list_never_verifies_destruction() {
    let (env, _, _, _) = fixture();
    let runner = super::gcp_transport::runner(|cmd| {
        if cmd.argv[1] == "tasks" {
            return ok("[]");
        }
        assert_eq!(cmd.argv[4], "list");
        ok(r#"[{"state":"READY"}]"#)
    });
    let ctx = env.ctx().runner(&runner).build();
    assert_eq!(main(&ctx, &["down", ID, "--yes"]), 1);
    assert!(err(&ctx).contains("incomplete reply"));
    assert_eq!(runner.calls().len(), 3);
}

#[test]
fn impossible_type_is_rejected_before_confirmation_or_arming() {
    let (_env, ctx, runner, state) = fixture();
    state.lock().unwrap().empty_types = true;
    assert_eq!(main(&ctx, UP), 1);
    let message = err(&ctx);
    assert!(
        message.contains("v6e-1 unavailable in us-east5-b"),
        "{message}"
    );
    assert!(!runner
        .argvs()
        .iter()
        .any(|a| a.iter().any(|s| s == "create-http-task" || s == "create")));
    assert!(state.lock().unwrap().tasks.is_empty());
}

#[test]
fn orphan_reconciliation_requires_authenticated_absence_and_preserves_other_tasks() {
    let (_env, ctx, _runner, state) = fixture();
    state.lock().unwrap().nodes.push(node(false));
    assert_eq!(main(&ctx, &["expire", ID, "2h"]), 0, "{}", err(&ctx));
    {
        let mut s = state.lock().unwrap();
        let mut unrelated = s.tasks[0].clone();
        unrelated.insert("name", Value::str(format!("{QUEUE}/tasks/manual")));
        s.tasks.push(unrelated);
        s.nodes.clear();
        s.fail_presence = true;
    }
    assert_eq!(main(&ctx, &["ls", "--provider", "gcp"]), 1);
    assert_eq!(state.lock().unwrap().tasks.len(), 2);
    assert!(err(&ctx).contains("access was denied"));
    ctx.clock.sleep(61.0);
    state.lock().unwrap().fail_presence = false;
    assert_eq!(main(&ctx, &["ls", "--provider", "gcp"]), 0, "{}", err(&ctx));
    let s = state.lock().unwrap();
    assert_eq!(s.tasks.len(), 1);
    assert!(crate::gcp::text(&s.tasks[0], "name")
        .unwrap()
        .ends_with("/manual"));
}

#[test]
fn ambiguous_create_keeps_timer_and_next_command_reconciles_a_verified_orphan() {
    let (_env, ctx, _runner, state) = fixture();
    {
        let mut s = state.lock().unwrap();
        s.fail_create = true;
        s.ambiguous_create = true;
    }
    assert_eq!(main(&ctx, UP), 1);
    assert_eq!(state.lock().unwrap().tasks.len(), 1);
    assert!(err(&ctx).contains("Deletion task remains armed"));
    assert_eq!(main(&ctx, &["ls", "--provider", "gcp"]), 0);
    assert_eq!(
        state.lock().unwrap().tasks.len(),
        1,
        "do not reap a newly armed task"
    );
    ctx.clock.sleep(61.0);
    assert_eq!(main(&ctx, &["ls", "--provider", "gcp"]), 0, "{}", err(&ctx));
    assert!(state.lock().unwrap().tasks.is_empty());
}

#[test]
fn capacity_prices_reuse_the_size_job_without_blocking_input() {
    use crate::picker::{testing::*, Job, Outcome};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (_env, ctx, runner, _) = fixture();
    let (release, held) = std::sync::mpsc::channel();
    let held = Mutex::new(held);
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let rates = Job::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
        held.lock().unwrap().recv().unwrap();
        Ok(Arc::new(super::gcp_prices::prices()))
    });
    let settings = crate::gcp::Settings::load(&dict(CONFIG)).unwrap();
    let selected = crate::gcp_wizard::Selection {
        tpu: Some("v6e-4".into()),
        zone: Some("us-east5-b".into()),
        expiry_at: Some("2026-11-03T02:00:00Z".parse().unwrap()),
        ..Default::default()
    };
    let screen = crate::gcp_wizard::capacity_screen(
        ctx,
        settings,
        dict(CONFIG),
        selected.clone(),
        Some(crate::gcp_wizard::Types {
            initial: Vec::new(),
            catalog: Job::new(|| panic!("capacity must not reload the type catalog")),
            rates: rates.clone(),
            cached: None,
        }),
    );
    let result = drive(screen, move |keys| {
        assert_eq!(
            keys.labels(),
            ["On-demand  -", "Spot       -  (may be interrupted)"]
        );
        keys.press_until(DOWN, |s| {
            s.body
                .iter()
                .any(|l| l.starts_with('›') && l.contains("Spot"))
        });
        assert_eq!(keys.title(), "TPU capacity");
        assert!(keys.footer().contains("1-9 select"));
        release.send(()).unwrap();
        keys.wait(|s| {
            s.labels
                == [
                    "On-demand  $11.000/hr",
                    "Spot       $3.000/hr   (may be interrupted)",
                ]
        });
        assert!(!keys
            .snapshot()
            .body
            .iter()
            .any(|l| l.starts_with('›') && l.contains("On-demand")));
        keys.press(ENTER);
    });
    assert_eq!(
        result,
        Outcome::Done(crate::gcp_wizard::Selection {
            spot: Some(true),
            ..selected
        })
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(runner.calls().is_empty());
}

#[test]
fn failed_or_missing_capacity_prices_still_allow_number_selection() {
    use crate::picker::{testing::*, Job, Outcome};
    for failed in [false, true] {
        let (_env, ctx, runner, _) = fixture();
        let settings = crate::gcp::Settings::load(&dict(CONFIG)).unwrap();
        let selected = crate::gcp_wizard::Selection {
            tpu: Some("v5e-1".into()),
            zone: Some("us-east5-b".into()),
            expiry_at: Some("2026-11-03T02:00:00Z".parse().unwrap()),
            ..Default::default()
        };
        let screen = crate::gcp_wizard::capacity_screen(
            ctx,
            settings,
            dict(CONFIG),
            selected.clone(),
            Some(crate::gcp_wizard::Types {
                initial: Vec::new(),
                catalog: Job::new(|| panic!("no catalog read expected")),
                rates: Job::new(move || {
                    if failed {
                        Err("price unavailable".into())
                    } else {
                        Ok(Arc::new(super::gcp_prices::prices()))
                    }
                }),
                cached: None,
            }),
        );
        let result = drive(screen, move |keys| {
            keys.wait(|s| {
                if failed {
                    s.footer.contains("Ctrl-R retry")
                } else {
                    s.labels[0].contains("$1.000/hr")
                }
            });
            assert!(keys.labels()[1].contains("-"));
            assert!(!keys.labels()[1].contains("/hr"));
            keys.press("2");
        });
        assert_eq!(
            result,
            Outcome::Done(crate::gcp_wizard::Selection {
                spot: Some(true),
                ..selected
            })
        );
        assert!(runner.calls().is_empty());
    }
}

#[test]
fn deadline_labels_use_local_time_without_hiding_a_different_date() {
    let (_env, mut ctx, _, _) = fixture();
    for (timestamp, expected) in [
        ("2026-11-03T02:00:00Z", "2:00 AM"),
        ("2026-11-04T02:00:00Z", "Nov 4 at 2:00 AM"),
        ("2027-01-01T14:58:00Z", "Jan 1 2027 at 2:58 PM"),
    ] {
        assert_eq!(
            crate::gcp::deadline_label(&ctx, timestamp.parse().unwrap()),
            expected
        );
    }
    ctx.env = Arc::new((*ctx.env).clone().with("TZ", "America/Los_Angeles"));
    assert_eq!(
        crate::gcp::deadline_label(&ctx, "2026-11-03T02:00:00Z".parse().unwrap()),
        "6:00 PM"
    );
}

#[test]
fn launch_confirmation_quotes_cached_catalog_prices_for_both_capacity_modes() {
    for spot in [false, true] {
        let (env, ctx, _runner, _state) = fixture();
        super::gcp_prices::cache(&env);
        let mut args = UP.to_vec();
        if spot {
            args.push("--spot");
        }
        assert_eq!(main(&ctx, &args), 0, "{}", err(&ctx));
        let text = out(&ctx);
        let line = text.lines().next().unwrap();
        assert_eq!(line, format!(
            "Launch v6e-1 {} in us-east5-b for {}, deletes at 2:00 AM? (storage and networking charges apply)",
            if spot { "Spot" } else { "on-demand" },
            if spot { "$0.75/hr" } else { "$2.75/hr" }
        ));
        assert!(!text.contains("cached") && !text.contains(';') && !text.contains("USD list"));
    }
}

#[test]
fn successful_tpu_launch_offers_last_select_and_dont_open_editors_like_vast() {
    for mode in ["select", "default", "none"] {
        let (env, _ctx, runner, _state) = fixture();
        env.executable("code", "#!/bin/sh\nexit 0\n");
        if mode == "default" {
            crate::history::record_picks(&env.env(), &[("editor", "code")]).unwrap();
        }
        let ctx = env
            .ctx()
            .runner(&runner)
            .interactive(true)
            .pickers(&[match mode {
                "select" => "12",
                "default" => "1",
                _ => "2",
            }])
            .build();
        let mut args = UP.to_vec();
        args.extend(["--on-demand", "--zone", "us-east5-b"]);
        assert_eq!(main(&ctx, &args), 0, "{mode}: {}", err(&ctx));
        assert_eq!(
            runner.argvs().iter().any(|a| a[0] == "code"),
            mode != "none"
        );
        assert_eq!(
            env.home.join(".ssh/gpu_gcp_config").exists(),
            mode != "none"
        );
    }
}

#[test]
fn identifiers_and_ssh_stanzas_reject_injection() {
    for id in [
        "gcp:p:z:../n",
        "gcp:p:z:n\nHost *",
        "gcp:p:z:-n",
        "gcp:p:z:n:extra",
    ] {
        assert!(TpuId::parse(id).is_err());
    }
    let id = TpuId::parse(ID).unwrap();
    for command in [
        "ssh -o ProxyCommand=evil user@192.0.2.1",
        "ssh -i 'key\nHost *' user@192.0.2.1",
        "sh -c evil",
    ] {
        assert!(crate::gcp_ssh::stanza(&id, command).is_err());
    }
}

#[test]
fn list_uses_constant_calls_independent_of_location_count() {
    for zones in [1, 130] {
        let (env, _, _, _) = fixture();
        let runner = FakeRunner::new(move |cmd| {
            if cmd.program() == "gcloud" {
                assert_eq!(cmd.argv[1], "auth");
                return ok("test-token");
            }
            super::gcp_transport::curl(cmd, |url| {
                if url.contains("/locations?") {
                    panic!("must not enumerate {zones} zones for ls");
                }
                assert!(
                    url.contains("/locations/-/nodes?")
                        || url.contains("/queues/gpu-expiry/tasks?")
                );
                (200, "{}".into())
            })
            .unwrap()
        });
        let ctx = env.ctx().runner(&runner).build();
        assert_eq!(main(&ctx, &["ls", "--provider", "gcp"]), 0, "{}", err(&ctx));
        assert_eq!(
            runner.calls().len(),
            3,
            "one token + concurrent node/task reads"
        );
    }
}
#[test]
fn incomplete_inventory_never_proves_absence() {
    for payload in [
        r#"{"nodes":[],"unreachable":["us-east5-b"]}"#,
        r#"{"nodes":[],"nextPageToken":5}"#,
        r#"{"acceleratorTypes":[]}"#,
    ] {
        let (env, _, _, _) = fixture();
        let runner = FakeRunner::new(move |cmd| {
            if cmd.program() == "gcloud" {
                assert_eq!(cmd.argv[1], "auth");
                return ok("test-token");
            }
            super::gcp_transport::curl(cmd, |_| (200, payload.into())).unwrap()
        });
        let ctx = env.ctx().runner(&runner).build();
        assert!(crate::gcp::all_nodes(&ctx, "test-project").is_err());
        assert!(!runner
            .argvs()
            .iter()
            .any(|a| a.iter().any(|s| s == "delete")));
    }
}

fn retry_flags() -> crate::cli::UpFlags {
    let mut args = UP.to_vec();
    args.extend(["--zone", "us-east5-b", "--spot"]);
    let crate::cli::Parsed::Run(command) = crate::cli::parse(&argv(&args)) else {
        panic!("parse")
    };
    let crate::cli::Command::Up(flags) = *command else {
        panic!("up")
    };
    flags
}
fn restrict_retry_zones(ctx: &Ctx) {
    let mut zones = crate::gcp_facts::known_zones(ctx, "test-project");
    for zone in &mut zones {
        zone.types.clear();
        if zone.name == "us-east5-b" {
            zone.types.push("v6e-1".into());
        }
    }
    zones.push(crate::gcp_catalog::Zone {
        name: "us-east5-a".into(),
        types: vec!["v6e-1".into()],
    });
    let mut d = Dict::new();
    d.insert(
        "zones",
        Value::List(
            zones
                .into_iter()
                .map(|z| {
                    let mut row = Dict::new();
                    row.insert("name", Value::str(z.name));
                    row.insert(
                        "types",
                        Value::List(z.types.into_iter().map(Value::str).collect()),
                    );
                    Value::Dict(row)
                })
                .collect(),
        ),
    );
    crate::gcp_http::write_cache(ctx, "tpu-catalog-test-project.json", d);
}
#[test]
fn spot_stockout_names_a_different_zone_and_preserves_request_scope() {
    let (env, ctx, _runner, state) = fixture();
    restrict_retry_zones(&ctx);
    super::gcp_prices::cache(&env);
    state.lock().unwrap().fail_create = true;
    let mut args = UP.to_vec();
    args.push("--spot");
    assert_eq!(main(&ctx, &args), 10);
    let text = err(&ctx);
    assert!(
        text.contains("No Spot capacity in us-east5-b. No node created. Deletion task removed."),
        "{text}"
    );
    assert!(!text.contains("or Spot"));
    assert!(text.contains("Other v6e-1 zones: us-east5-a"));
    let next = text.split("Next:").nth(1).unwrap();
    assert!(next.contains("--zone us-east5-a --spot"));
    assert!(!next.contains("--zone us-east5-b"));
    assert!(next.contains("--runtime v2-alpha-tpuv6e"));
    assert!(next.contains("--expire 2026-11-03T02:00:00Z"));
    assert!(state.lock().unwrap().tasks.is_empty());
}
#[test]
fn interactive_capacity_retry_is_one_key_and_keeps_original_deadline() {
    use crate::picker::{testing::*, Outcome};
    let (env, _ctx, runner, state) = fixture();
    let ctx = env
        .ctx()
        .runner(&runner)
        .clock(&FakeClock::at("2026-11-03T00:00:00Z".parse().unwrap()))
        .interactive(true)
        .pickers(&["2"])
        .build();
    restrict_retry_zones(&ctx);
    super::gcp_prices::cache(&env);
    state.lock().unwrap().reject_creates = 1;
    let cfg = crate::config::load_config(&ctx.env).unwrap();
    let flags = retry_flags();
    let clock = ctx.clock.clone();
    let watched = state.clone();
    let result = crate::gcp::launch_with_retry(
        &ctx,
        &flags,
        &cfg,
        Some("2026-11-03T02:00:00Z".parse().unwrap()),
        move |screen| {
            assert!(
                watched.lock().unwrap().tasks.is_empty(),
                "remove rejected attempt's timer before offering another paid create"
            );
            let clock = clock.clone();
            match drive(screen, move |keys| {
                keys.wait(|s| s.labels.iter().any(|l| l.contains("us-east5-a  $0.75/hr")));
                assert!(keys.snapshot().notice.contains("Deletes at 2:00 AM"));
                assert!(!keys.labels().iter().any(|s| s.contains("in us-east5-b")));
                clock.sleep(30.0);
                keys.press("1");
            }) {
                Outcome::Done(choice) => Ok(choice),
                other => panic!("{other:?}"),
            }
        },
    );
    assert!(result.is_ok(), "{}", err(&ctx));
    let s = state.lock().unwrap();
    assert_eq!(s.nodes.len(), 1);
    assert!(crate::gcp::text(&s.nodes[0], "name")
        .unwrap()
        .contains("/us-east5-a/"));
    assert_eq!(s.tasks.len(), 1);
    assert_eq!(
        crate::gcp::text(&s.tasks[0], "scheduleTime"),
        Some("2026-11-03T02:00:00Z")
    );
    let creates = runner
        .calls()
        .into_iter()
        .filter(|c| c.argv.get(4).is_some_and(|a| a == "create"))
        .collect::<Vec<_>>();
    assert_eq!(creates.len(), 2);
    assert!(creates.iter().all(|c| c.argv.contains(&"--spot".into())));
    assert_eq!(ctx.ui.remaining(), 0);
}
#[test]
fn ambiguous_create_never_offers_capacity_retry_or_disarms_timer() {
    let (env, _ctx, runner, state) = fixture();
    let ctx = env
        .ctx()
        .runner(&runner)
        .clock(&FakeClock::at("2026-11-03T00:00:00Z".parse().unwrap()))
        .interactive(true)
        .build();
    {
        let mut s = state.lock().unwrap();
        s.fail_create = true;
        s.ambiguous_create = true;
    }
    let cfg = crate::config::load_config(&ctx.env).unwrap();
    assert!(crate::gcp::launch_with_retry(
        &ctx,
        &retry_flags(),
        &cfg,
        Some("2026-11-03T02:00:00Z".parse().unwrap()),
        |_| panic!("unverified create cannot offer another paid launch")
    )
    .is_err());
    assert_eq!(state.lock().unwrap().tasks.len(), 1);
}
#[test]
fn retry_can_return_to_grouped_zone_picker_without_authorizing_payment() {
    use crate::picker::{testing::*, Outcome};
    let (env, ctx, _runner, _state) = fixture();
    restrict_retry_zones(&ctx);
    super::gcp_prices::cache(&env);
    let picks = crate::gcp_wizard::Picks {
        zone: "us-east5-b".into(),
        tpu: "v6e-1".into(),
        spot: true,
        runtime: "v2-alpha-tpuv6e".into(),
        repo: crate::repos::Repo::NoRepo,
        directory: "/workspace".into(),
        deadline: "2026-11-03T02:00:00Z".parse().unwrap(),
    };
    let settings = crate::gcp::Settings::load(&dict(CONFIG)).unwrap();
    let skipped = ["us-east5-b".into()].into_iter().collect();
    let result = drive(
        crate::gcp_retry::screen(&ctx, &settings, &dict(CONFIG), &picks, &skipped),
        |keys| {
            keys.wait(|s| s.labels.iter().any(|l| l.starts_with("Retry")));
            keys.press_until("2", on("TPU zone"));
            assert!(keys.labels().iter().any(|s| s.starts_with("us-east ")));
            keys.type_text("us-east5-a");
            keys.press(RIGHT);
        },
    );
    assert_eq!(
        result,
        Outcome::Done(Some(crate::gcp_retry::Choice {
            zone: "us-east5-a".into(),
            approved: false,
            rate: None
        }))
    );
}

#[test]
fn cleanup_failure_never_offers_retry_and_keeps_expiry() {
    let (env, _ctx, runner, state) = fixture();
    let ctx = env
        .ctx()
        .runner(&runner)
        .clock(&FakeClock::at("2026-11-03T00:00:00Z".parse().unwrap()))
        .interactive(true)
        .build();
    {
        let mut s = state.lock().unwrap();
        s.fail_create = true;
        s.fail_delete_task = true;
    }
    let cfg = crate::config::load_config(&ctx.env).unwrap();
    assert!(crate::gcp::launch_with_retry(
        &ctx,
        &retry_flags(),
        &cfg,
        Some("2026-11-03T02:00:00Z".parse().unwrap()),
        |_| panic!("timer cleanup was not verified")
    )
    .is_err());
    assert_eq!(state.lock().unwrap().tasks.len(), 1);
}
#[test]
fn retry_does_not_extend_an_expired_deadline_or_create_again() {
    use crate::picker::{testing::*, Outcome};
    let (env, _ctx, runner, state) = fixture();
    let ctx = env
        .ctx()
        .runner(&runner)
        .clock(&FakeClock::at("2026-11-03T00:00:00Z".parse().unwrap()))
        .interactive(true)
        .build();
    restrict_retry_zones(&ctx);
    super::gcp_prices::cache(&env);
    state.lock().unwrap().reject_creates = 1;
    let cfg = crate::config::load_config(&ctx.env).unwrap();
    let clock = ctx.clock.clone();
    let result = crate::gcp::launch_with_retry(
        &ctx,
        &retry_flags(),
        &cfg,
        Some("2026-11-03T02:00:00Z".parse().unwrap()),
        move |screen| {
            let clock = clock.clone();
            match drive(screen, move |keys| {
                keys.wait(|s| s.labels.iter().any(|l| l.starts_with("Retry")));
                clock.sleep(7201.0);
                keys.press("1");
            }) {
                Outcome::Done(choice) => Ok(choice),
                other => panic!("{other:?}"),
            }
        },
    );
    assert!(result.is_err());
    assert!(state.lock().unwrap().tasks.is_empty());
    assert!(state.lock().unwrap().nodes.is_empty());
    assert_eq!(
        runner
            .calls()
            .iter()
            .filter(|c| c.argv.get(4).is_some_and(|s| s == "create"))
            .count(),
        1
    );
}
#[test]
fn changed_retry_estimate_requires_a_new_confirmation() {
    let (env, _ctx, runner, state) = fixture();
    let ctx = env
        .ctx()
        .runner(&runner)
        .clock(&FakeClock::at("2026-11-03T00:00:00Z".parse().unwrap()))
        .interactive(true)
        .pickers(&["no\r"])
        .build();
    restrict_retry_zones(&ctx);
    super::gcp_prices::cache(&env);
    state.lock().unwrap().reject_creates = 1;
    let cfg = crate::config::load_config(&ctx.env).unwrap();
    crate::gcp::launch_with_retry(
        &ctx,
        &retry_flags(),
        &cfg,
        Some("2026-11-03T02:00:00Z".parse().unwrap()),
        |_| {
            Ok(Some(crate::gcp_retry::Choice {
                zone: "us-east5-a".into(),
                approved: true,
                rate: Some(999.0),
            }))
        },
    )
    .unwrap();
    assert_eq!(
        ctx.ui.remaining(),
        0,
        "changed price must require a new approval"
    );
    assert!(state.lock().unwrap().tasks.is_empty());
    assert_eq!(
        runner
            .calls()
            .iter()
            .filter(|c| c.argv.get(4).is_some_and(|s| s == "create"))
            .count(),
        1
    );
}

#[test]
fn remembered_zone_is_project_scoped_and_does_not_replace_config() {
    let (env, ctx, _runner, _state) = fixture();
    let mut args = UP.to_vec();
    args.extend(["--zone", "us-east5-a"]);
    assert_eq!(main(&ctx, &args), 0, "{}", err(&ctx));
    assert_eq!(
        crate::gcp_preferences::last(&ctx, "test-project"),
        Some("us-east5-a".into())
    );
    assert_eq!(
        crate::gcp_preferences::generation(&ctx, "test-project"),
        Some("v6e".into())
    );
    assert_eq!(
        crate::gcp_preferences::accelerator(&ctx, "test-project", "v6e"),
        Some("v6e-1".into())
    );
    assert_eq!(crate::gcp_preferences::last(&ctx, "other-project"), None);
    let cfg = dict(&read(&env.config.join("gpu/config.json")));
    assert_eq!(
        crate::gcp::text(cfg.get("gcp").unwrap().as_dict().unwrap(), "zone"),
        Some("us-east5-b")
    );
}
#[test]
fn type_filter_counts_and_prices_exclude_unsupported_remembered_zone() {
    use crate::picker::{testing::*, Outcome};
    let (env, ctx, _runner, _state) = fixture();
    restrict_retry_zones(&ctx);
    super::gcp_prices::cache(&env);
    let mut cache =
        crate::gcp_http::read_cache(&ctx, "tpu-catalog-test-project.json", 900).unwrap();
    let Value::List(mut zones) = cache.get("zones").unwrap().clone() else {
        panic!("zones")
    };
    for zone in &mut zones {
        let Value::Dict(d) = zone else { panic!("zone") };
        if crate::gcp::text(d, "name") == Some("us-central1-a") {
            d.insert("types", json(r#"["v5litepod-1"]"#));
        }
    }
    cache.insert("zones", Value::List(zones));
    crate::gcp_http::write_cache(&ctx, "tpu-catalog-test-project.json", cache);
    crate::gcp_preferences::remember(&ctx, "test-project", "us-central1-a");
    let settings = crate::gcp::Settings::load(&dict(CONFIG)).unwrap();
    let result = drive(
        crate::gcp_wizard::priced_zone_screen(
            &ctx,
            &settings,
            &dict(CONFIG),
            "v6e-1",
            true,
            &Default::default(),
            crate::picker::Next::Done,
        ),
        |keys| {
            keys.wait(|s| s.labels == ["us-east  2  zones"]);
            keys.press_until(RIGHT, on("us-east zones"));
            keys.wait(|s| s.labels.iter().all(|l| l.contains("$0.750/hr")));
            assert_eq!(keys.labels().len(), 2);
            assert!(!keys
                .labels()
                .iter()
                .any(|s| s.contains("central") || s.contains('\u{2014}')));
            keys.press(RIGHT);
        },
    );
    assert!(
        matches!(result,Outcome::Done(ref zone) if ["us-east5-a","us-east5-b"].contains(&zone.as_str()))
    );
}

#[test]
fn full_type_cli_alias_is_offline_and_preserves_canonical_accelerator_name() {
    let (_env, ctx, runner, _state) = fixture();
    assert_eq!(
        main(
            &ctx,
            &[
                "up",
                "--type",
                "v5e-4",
                "--zone",
                "us-east5-b",
                "--no-repo",
                "--spot",
                "--expire",
                "2h",
                "--dry-run"
            ]
        ),
        0,
        "{}",
        err(&ctx)
    );
    assert!(out(&ctx).contains("--accelerator-type=v5litepod-4"));
    assert!(runner.calls().is_empty());
}
#[test]
fn generation_and_size_preferences_are_scoped_and_highlight_without_reordering() {
    use crate::picker::{testing::*, Next, Outcome};
    let (_env, ctx, _runner, _state) = fixture();
    crate::gcp_preferences::remember_type(&ctx, "test-project", "v5p-16");
    assert_eq!(
        crate::gcp_preferences::generation(&ctx, "other-project"),
        None
    );
    let settings = crate::gcp::Settings::load(&dict(CONFIG)).unwrap();
    let Next::Screen(first) = crate::gcp_wizard::next(ctx, settings, Default::default()) else {
        panic!("generation")
    };
    let result = drive(first, |keys| {
        keys.wait(on("TPU generation"));
        assert!(keys.labels().len() <= 7);
        assert!(keys
            .snapshot()
            .body
            .iter()
            .any(|s| s.starts_with('›') && s.contains("v5p")));
        keys.press_until(RIGHT, on("v5p size"));
        assert!(keys.labels()[0].starts_with("8 TensorCores (4 chips)"));
        assert!(keys
            .snapshot()
            .body
            .iter()
            .any(|s| s.starts_with('›') && s.contains("16 TensorCores (8 chips)")));
        assert!(!keys.labels().iter().any(|s| s.contains("pod")));
        keys.press_until(BACK, on("TPU generation"));
        keys.press(CTRL_C);
    });
    assert_eq!(result, Outcome::Cancelled(130));
}
#[test]
fn mapped_cloud_completion_still_waits_for_cached_repository_validation() {
    use crate::picker::{testing::*, Job, Next, Outcome};
    let (env, ctx, _runner, _state) = fixture();
    crate::repos::save_cached_repos(&ctx.env, "owner", &["demo".into()]).unwrap();
    let gate = super::picker::Gate::default();
    let blocked = gate.clone();
    let cfg = crate::config::load_config(&ctx.env).unwrap();
    let wizard = crate::wizard::LaunchWizard::new(
        &env.env(),
        crate::wizard::WizardOptions {
            repos: Job::new(move || {
                assert!(blocked.wait());
                Ok(vec!["demo".into()])
            }),
            search: None,
            models: Vec::new(),
            recent_gpus: Vec::new(),
            choose_offer: false,
            tpu_setup_error: None,
            tpu_transition: Some(crate::gcp_wizard::transition(
                &ctx,
                &crate::launch::UpArgs {
                    num_gpus: 1,
                    ..Default::default()
                },
                &cfg,
                Some("2026-11-03T02:00:00Z".parse().unwrap()),
            )),
            tpu_prices: None,
            tpu_cached_prices: None,
            tpu_types: crate::gcp_facts::known_types(&ctx, "test-project", None),
            tpu_catalog: None,
            owner: "owner".into(),
        },
    );
    let Next::Screen(first) = wizard.next(crate::wizard::Picks::new(None, Some("container"), None))
    else {
        panic!("repo")
    };
    let result = drive(first, |keys| {
        keys.wait(|s| s.labels.iter().any(|l| l.contains("demo")));
        keys.press_until(RIGHT, on("GPU"));
        keys.press_until(RIGHT, on("TPU generation"));
        keys.type_text("v6e");
        keys.press_until(RIGHT, on("v6e size"));
        keys.press_until(RIGHT, on("TPU capacity"));
        keys.press_until(RIGHT, on("TPU zone"));
        keys.type_text("us-east5-b");
        keys.press_until(RIGHT, on("Checking repositories"));
        keys.press(CTRL_C);
    });
    gate.set();
    assert_eq!(result, Outcome::Cancelled(130));
}
