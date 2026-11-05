//! Port of tests/test_repo_cache.py: a cached repository pick never
//! launches without fresh GitHub validation.

use super::picker::Gate;
use crate::launch::sync_repos;
use crate::picker::testing::*;
use crate::picker::{Job, JobState, Outcome};
use crate::repos::{load_cached_repos, save_cached_repos, Repo};
use crate::testing::{ok, strs, FakeRunner, TestEnv};
use crate::wizard::{LaunchWizard, Picks, WizardOptions};

pub fn flow(t: &TestEnv, job: Job<Vec<String>>) -> LaunchWizard {
    LaunchWizard::new(
        &t.env(),
        WizardOptions {
            repos: job,
            search: None,
            models: Vec::new(),
            recent_gpus: Vec::new(),
            choose_offer: false,
            tpu_setup_error: None,
            tpu_prices: None,
            tpu_cached_prices: None,
            tpu_types: Vec::new(),
            tpu_catalog: None,
            owner: "minoya1220".into(),
        },
    )
}

/// A repository listing that waits for `gate`, then answers `result`.
pub fn gated(gate: &Gate, result: Result<Vec<&str>, &str>) -> Job<Vec<String>> {
    let gate = gate.clone();
    let result: Result<Vec<String>, String> = result
        .map(|v| v.iter().map(|s| s.to_string()).collect())
        .map_err(String::from);
    Job::new(move || {
        if !gate.wait() {
            return Err("test never released the loader".into());
        }
        result.clone()
    })
}

fn b200() -> Picks {
    Picks::new(None, None, Some("B200"))
}

fn save(t: &TestEnv, names: &[&str]) {
    let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    save_cached_repos(&t.env(), "minoya1220", &names).unwrap();
}

#[test]
fn test_cached_display_and_wait_before_launch() {
    let t = TestEnv::new();
    save(&t, &["old"]);
    let gate = Gate::default();
    let job = gated(&gate, Ok(vec!["old", "new"]));
    let observer = job.observer();
    let release = gate.clone();
    let result = super::wizard::run(&flow(&t, job), b200(), move |keys| {
        assert_eq!(keys.labels(), ["old", "Clone from URL...", "No repository"]);
        keys.wait_for("the sync to run", || observer.state() == JobState::Running);
        keys.press_until("1", on("Mode"));
        keys.press_until("1", on("Checking repositories"));
        assert!(!keys.snapshot().done);
        release.set();
    });
    gate.set();
    assert_eq!(
        result,
        Outcome::Done(Picks::new(
            Some(Repo::from_arg("old")),
            Some("container"),
            Some("B200")
        ))
    );
}

#[test]
fn test_invalid_cache_resets_with_message_and_never_launches() {
    let t = TestEnv::new();
    save(&t, &["deleted"]);
    let gate = Gate::default();
    let release = gate.clone();
    let result = super::wizard::run(
        &flow(&t, gated(&gate, Ok(vec!["new"]))),
        b200(),
        move |keys| {
            keys.press_until("1", on("Mode"));
            keys.press_until("1", on("Checking repositories"));
            release.set();
            keys.wait(on("Repository"));
            assert_eq!(
                keys.snapshot().notice,
                "Repository not available or deleted: deleted"
            );
            assert_eq!(keys.labels(), ["new", "Clone from URL...", "No repository"]);
            keys.press(CTRL_C);
        },
    );
    gate.set();
    assert!(matches!(result, Outcome::Cancelled(_)));
}

#[test]
fn test_sync_error_resets_without_launch() {
    let t = TestEnv::new();
    save(&t, &["old"]);
    let gate = Gate::default();
    let release = gate.clone();
    let result = super::wizard::run(
        &flow(&t, gated(&gate, Err("GitHub unavailable"))),
        b200(),
        move |keys| {
            keys.press_until("1", on("Mode"));
            keys.press_until("1", on("Checking repositories"));
            release.set();
            keys.wait(on("Repository"));
            assert!(keys.snapshot().notice.contains("GitHub unavailable"));
            keys.press(CTRL_C);
        },
    );
    gate.set();
    assert!(matches!(result, Outcome::Cancelled(_)));
}

#[test]
fn test_first_run_waits_for_sync() {
    let t = TestEnv::new();
    let gate = Gate::default();
    let release = gate.clone();
    let start = Picks::new(None, Some("vm"), Some("B200"));
    let result = super::wizard::run(
        &flow(&t, gated(&gate, Ok(vec!["fresh"]))),
        start,
        move |keys| {
            assert!(keys.labels().is_empty());
            assert_eq!(keys.snapshot().loading, "Loading repositories...");
            release.set();
            keys.wait(|s| s.labels.contains(&"fresh".to_string()));
            keys.press("1");
        },
    );
    gate.set();
    let Outcome::Done(picks) = result else {
        panic!("{result:?}")
    };
    assert_eq!(picks.repo, Some(Repo::from_arg("fresh")));
}

#[test]
fn test_sync_rewrites_cache_with_names_only() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|cmd| match strs(&cmd.argv)[..] {
        ["gh", "repo", "list", "minoya1220", "--json", "name", "--limit", "1000"] => {
            ok(r#"[{"name": "fresh"}]"#)
        }
        _ => panic!("unexpected command: {:?}", cmd.argv),
    });
    let ctx = t.ctx().runner(&runner).build();
    save(&t, &["old"]);
    assert_eq!(sync_repos(&ctx, "minoya1220").unwrap(), ["fresh"]);
    assert_eq!(
        load_cached_repos(&t.env(), "minoya1220"),
        Some(vec!["fresh".to_string()])
    );
}
