//! Setup/undo and opt-in safety tests: all cloud requests terminate here.
#[path = "setup_automation.rs"]
mod automation;
use std::sync::{Arc, Mutex};

use crate::ctx::Ctx;
use crate::process::{Cmd, Output};
use crate::pyjson::{self, Dict, Value};
use crate::testing::*;

const PROJECT: &str = "test-project";
const ID: &str = "gcp:test-project:us-east5-b:test-tpu";
const RESOURCE: &str = "projects/test-project/locations/us-central1/queues/gpu-expiry";
const ACCOUNT: &str = "123456-compute@developer.gserviceaccount.com";
const APIS: [&str; 4] = [
    "compute.googleapis.com",
    "tpu.googleapis.com",
    "cloudtasks.googleapis.com",
    "cloudbilling.googleapis.com",
];
const APPLY: &[&str] = &[
    "setup",
    "--provider",
    "gcp",
    "--project",
    PROJECT,
    "--confirm-project",
    PROJECT,
];
const REMOVE: &[&str] = &[
    "setup",
    "--provider",
    "gcp",
    "--remove",
    "--project",
    PROJECT,
    "--confirm-project",
    PROJECT,
];

struct State {
    apis: Vec<String>,
    queue: bool,
    incompatible: bool,
    identity: bool,
    grant: bool,
    tasks: bool,
    owned_tpu: bool,
    fail_enable: bool,
    fail_create: bool,
    fail_disable: bool,
    fail_delete: bool,
}
impl Default for State {
    fn default() -> Self {
        Self {
            apis: APIS.iter().map(|s| s.to_string()).collect(),
            queue: false,
            incompatible: false,
            identity: true,
            grant: true,
            tasks: false,
            owned_tpu: false,
            fail_enable: false,
            fail_create: false,
            fail_disable: false,
            fail_delete: false,
        }
    }
}
fn dump(value: Value) -> String {
    pyjson::dumps(&value, None)
}
fn names(names: &[String]) -> String {
    dump(Value::List(
        names
            .iter()
            .map(|name| {
                let mut config = Dict::new();
                config.insert("name", Value::str(name));
                let mut item = Dict::new();
                item.insert("config", Value::Dict(config));
                Value::Dict(item)
            })
            .collect(),
    ))
}
fn fixture() -> (TestEnv, Ctx, Arc<FakeRunner>, Arc<Mutex<State>>) {
    let env = TestEnv::new();
    let state = Arc::new(Mutex::new(State::default()));
    let shared = state.clone();
    let config = env.config.join("gpu/config.json");
    let native = move |cmd: &Cmd| {
        let a = strs(&cmd.argv);
        if a == ["vastai", "show", "instances", "--raw"] {
            return ok("[]");
        }
        assert_eq!(a[0], "gcloud", "{a:?}");
        assert!(a.contains(&"--quiet"), "{a:?}");
        let parts: Vec<_> = a[1..]
            .iter()
            .copied()
            .filter(|s| !s.starts_with("--project=") && *s != "--quiet" && *s != "--format=json")
            .collect();
        if parts != ["projects", "list", "--filter=lifecycleState:ACTIVE"] {
            assert!(
                a.contains(&"--project=test-project"),
                "unscoped cloud request: {a:?}"
            );
        }
        let mut s = shared.lock().unwrap();
        match parts.as_slice() {
            ["projects", "list", "--filter=lifecycleState:ACTIVE"] => {
                ok(r#"[{"projectId":"test-project","projectNumber":"123456","name":"sandbox"}]"#)
            }
            ["services", "list", "--enabled"] => ok(&names(&s.apis)),
            ["services", "enable", api] if APIS.contains(api) => {
                let saved = dict(&read(&config));
                let journal = saved
                    .get("gcp")
                    .unwrap()
                    .as_dict()
                    .unwrap()
                    .get("setup")
                    .unwrap()
                    .as_dict()
                    .unwrap();
                assert!(
                    matches!(journal.get("enabled_apis"), Some(Value::List(apis)) if apis.contains(&Value::str(*api))),
                    "mutation was not journaled first"
                );
                // Even a failed request may have committed on the server.
                if !s.apis.contains(&api.to_string()) {
                    s.apis.push(api.to_string());
                }
                if s.fail_enable {
                    Ok(Output::failed(1, "secret failure"))
                } else {
                    ok("")
                }
            }
            ["iam", "service-accounts", "describe", ACCOUNT] => {
                if s.identity {
                    ok(&format!(r#"{{"email":"{ACCOUNT}"}}"#))
                } else {
                    Ok(Output::failed(1, "not found"))
                }
            }
            ["projects", "get-iam-policy", PROJECT] => {
                if s.grant {
                    ok(&format!(
                        r#"{{"bindings":[{{"role":"roles/editor","members":["serviceAccount:{ACCOUNT}"]}}]}}"#
                    ))
                } else {
                    ok(r#"{"bindings":[]}"#)
                }
            }
            ["tasks", "queues", "list", "--location=us-central1"] => {
                if s.queue {
                    ok(&format!(r#"[{{"name":"{RESOURCE}"}}]"#))
                } else {
                    ok("[]")
                }
            }
            ["tasks", "queues", "describe", "gpu-expiry", "--location=us-central1"] => {
                if s.incompatible {
                    ok(r#"{"state":"PAUSED"}"#)
                } else {
                    ok(r#"{"state":"RUNNING","retryConfig":{"maxAttempts":-1,"maxBackoff":"60s"}}"#)
                }
            }
            ["tasks", "queues", "create", "gpu-expiry", "--location=us-central1", "--max-attempts=-1", "--max-retry-duration=0s", "--min-backoff=5s", "--max-backoff=60s"] =>
            {
                assert!(!s.queue, "duplicate create");
                assert!(
                    dict(&read(&config))
                        .get("gcp")
                        .unwrap()
                        .as_dict()
                        .unwrap()
                        .get("setup")
                        .unwrap()
                        .as_dict()
                        .unwrap()
                        .get("created_queue")
                        == Some(&Value::Bool(true))
                );
                s.queue = true;
                if s.fail_create {
                    Ok(Output::failed(1, "secret timeout"))
                } else {
                    ok("")
                }
            }
            ["compute", "tpus", "tpu-vm", "accelerator-types", "list", _zone] => {
                ok(r#"[{"type":"v6e-1"}]"#)
            }
            ["compute", "tpus", "locations", "list"] => {
                ok(r#"[{"locationId":"us-east5-b"},{"locationId":"us-central1-a"}]"#)
            }
            ["compute", "tpus", "tpu-vm", "list", zone]
                if ["--zone=us-east5-b", "--zone=us-central1-a", "--zone=-"].contains(zone) =>
            {
                // Outside default zone: undo must not overlook it.
                if s.owned_tpu && ["--zone=us-central1-a", "--zone=-"].contains(zone) {
                    ok(
                        r#"[{"name":"projects/test-project/locations/us-central1-a/nodes/test-tpu","labels":{"gpu-cli":"true"}}]"#,
                    )
                } else {
                    ok("[]")
                }
            }
            ["tasks", "list", "--location=us-central1", "--queue=gpu-expiry"] => {
                if s.tasks {
                    ok(r#"[{"name":"pending-task"}]"#)
                } else {
                    ok("[]")
                }
            }
            ["tasks", "queues", "delete", "gpu-expiry", "--location=us-central1"] => {
                assert!(!s.tasks && !s.owned_tpu);
                s.queue = false;
                if s.fail_delete {
                    Ok(Output::failed(1, "timeout"))
                } else {
                    ok("")
                }
            }
            ["services", "disable", api] if APIS.contains(api) => {
                if s.fail_disable {
                    return Ok(Output::failed(1, "dependent services"));
                }
                s.apis.retain(|s| s != api);
                ok("")
            }
            _ => panic!("unexpected cloud operation (no IAM/resource writes allowed): {a:?}"),
        }
    };
    let runner = FakeRunner::new(move |cmd| {
        if cmd.program() == "curl" {
            if let crate::process::Input::Text(config) = &cmd.stdin {
                if config.contains("https://cloudbilling.googleapis.com/v1/services")
                    || cmd
                        .argv
                        .iter()
                        .any(|a| a.starts_with("https://cloudbilling.googleapis.com/v1/services"))
                {
                    return super::gcp_transport::curl(cmd, |url| {
                        if url.contains("/skus?") {
                            (200, r#"{"skus":[{"skuId":"test-sku","description":"TPU v6e chip","category":{"usageType":"OnDemand","resourceGroup":"TPU"},"serviceRegions":["us-east5"]}]}"#.into())
                        } else {
                            (200, r#"{"services":[{"name":"services/test-service","displayName":"Cloud TPU"}]}"#.into())
                        }
                    }).unwrap();
                }
            }
        }
        super::gcp_transport::adapt(cmd, &native).unwrap_or_else(|| native(cmd))
    });
    let ctx = env.ctx().runner(&runner).build();
    (env, ctx, runner, state)
}
fn cfg(env: &TestEnv) -> Dict {
    crate::config::load_config(&env.env()).unwrap()
}
fn journal(env: &TestEnv) -> Dict {
    cfg(env)
        .get("gcp")
        .unwrap()
        .as_dict()
        .unwrap()
        .get("setup")
        .unwrap()
        .as_dict()
        .unwrap()
        .clone()
}
fn mutations(runner: &FakeRunner) -> Vec<Vec<String>> {
    runner
        .argvs()
        .into_iter()
        .filter(|a| {
            a.iter()
                .any(|s| ["enable", "disable", "create", "delete"].contains(&s.as_str()))
        })
        .collect()
}
fn call(parts: &[&str], json: bool) -> Vec<String> {
    let mut a = vec!["gcloud".into()];
    a.extend(parts.iter().map(|s| s.to_string()));
    a.extend(["--project=test-project".into(), "--quiet".into()]);
    if json {
        a.push("--format=json".into());
    }
    a
}

#[test]
fn setup_creates_only_one_queue_preserves_config_and_is_idempotent() {
    let (env, ctx, runner, state) = fixture();
    let path = env.config.join("gpu");
    std::fs::create_dir(&path).unwrap();
    std::fs::write(
        path.join("config.json"),
        r#"{"owner":"me","custom":{"keep":true}}"#,
    )
    .unwrap();
    state
        .lock()
        .unwrap()
        .apis
        .retain(|s| s != "cloudtasks.googleapis.com");
    assert_eq!(main(&ctx, APPLY), 0, "{}", err(&ctx));
    assert!(crate::setup::gcp_ready(&cfg(&env)));
    assert_eq!(cfg(&env).get("owner"), Some(&Value::str("me")));
    assert_eq!(
        cfg(&env).get("custom"),
        Some(&Value::Dict(dict(r#"{"keep":true}"#)))
    );
    assert_eq!(
        journal(&env).get("enabled_apis"),
        Some(&Value::List(vec![Value::str("cloudtasks.googleapis.com")]))
    );
    assert_eq!(journal(&env).get("created_queue"), Some(&Value::Bool(true)));
    assert_eq!(
        mutations(&runner),
        [
            call(&["services", "enable", "cloudtasks.googleapis.com"], false),
            call(
                &[
                    "tasks",
                    "queues",
                    "create",
                    "gpu-expiry",
                    "--location=us-central1",
                    "--max-attempts=-1",
                    "--max-retry-duration=0s",
                    "--min-backoff=5s",
                    "--max-backoff=60s"
                ],
                false
            ),
        ]
    );
    let before = runner.calls().len();
    assert_eq!(main(&ctx, APPLY), 0, "{}", err(&ctx));
    assert_eq!(mutations(&runner).len(), 2);
    assert!(runner.argvs()[before..].contains(
        &[
            "gcloud",
            "tasks",
            "queues",
            "describe",
            "gpu-expiry",
            "--project=test-project",
            "--location=us-central1",
            "--quiet",
            "--format=json"
        ]
        .map(String::from)
        .to_vec()
    ));
    let config = cfg(&env);
    let gcp = config.get("gcp").unwrap().as_dict().unwrap();
    assert_eq!(gcp.get("service_account"), Some(&Value::str(ACCOUNT)));
    assert!(
        gcp.get("runtime").is_none(),
        "runtime is a launch choice, never a saved setup default"
    );
    assert_eq!(
        gcp.get("expiry")
            .unwrap()
            .as_dict()
            .unwrap()
            .get("service_account"),
        Some(&Value::str(ACCOUNT))
    );
    let before = runner.calls().len();
    assert_eq!(
        main(
            &ctx,
            &[
                "up",
                "--tpu",
                "v6e-1",
                "--zone",
                "us-east5-b",
                "--no-repo",
                "--expire",
                "2h",
                "--yes",
                "--dry-run"
            ]
        ),
        0,
        "{}",
        err(&ctx)
    );
    let printed = out(&ctx);
    assert!(printed.contains(&format!("--oauth-service-account-email={ACCOUNT}")));
    assert!(printed.contains(&format!("--service-account={ACCOUNT}")));
    assert_eq!(runner.calls().len(), before);
}

#[test]
fn dry_run_missing_confirmation_and_decline_have_zero_mutations() {
    for mode in ["dry", "missing", "decline"] {
        let (env, ctx, runner, _state) = fixture();
        let (args, ctx) = match mode {
            "dry" => (
                vec![
                    "setup",
                    "--provider",
                    "gcp",
                    "--project",
                    PROJECT,
                    "--dry-run",
                ],
                ctx,
            ),
            "decline" => (
                APPLY[..5].to_vec(),
                env.ctx()
                    .runner(&runner)
                    .interactive(true)
                    .pickers(&["no\r"])
                    .build(),
            ),
            _ => (APPLY[..5].to_vec(), ctx),
        };
        assert_eq!(
            main(&ctx, &args),
            if mode == "missing" { 2 } else { 0 },
            "{}",
            err(&ctx)
        );
        assert!(mutations(&runner).is_empty());
        assert!(!env.config.join("gpu/config.json").exists());
    }
}

#[test]
fn default_identity_without_delete_grant_does_not_create_queue_or_change_iam() {
    let (env, ctx, runner, state) = fixture();
    state.lock().unwrap().grant = false;
    assert_eq!(main(&ctx, APPLY), 1);
    assert!(err(&ctx).contains("tpu.nodes.delete"));
    assert!(!crate::setup::gcp_ready(&cfg(&env)));
    assert!(mutations(&runner).is_empty());
    // The partial journal is undoable even though completion was never recorded.
    assert_eq!(main(&ctx, REMOVE), 0, "{}", err(&ctx));
    assert!(cfg(&env).get("gcp").is_none());
}

#[test]
fn reused_queue_and_preexisting_apis_are_never_removed_or_modified() {
    let (env, ctx, runner, state) = fixture();
    state.lock().unwrap().queue = true;
    assert_eq!(main(&ctx, APPLY), 0, "{}", err(&ctx));
    assert_eq!(
        journal(&env).get("created_queue"),
        Some(&Value::Bool(false))
    );
    assert_eq!(main(&ctx, REMOVE), 0, "{}", err(&ctx));
    assert!(mutations(&runner).is_empty());
    assert!(state.lock().unwrap().queue);
    assert!(cfg(&env).get("gcp").is_none());
    assert!(ctx.require_gcloud().is_err());
}

#[test]
fn incompatible_existing_queue_fails_without_overwriting_it() {
    let (env, ctx, runner, state) = fixture();
    {
        let mut s = state.lock().unwrap();
        s.queue = true;
        s.incompatible = true;
    }
    assert_eq!(main(&ctx, APPLY), 1);
    assert!(mutations(&runner).is_empty());
    assert!(!env.config.join("gpu/config.json").exists());
}

#[test]
fn undo_removes_only_recorded_additions_and_restores_the_gate() {
    let (env, ctx, runner, state) = fixture();
    state
        .lock()
        .unwrap()
        .apis
        .retain(|s| s == "compute.googleapis.com");
    assert_eq!(main(&ctx, APPLY), 0, "{}", err(&ctx));
    let before = mutations(&runner).len();
    assert_eq!(main(&ctx, REMOVE), 0, "{}", err(&ctx));
    assert_eq!(
        &mutations(&runner)[before..],
        [
            call(
                &[
                    "tasks",
                    "queues",
                    "delete",
                    "gpu-expiry",
                    "--location=us-central1"
                ],
                false
            ),
            call(
                &["services", "disable", "cloudbilling.googleapis.com"],
                false
            ),
            call(&["services", "disable", "cloudtasks.googleapis.com"], false),
            call(&["services", "disable", "tpu.googleapis.com"], false),
        ]
    );
    assert_eq!(state.lock().unwrap().apis, ["compute.googleapis.com"]);
    assert!(cfg(&env).get("gcp").is_none());
    let before = runner.calls().len();
    assert_eq!(main(&ctx, &["ls", "--provider", "gcp"]), 12);
    assert_eq!(main(&ctx, REMOVE), 0);
    assert_eq!(runner.calls().len(), before);
}

#[test]
fn undo_refuses_pending_tasks_or_owned_tpus_in_any_zone() {
    for owned_tpu in [false, true] {
        let (_env, ctx, runner, state) = fixture();
        assert_eq!(main(&ctx, APPLY), 0, "{}", err(&ctx));
        {
            let mut s = state.lock().unwrap();
            s.owned_tpu = owned_tpu;
            s.tasks = !owned_tpu;
        }
        let before = mutations(&runner).len();
        assert_eq!(main(&ctx, REMOVE), 1);
        let message = err(&ctx);
        assert!(message.contains("gpu down"), "{message}");
        assert_eq!(mutations(&runner).len(), before);
        assert_eq!(message.matches("Next:").count(), 1);
    }
}

#[test]
fn partial_enable_and_create_are_journaled_and_resumable() {
    for fail_enable in [false, true] {
        let (env, ctx, runner, state) = fixture();
        {
            let mut s = state.lock().unwrap();
            s.apis.clear();
            s.fail_enable = fail_enable;
            s.fail_create = !fail_enable;
        }
        assert_eq!(main(&ctx, APPLY), 1);
        assert!(!err(&ctx).contains("secret"));
        assert!(!crate::setup::gcp_ready(&cfg(&env)));
        {
            let mut s = state.lock().unwrap();
            s.fail_enable = false;
            s.fail_create = false;
        }
        assert_eq!(main(&ctx, APPLY), 0, "{}", err(&ctx));
        assert_eq!(
            journal(&env).get("enabled_apis"),
            Some(&Value::List(APIS.into_iter().map(Value::str).collect()))
        );
        assert_eq!(main(&ctx, REMOVE), 0, "{}", err(&ctx));
        assert!(state.lock().unwrap().apis.is_empty());
        assert!(mutations(&runner)
            .iter()
            .all(|a| !a.contains(&"--force".into())));
    }
}

#[test]
fn partial_removal_retains_journal_and_can_resume() {
    for fail_delete in [false, true] {
        let (env, ctx, _runner, state) = fixture();
        state.lock().unwrap().apis.clear();
        assert_eq!(main(&ctx, APPLY), 0, "{}", err(&ctx));
        {
            let mut s = state.lock().unwrap();
            s.fail_delete = fail_delete;
            s.fail_disable = !fail_delete;
        }
        assert_eq!(main(&ctx, REMOVE), 1);
        assert!(!crate::setup::gcp_ready(&cfg(&env)));
        assert!(cfg(&env).get("gcp").is_some());
        {
            let mut s = state.lock().unwrap();
            s.fail_delete = false;
            s.fail_disable = false;
        }
        assert_eq!(main(&ctx, REMOVE), 0, "{}", err(&ctx));
        assert!(cfg(&env).get("gcp").is_none());
    }
}

#[test]
fn disabled_apis_are_not_treated_as_proof_that_resources_are_absent() {
    for api in ["tpu.googleapis.com", "cloudtasks.googleapis.com"] {
        let (env, ctx, runner, state) = fixture();
        assert_eq!(main(&ctx, APPLY), 0, "{}", err(&ctx));
        state.lock().unwrap().apis.retain(|s| s != api);
        let before = mutations(&runner).len();
        assert_eq!(main(&ctx, REMOVE), 1);
        assert!(err(&ctx).contains("Absence unverified"));
        assert_eq!(mutations(&runner).len(), before);
        assert!(cfg(&env).get("gcp").is_some());
    }
}

#[test]
fn removal_dry_run_and_decline_never_change_cloud_or_config() {
    for decline in [false, true] {
        let (env, ctx, runner, _state) = fixture();
        assert_eq!(main(&ctx, APPLY), 0, "{}", err(&ctx));
        let original = read(&env.config.join("gpu/config.json"));
        let before = mutations(&runner).len();
        let ctx = if decline {
            env.ctx()
                .runner(&runner)
                .interactive(true)
                .pickers(&["no\r"])
                .build()
        } else {
            ctx
        };
        let args = if decline {
            vec!["setup", "--provider", "gcp", "--remove"]
        } else {
            vec!["setup", "--provider", "gcp", "--remove", "--dry-run"]
        };
        assert_eq!(main(&ctx, &args), 0, "{}", err(&ctx));
        assert_eq!(mutations(&runner).len(), before);
        assert_eq!(read(&env.config.join("gpu/config.json")), original);
    }
}

#[test]
fn missing_identity_and_failed_inventory_never_authorize_removal() {
    let (env, ctx, runner, state) = fixture();
    state.lock().unwrap().identity = false;
    assert_eq!(main(&ctx, APPLY), 1);
    assert!(mutations(&runner).is_empty());
    state.lock().unwrap().identity = true;
    assert_eq!(main(&ctx, APPLY), 0, "{}", err(&ctx));
    let inventory_fail = super::gcp_transport::runner(|cmd| {
        assert_eq!(cmd.program(), "gcloud");
        if cmd.argv[1..3] == ["services", "list"] {
            return ok(&names(&APIS.map(String::from)));
        }
        assert_eq!(cmd.argv[1..5], ["compute", "tpus", "tpu-vm", "list"]);
        Ok(Output::failed(1, "permission denied secret"))
    });
    let ctx = env.ctx().runner(&inventory_fail).build();
    assert_eq!(main(&ctx, REMOVE), 1);
    assert!(cfg(&env).get("gcp").is_some());
    assert!(mutations(&inventory_fail).is_empty());
    assert!(!err(&ctx).contains("secret"));
}

#[test]
fn wrong_confirmation_and_project_switches_fail_closed() {
    let (_env, ctx, runner, _state) = fixture();
    assert_eq!(
        main(
            &ctx,
            &[
                "setup",
                "--provider",
                "gcp",
                "--project",
                PROJECT,
                "--confirm-project",
                "other-project"
            ]
        ),
        1
    );
    assert!(runner.calls().is_empty());
    assert_eq!(main(&ctx, APPLY), 0, "{}", err(&ctx));
    let before = runner.calls().len();
    assert_eq!(
        main(
            &ctx,
            &[
                "setup",
                "--provider",
                "gcp",
                "--remove",
                "--project",
                "other-project",
                "--confirm-project",
                "other-project"
            ]
        ),
        1
    );
    assert_eq!(runner.calls().len(), before);
}

#[test]
fn unset_up_cloud_is_blocked_at_the_single_process_boundary_for_all_commands() {
    for config in [
        "{}",
        r#"{"gcp":{"project":"test-project"}}"#,
        r#"{"gcp":{"project":"test-project","setup":{"version":0,"project":"test-project"}}}"#,
        r#"{"gcp":{"project":"other-project","setup":{"version":1,"project":"test-project"}}}"#,
    ] {
        let env = TestEnv::new();
        std::fs::create_dir(env.config.join("gpu")).unwrap();
        std::fs::write(env.config.join("gpu/config.json"), config).unwrap();
        let runner = super::gcp_transport::runner(|cmd| {
            assert_eq!(
                cmd.program(),
                "vastai",
                "gcloud escaped gate: {:?}",
                cmd.argv
            );
            ok("[]")
        });
        let ctx = env.ctx().runner(&runner).build();
        for args in [
            vec![
                "up",
                "--provider",
                "gcp",
                "--tpu",
                "v6e-1",
                "--zone",
                "us-east5-b",
                "--no-repo",
                "--expire",
                "2h",
                "--yes",
            ],
            vec!["ls", "--provider", "gcp"],
            vec!["ssh", ID],
            vec!["ssh", ID, "--exec"],
            vec!["edit", ID],
            vec!["cursor", ID],
            vec!["stop", ID],
            vec!["down", ID, "--yes"],
            vec!["expire", ID, "show"],
            vec!["expire", ID, "cancel"],
            vec!["expire", ID, "2h"],
        ] {
            assert_eq!(main(&ctx, &args), 12, "{args:?}");
            assert!(err(&ctx).contains("Next: gpu setup"), "{args:?}");
        }
        // Direct wrappers cannot bypass it, including exec and absolute paths.
        for program in ["gcloud", "/usr/local/bin/gcloud"] {
            assert!(ctx.run(&Cmd::new(&[program, "version"])).is_err());
            assert!(ctx
                .exec(&[program.into(), "version".into()])
                .contains("gpu setup"));
        }
        assert!(runner.calls().is_empty());
        assert_eq!(main(&ctx, &["ls"]), 0);
        assert_eq!(main(&ctx, &["setup"]), 0);
        assert!(out(&ctx).contains("Google Cloud  not set up"));
        assert_eq!(main(&ctx, &["setup", "--provider", "vast"]), 0);
        assert!(runner.argvs().iter().all(|a| a[0] == "vastai"));
    }
}

#[test]
fn setup_needs_no_zone_and_populates_types_zones_and_sku_cache() {
    let (env, ctx, runner, _state) = fixture();
    assert_eq!(main(&ctx, APPLY), 0, "{}", err(&ctx));
    assert!(cfg(&env)
        .get("gcp")
        .unwrap()
        .as_dict()
        .unwrap()
        .get("zone")
        .is_none());
    assert!(runner.calls().iter().any(|c| c.program() == "curl"));
    assert_eq!(
        crate::gcp_facts::known_types(&ctx, "test-project", Some("us-east5-b")),
        ["v6e-1"]
    );
    assert_eq!(crate::gcp_facts::skus(&ctx).len(), 1);
    assert_eq!(
        crate::gcp::text(&crate::gcp_facts::skus(&ctx)[0], "id"),
        Some("test-sku")
    );
}
#[test]
fn setup_zone_flag_is_rejected_without_cloud_calls() {
    let (_env, ctx, runner, _state) = fixture();
    let mut args = APPLY.to_vec();
    args.extend(["--zone", "us-west1-b"]);
    assert_eq!(main(&ctx, &args), 1);
    assert!(err(&ctx).contains("Choose it at launch"));
    assert!(runner.calls().is_empty());
}

#[test]
fn provider_status_picker_real_keys_never_probes_gcloud() {
    use crate::picker::testing::*;
    let env = TestEnv::new();
    let runner = super::gcp_transport::runner(|cmd| {
        assert_eq!(cmd.program(), "vastai");
        ok("[]")
    });
    let ctx = env.ctx().runner(&runner).build();
    let screen = crate::setup::provider_screen(&ctx, &cfg(&env));
    let result = drive(screen, |keys| {
        keys.wait(|s| s.labels.iter().any(|l| l == "Vast.ai       ✓ set up"));
        assert!(keys
            .labels()
            .contains(&"Google Cloud  not set up".to_string()));
        keys.press(DOWN);
        keys.press(RIGHT);
    });
    assert_eq!(result, crate::picker::Outcome::Done("gcp".to_string()));
    assert!(runner.argvs().iter().all(|a| a[0] == "vastai"));
}
