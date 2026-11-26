//! Instance owners: flag/env precedence, the label, `gpu ls` and the duplicate warning.

use super::cli::dry_run_request;
use crate::owner;
use crate::testing::*;

const LIVE: &str = r#"[
 {"id": 1, "gpu_name": "RTX 4090", "actual_status": "running", "label": "proj@firstmate"},
 {"id": 2, "gpu_name": "RTX 4090", "actual_status": "running", "label": "proj@rental-agent"},
 {"id": 3, "gpu_name": "RTX 4090", "actual_status": "exited", "label": "proj@carol"},
 {"id": 4, "gpu_name": "RTX 4090", "actual_status": "running", "label": "proj"}
]"#;

#[test]
fn flag_beats_env_beats_none() {
    let t = TestEnv::new();
    let with_env = t.ctx().env(t.env().with("GPU_OWNER", "from-env")).build();
    assert_eq!(
        owner::resolve(&with_env, Some("from-flag"))
            .unwrap()
            .as_deref(),
        Some("from-flag")
    );
    assert_eq!(
        owner::resolve(&with_env, None).unwrap().as_deref(),
        Some("from-env")
    );
    assert_eq!(
        owner::resolve(&with_env, Some("")).unwrap().as_deref(),
        Some("from-env")
    );
    let bare = t.ctx().build();
    assert_eq!(owner::resolve(&bare, None).unwrap(), None);
    assert!(owner::resolve(&bare, Some("bad owner@x")).is_err());
}

#[test]
fn label_round_trips_and_keeps_the_repo_name() {
    assert_eq!(owner::split("proj@firstmate"), ("proj", Some("firstmate")));
    assert_eq!(owner::split("proj"), ("proj", None));
    assert_eq!(owner::split("no-repo@a.b"), ("no-repo", Some("a.b")));
    let item = dict(r#"{"label": "proj@firstmate"}"#);
    assert_eq!(crate::instances::repo_name(&item).as_deref(), Some("proj"));
}

fn launch_label(t: &TestEnv, env: crate::ctx::Env, flags: &[&str]) -> String {
    let runner = FakeRunner::new(|cmd| {
        assert_eq!(cmd.argv[..3], ["vastai", "search", "offers"]);
        ok(
            r#"[{"id": 42, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.1, "reliability": 0.99, "verification": "verified"}]"#,
        )
    });
    let ctx = t.ctx().env(env).runner(&runner).build();
    let mut argv = vec![
        "up",
        "--no-repo",
        "--mode",
        "container",
        "--gpu",
        "Tesla T4",
        "--yes",
        "--dry-run",
        "--expire",
        "2h",
    ];
    argv.extend_from_slice(flags);
    assert_eq!(main(&ctx, &argv), 0, "{}", err(&ctx));
    let request = dry_run_request(&out(&ctx));
    request[request.iter().position(|a| a == "--label").unwrap() + 1].clone()
}

#[test]
fn launch_label_carries_the_owner() {
    let t = TestEnv::new();
    assert_eq!(launch_label(&t, t.env(), &[]), "no-repo");
    assert_eq!(
        launch_label(&t, t.env(), &["--owner", "firstmate"]),
        "no-repo@firstmate"
    );
    let env = t.env().with("GPU_OWNER", "envy");
    assert_eq!(launch_label(&t, env.clone(), &[]), "no-repo@envy");
    assert_eq!(
        launch_label(&t, env, &["--owner", "firstmate"]),
        "no-repo@firstmate"
    );
}

#[test]
fn ls_shows_filters_and_reports_owners() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|_| ok(LIVE));
    let ctx = t.ctx().runner(&runner).build();
    assert_eq!(main(&ctx, &["ls"]), 0);
    let table = out(&ctx);
    assert!(table.lines().next().unwrap().ends_with("Owner"));
    assert!(table.contains("firstmate") && table.contains("rental-agent"));
    let row4 = table.lines().find(|l| l.starts_with("4 ")).unwrap();
    assert!(
        row4.trim_end().ends_with('-') || row4.trim_end().ends_with("0s"),
        "{row4}"
    );

    let ctx = t.ctx().runner(&runner).build();
    assert_eq!(main(&ctx, &["ls", "--owner", "firstmate"]), 0);
    let table = out(&ctx);
    assert!(table.contains("firstmate") && !table.contains("rental-agent"));

    let ctx = t.ctx().runner(&runner).build();
    assert_eq!(main(&ctx, &["ls", "--json", "--owner", "rental-agent"]), 0);
    let listed = json(&out(&ctx));
    let items = listed.as_list().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].get("owner").unwrap().as_str(),
        Some("rental-agent")
    );
}

#[test]
fn duplicate_warning_fires_only_for_the_same_owner() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|_| ok(LIVE));
    let ctx = t.ctx().runner(&runner).build();
    owner::warn_duplicate(&ctx, &crate::config::defaults(), "firstmate").unwrap();
    let warning = err(&ctx);
    assert!(
        warning.contains("firstmate") && warning.contains("1"),
        "{warning}"
    );
    assert!(!warning.contains('2'), "{warning}");
    // Another owner's instance, a stopped one, and an unowned one never trigger it.
    for other in ["carol", "nobody"] {
        let ctx = t.ctx().runner(&runner).build();
        owner::warn_duplicate(&ctx, &crate::config::defaults(), other).unwrap();
        assert_eq!(err(&ctx), "");
    }
}
