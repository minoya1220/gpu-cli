//! No test in this module can invoke a real cloud command.
use crate::ctx::Ctx;
use crate::gcp::TpuId;
use crate::process::{Cmd, Output};
use crate::pyjson::{self, Dict, Value};
use crate::testing::*;
use std::sync::{Arc, Mutex};

const CONFIG: &str = r#"{"gcp":{"project":"test-project","zone":"us-east5-b","runtime":"v2-alpha-tpuv6e","expiry":{"queue":"gpu-expiry","location":"us-central1","service_account":"reaper@test-project.iam.gserviceaccount.com"}}}"#;
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
    fail_arm: bool,
    fail_delete_task: bool,
    corrupt_task: bool,
    paused: bool,
    finite_retries: bool,
    vast_fail: bool,
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
    let runner = FakeRunner::new(move |cmd| {
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
        if a[0] == "code" {
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
            "accelerator-types" => ok(r#"[{"type":"v6e-1"}]"#),
            "versions" => ok(r#"[{"version":"v2-alpha-tpuv6e"}]"#),
            "list" => ok(&list(if arg(cmd,"zone") == "us-east5-b" {state.nodes.clone()} else {Vec::new()})),
            "describe" => ok(&value(state.nodes.first().unwrap().clone())),
            "create" => {
                assert!(!state.tasks.is_empty(), "create without a deletion task");
                if state.fail_create { return Ok(Output::failed(1, "RESOURCE_EXHAUSTED capacity stockout token=do-not-print")); }
                let mut n = node(a.contains(&"--spot"));
                n.insert("name", Value::str(format!("projects/test-project/locations/us-east5-b/nodes/{}", a[5])));
                state.nodes.push(n);
                ok("{}")
            },
            "stop" => {
                state.nodes[0].insert("state", Value::str("STOPPED"));
                ok("{}")
            },
            "delete" => { state.nodes.clear(); ok("{}") },
            "ssh" if a.contains(&"--dry-run") => ok("/usr/bin/ssh -t -i /tmp/test-key -o CheckHostIP=no -o HostKeyAlias=compute.123 -o UserKnownHostsFile=/tmp/known_hosts developer@192.0.2.10"),
            "ssh" => { assert!(a.contains(&"--command=true")); ok("") },
            _ => panic!("unexpected cloud request: {a:?}"),
        }
    });
    let ctx = env
        .ctx()
        .runner(&runner)
        .clock(&FakeClock::at("2026-11-03T00:00:00Z".parse().unwrap()))
        .build();
    (env, ctx, runner, state)
}

#[test]
fn launch_arms_and_verifies_before_create_and_never_uses_vast_or_ssh() {
    let (_env, ctx, runner, state) = fixture();
    let mut flags = UP.to_vec();
    flags.push("--spot");
    assert_eq!(main(&ctx, &flags), 0, "{}", err(&ctx));
    let calls = runner.argvs();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0][1..4], ["tasks", "queues", "describe"]);
    assert_eq!(calls[1][2], "create-http-task");
    assert_eq!(calls[2][2], "describe");
    assert_eq!(calls[3][1..5], ["compute", "tpus", "tpu-vm", "create"]);
    assert!(calls[3].contains(&"--accelerator-type=v6e-1".into()));
    assert!(calls[3].contains(&"--spot".into()));
    assert!(!calls[3].iter().any(|a| a.contains("reaper@")));
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
        1
    );
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
fn stockout_keeps_safety_task_and_reports_one_next_command() {
    let (_env, ctx, _runner, state) = fixture();
    state.lock().unwrap().fail_create = true;
    assert_eq!(main(&ctx, UP), 1);
    let message = err(&ctx);
    assert!(message.contains("try another zone or Spot"), "{message}");
    assert_eq!(message.matches("Next:").count(), 1);
    assert!(!message.contains("do-not-print"));
    assert_eq!(state.lock().unwrap().tasks.len(), 1);
}

#[test]
fn lists_are_provider_labelled_and_gcp_only_does_not_require_vast() {
    let (_env, ctx, runner, state) = fixture();
    state.lock().unwrap().nodes.push(node(false));
    assert_eq!(main(&ctx, &["ls"]), 0, "{}", err(&ctx));
    let printed = out(&ctx);
    assert!(printed.contains("Vast.ai GPUs"));
    assert!(printed.contains("Google Cloud TPUs"));
    assert!(printed.contains(ID));
    assert!(!printed.contains("not-for-output"));
    let start = runner.calls().len();
    assert_eq!(main(&ctx, &["ls", "--json", "--provider", "gcp"]), 0);
    assert!(runner.argvs()[start..].iter().all(|a| a[0] == "gcloud"));
    assert!(out(&ctx).contains("\"provider\": \"gcp\""));
}

#[test]
fn unavailable_vast_does_not_hide_tpus_but_listing_is_nonzero() {
    let (_env, ctx, _runner, state) = fixture();
    state.lock().unwrap().vast_fail = true;
    state.lock().unwrap().nodes.push(node(false));
    assert_eq!(main(&ctx, &["ls"]), 1);
    assert!(out(&ctx).contains(ID));
    assert!(err(&ctx).contains("gpu ls --provider vast"));
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
    assert_eq!(calls[0][4], "list");
    assert_eq!(calls[1][4], "delete");
    assert_eq!(calls[2][4], "list");
    assert_eq!(calls[3][1], "tasks");
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
        .all(|a| a[0] == "gcloud" && !a.contains(&"ssh".into())));
    assert_eq!(main(&ctx, &["expire", ID, "show"]), 1);
    assert!(err(&ctx).contains("NOT armed"));
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
    state.lock().unwrap().fail_create = true;
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
        ("versions", "TPU runtime image", "v2-alpha-tpuv6e"),
    ] {
        let gate = super::picker::Gate::default();
        let worker_gate = gate.clone();
        let inner = runner.clone();
        let gated_runner = FakeRunner::new(move |cmd| {
            assert!(worker_gate.wait(), "test never released catalog loader");
            inner.run(cmd)
        });
        let ctx = env.ctx().runner(&gated_runner).build();
        let job = crate::gcp_wizard::catalog_job(&ctx, &settings, Some("us-east5-b"), kind);
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
                keys.press(ENTER);
            },
        );
        gate.set();
        assert_eq!(result, Outcome::Done(expected.to_string()));
    }
    assert_eq!(runner.calls().len(), 3);
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
        keys.wait(|s| s.title == "TPU zone" && !s.labels.is_empty());
        keys.press_until(ENTER, on("TPU type"));
        keys.wait(|s| s.labels.contains(&"v6e-1".to_string()));
        keys.press_until(ENTER, on("TPU runtime image"));
        keys.wait(|s| !s.labels.is_empty());
        keys.press_until(BACK, on("TPU type"));
        keys.press_until(BACK, on("TPU zone"));
        assert_eq!(keys.labels(), ["us-east5-b", "us-central1-a"]);
        keys.press(CTRL_C);
    });
    assert_eq!(result, Outcome::Cancelled(130));
    assert_eq!(runner.calls().len(), 3);
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
fn declining_creation_or_destruction_never_calls_a_provider() {
    let (env, _ctx, runner, _state) = fixture();
    let ctx = env.ctx().runner(&runner).input(&["no", "no"]).build();
    let flags: Vec<_> = UP.iter().copied().filter(|s| *s != "--yes").collect();
    assert_eq!(main(&ctx, &flags), 0);
    assert_eq!(main(&ctx, &["down", ID]), 0);
    assert!(runner.calls().is_empty());
}

#[test]
fn malformed_node_list_never_verifies_destruction() {
    let env = TestEnv::new();
    let runner = FakeRunner::new(|cmd| {
        assert_eq!(cmd.argv[4], "list");
        ok(r#"[{"state":"READY"}]"#)
    });
    let ctx = env.ctx().runner(&runner).build();
    assert_eq!(main(&ctx, &["down", ID, "--yes"]), 1);
    assert!(err(&ctx).contains("incomplete reply"));
    assert_eq!(runner.calls().len(), 1);
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
