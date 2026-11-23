//! Port of tests/test_wizard.py: the `gpu up` screen flow, driven with real
//! key presses through a pipe.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::picker::Gate;
use crate::error::Result;
use crate::picker::testing::*;
use crate::picker::{Job, Next, Outcome};
use crate::pyjson::Dict;
use crate::repos::Repo;
use crate::testing::{dict, TestEnv};
use crate::vast::offers::Stats;
use crate::wizard::{
    LaunchWizard, Picks, Search, TpuChoice, TpuOptions, WizardOptions, TPU_CATEGORY,
};

const MODELS: [&str; 5] = ["B200", "H100 SXM", "RTX 4090", "RTX 5090", "Tesla T4"];

pub fn offer(id: i64, gpu: &str, price: f64) -> Dict {
    dict(&format!(
        r#"{{"id": {id}, "gpu_name": "{gpu}", "num_gpus": 1, "dph_total": {price}, "reliability": 0.99, "geolocation": "Test, XX", "verification": "verified"}}"#
    ))
}

/// Stands in for OfferSearch; records calls and can hold offer searches open.
#[derive(Default)]
pub struct FakeSearch {
    pub offer_calls: Mutex<Vec<(String, Option<String>)>>,
    pub stats_calls: Mutex<Vec<String>>,
    pub gate: Gate,
    pub stats_gate: Option<Gate>,
}

impl FakeSearch {
    pub fn new() -> Arc<FakeSearch> {
        let search = FakeSearch::default();
        search.gate.set();
        Arc::new(search)
    }

    pub fn offer_calls(&self) -> Vec<(String, Option<String>)> {
        self.offer_calls.lock().unwrap().clone()
    }

    pub fn stats_calls(&self) -> Vec<String> {
        self.stats_calls.lock().unwrap().clone()
    }
}

impl Search for FakeSearch {
    fn offers(&self, mode: &str, gpu: Option<&str>) -> Result<Vec<Dict>> {
        self.offer_calls
            .lock()
            .unwrap()
            .push((mode.into(), gpu.map(String::from)));
        assert!(self.gate.wait(), "test never released the offer search");
        let gpu = gpu.unwrap_or_default();
        Ok(vec![offer(1, gpu, 0.5), offer(2, gpu, 0.3)])
    }

    fn stats(&self, mode: &str) -> Result<Stats> {
        self.stats_calls.lock().unwrap().push(mode.into());
        if let Some(gate) = &self.stats_gate {
            assert!(gate.wait(), "test never released price stats");
        }
        Ok([
            ("B200".to_string(), (1.735, 1)),
            ("RTX 4090".to_string(), (0.3, 12)),
        ]
        .into_iter()
        .collect())
    }
}

fn wizard(
    t: &TestEnv,
    search: &Arc<FakeSearch>,
    repo_calls: &Arc<AtomicUsize>,
    recent: &[&str],
    choose_offer: bool,
) -> LaunchWizard {
    let calls = repo_calls.clone();
    LaunchWizard::new(
        &t.env(),
        WizardOptions {
            repos: Job::new(move || {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(vec!["gpu-kernels".to_string(), "other".to_string()])
            }),
            search: Some(search.clone() as Arc<dyn Search>),
            models: MODELS.map(String::from).to_vec(),
            recent_gpus: recent.iter().map(|s| s.to_string()).collect(),
            choose_offer,
            owner: "sampleuser".into(),
            tpu: TpuOptions {
                types: vec!["v5e-1".into(), "v6e-1".into()],
                ..Default::default()
            },
            ..Default::default()
        },
    )
}

pub fn run(
    flow: &LaunchWizard,
    picks: Picks,
    script: impl FnOnce(&Keyboard) + Send + 'static,
) -> Outcome<Picks> {
    match flow.next(picks) {
        Next::Screen(first) => drive(first, script),
        Next::Notice(message) => panic!("unexpected initial notice: {message}"),
        Next::Done(picks) => Outcome::Done(picks),
    }
}

fn hardware_cells(label: &str) -> Vec<String> {
    regex::Regex::new(r" {2,}")
        .unwrap()
        .split(label)
        .map(String::from)
        .collect()
}

fn assert_hardware_grid(labels: &[String]) {
    assert!(labels.len() >= 2, "{labels:?}");
    let columns = labels
        .iter()
        .map(|label| {
            assert_eq!(hardware_cells(label).len(), 4, "{label:?}");
            let spot = label.find("Spot ").expect("Spot column");
            let count = label.rfind("  ").expect("offers column") + 2;
            [
                label[..spot].chars().count(),
                label[..count].chars().count(),
            ]
        })
        .collect::<Vec<_>>();
    assert!(
        columns.iter().all(|cols| *cols == columns[0]),
        "misaligned: {labels:?}"
    );
}

#[test]
fn line_labels_and_picker_cells_agree_on_unpriced_offers() {
    let stats = [
        ("RTX 5090".into(), (0.441, 64)),
        ("Unpriced".into(), (f64::INFINITY, 2)),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        crate::vast::offers::model_label("Unpriced", &stats),
        "Unpriced  - (2 offers)"
    );
    assert_eq!(
        crate::vast::offers::model_columns("Unpriced", &stats, "-")[1],
        "-"
    );
    assert_eq!(
        crate::vast::offers::category_label("All", &["Unpriced".into(), "RTX 5090".into()], &stats),
        "All (2 models, from $0.441/hr)"
    );
}

#[test]
fn hardware_cells_keep_offer_totals_and_do_not_invent_vast_spot_quotes() {
    let stats = [
        ("RTX 5090".into(), (0.441, 64)),
        ("H100 SXM".into(), (2.0, 3)),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        crate::vast::offers::model_columns("RTX 5090", &stats, "…"),
        ["RTX 5090", "$0.441/hr", "Spot -", "64 offers"]
    );
    assert_eq!(
        crate::vast::offers::model_columns("missing", &stats, "…"),
        ["missing", "…", "Spot -", "-"]
    );
    assert_eq!(
        crate::vast::offers::category_columns(
            "All",
            &["RTX 5090".into(), "H100 SXM".into()],
            &stats,
            "-"
        ),
        ["All", "from $0.441/hr", "Spot -", "67 offers"]
    );
}

#[test]
fn hardware_grid_aligns_recents_categories_search_and_models_without_new_jobs() {
    let env = TestEnv::new();
    let gate = Gate::default();
    let search = Arc::new(FakeSearch {
        stats_gate: Some(gate.clone()),
        ..Default::default()
    });
    let blocked = gate.clone();
    let rates = Job::new(move || {
        assert!(blocked.wait());
        Ok(Arc::new(super::gcp_prices::prices()))
    });
    let flow = LaunchWizard::new(
        &env.env(),
        WizardOptions {
            repos: Job::new(|| panic!("no-repo must not request repositories")),
            search: Some(search.clone()),
            models: MODELS.map(String::from).to_vec(),
            recent_gpus: vec!["RTX 4090".into()],
            choose_offer: true,
            owner: "example".into(),
            tpu: TpuOptions {
                prices: Some(rates),
                types: vec!["v6e-1".into()],
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let result = run(
        &flow,
        Picks::new(Some(Repo::NoRepo), Some("container"), None),
        move |keys| {
            assert_hardware_grid(&keys.labels());
            assert!(
                keys.press_until(DOWN, |s| s
                    .body
                    .iter()
                    .any(|l| l.starts_with('›') && l.contains("Datacenter")))
                    < quick()
            );
            gate.set();
            keys.wait(|s| {
                s.labels.iter().any(|l| l.contains("Spot $0.750/hr"))
                    && s.labels.iter().any(|l| l.contains("from $1.735/hr"))
            });
            assert_hardware_grid(&keys.labels());
            keys.type_text("0");
            let labels = keys.labels();
            assert!(labels.iter().any(|l| l.starts_with("RTX 4090")));
            assert!(labels.iter().any(|l| l.starts_with("TPU ")));
            assert_hardware_grid(&labels);
            keys.press_until(BACK, |s| s.title == "GPU" && s.filter.is_empty());
            keys.press(DOWN);
            keys.press_until(RIGHT, on("Datacenter Blackwell/Hopper"));
            assert_hardware_grid(&keys.labels());
            keys.press_until(BACK, on("GPU"));
            keys.press(CTRL_C);
        },
    );
    assert_eq!(result, Outcome::Cancelled(130));
    assert_eq!(search.stats_calls(), ["container"]);
    assert!(search.offer_calls().is_empty());
}

#[test]
fn metadata_jobs_are_running_on_repository_and_reused_through_tpu_capacity() {
    use crate::picker::JobState;
    use crate::testing::{dict, FakeRunner};
    for (last, preset, warm, mode_key) in [
        (None, None, "container", "1"),
        (Some("vm"), None, "vm", "2"),
        (Some("vm"), Some("container"), "container", "1"),
    ] {
        let env = TestEnv::new();
        if let Some(last) = last {
            crate::history::record_picks(&env.env(), &[("mode", last)]).unwrap();
        }
        let cfg = dict(
            r#"{"gcp":{"setup":{"version":1,"project":"test-project"},"project":"test-project","expiry":{"queue":"gpu-expiry","location":"us-central1","service_account":"reaper@test-project.iam.gserviceaccount.com"}}}"#,
        );
        let path = env.config.join("gpu");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("config.json"),
            crate::pyjson::dumps(&crate::pyjson::Value::Dict(cfg.clone()), None),
        )
        .unwrap();
        let runner = FakeRunner::forbid();
        let ctx = env.ctx().runner(&runner).build();
        let gate = Gate::default();
        let search = Arc::new(FakeSearch {
            stats_gate: Some(gate.clone()),
            ..Default::default()
        });
        search.gate.set();
        let price_calls = Arc::new(AtomicUsize::new(0));
        let catalog_calls = Arc::new(AtomicUsize::new(0));
        let blocked = gate.clone();
        let calls = price_calls.clone();
        let prices = Job::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            assert!(blocked.wait());
            Ok(Arc::new(super::gcp_prices::prices()))
        });
        let blocked = gate.clone();
        let calls = catalog_calls.clone();
        let catalog = Job::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            assert!(blocked.wait());
            Ok(vec!["v6e-1".into(), "v6e-4".into()])
        });
        let price_state = prices.observer();
        let catalog_state = catalog.observer();
        let flow = LaunchWizard::new(
            &env.env(),
            WizardOptions {
                repos: Job::new(|| Ok(vec!["demo".into()])),
                search: Some(search.clone()),
                models: MODELS.map(String::from).to_vec(),
                owner: "example".into(),
                tpu: TpuOptions {
                    transition: Some(crate::gcp::wizard::transition(
                        &ctx,
                        &crate::launch::UpArgs {
                            num_gpus: 1,
                            ..Default::default()
                        },
                        &cfg,
                        None,
                    )),
                    prices: Some(prices),
                    catalog: Some(catalog),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let watched = search.clone();
        let counted_prices = price_calls.clone();
        let counted_catalog = catalog_calls.clone();
        let result = run(&flow, Picks::new(None, preset, None), move |keys| {
            keys.wait(on("Repository"));
            assert_eq!(price_state.state(), JobState::Running);
            assert_eq!(catalog_state.state(), JobState::Running);
            keys.wait(|_| {
                watched.stats_calls() == [warm]
                    && counted_prices.load(Ordering::SeqCst) == 1
                    && counted_catalog.load(Ordering::SeqCst) == 1
            });
            // Gated work cannot complete, yet the first screen is drawn/usable.
            if preset.is_none() {
                keys.press_until("0", on("Mode"));
                assert_eq!(price_state.state(), JobState::Running);
            }
            gate.set();
            keys.wait(|_| {
                price_state.state() == JobState::Done && catalog_state.state() == JobState::Done
            });
            if preset.is_none() {
                keys.press_until(mode_key, on("GPU"));
            } else {
                keys.press_until("0", on("GPU"));
            }
            keys.type_text("v6e");
            keys.wait(|s| s.labels.iter().any(|l| l.contains("Spot $0.750/hr")));
            keys.press_until(RIGHT, on("v6e size"));
            keys.wait(|s| {
                s.labels
                    .iter()
                    .any(|l| l.contains("$2.750/hr") && l.contains("Spot $0.750/hr"))
            });
            keys.press_until(ENTER, on("TPU capacity"));
            keys.wait(|s| {
                s.labels
                    == [
                        "On-demand  $2.750/hr",
                        "Spot       $0.750/hr  (may be interrupted)",
                    ]
            });
            assert_eq!(keys.footer(), "↑/↓ move  → select  ← back  1-9 select");
            keys.press_until(BACK, on("v6e size"));
            keys.press_until(ENTER, on("TPU capacity"));
            assert_eq!(watched.stats_calls(), [warm]);
            assert_eq!(counted_prices.load(Ordering::SeqCst), 1);
            assert_eq!(counted_catalog.load(Ordering::SeqCst), 1);
            keys.press(CTRL_C);
        });
        assert_eq!(result, Outcome::Cancelled(130));
        assert!(
            runner.calls().is_empty(),
            "generation/size/capacity must reuse the original Jobs, not create new cloud requests"
        );
        assert_eq!(search.stats_calls(), [warm]);
        assert_eq!(price_calls.load(Ordering::SeqCst), 1);
        assert_eq!(catalog_calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn prefetch_without_google_setup_never_calls_cloud_even_after_opening_gpu() {
    let env = TestEnv::new();
    let runner = crate::testing::FakeRunner::forbid();
    let ctx = env.ctx().runner(&runner).build();
    let cfg = Dict::new();
    let prices = crate::gcp::prices::job(&ctx, &cfg);
    let catalog = crate::gcp::catalog::types_job(&ctx, &cfg);
    let price_state = prices.observer();
    let catalog_state = catalog.observer();
    let flow = LaunchWizard::new(
        &env.env(),
        WizardOptions {
            search: Some(FakeSearch::new()),
            owner: "example".into(),
            tpu: TpuOptions {
                setup_error: Some("needs gpu setup".into()),
                prices: Some(prices),
                catalog: Some(catalog),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let result = run(&flow, Picks::default(), move |keys| {
        // Digits quick-select only once the repository list has loaded.
        keys.wait(|s| s.title == "Repository" && s.labels.contains(&"No repository".to_string()));
        assert_eq!(price_state.state(), crate::picker::JobState::Idle);
        assert_eq!(catalog_state.state(), crate::picker::JobState::Idle);
        keys.press_until("0", on("Mode"));
        keys.press_until("1", on("GPU"));
        keys.type_text("tpu");
        keys.wait(|s| s.labels.iter().any(|l| l.contains("needs gpu setup")));
        keys.wait(|_| {
            price_state.state() == crate::picker::JobState::Failed
                && catalog_state.state() == crate::picker::JobState::Done
        });
        keys.press(CTRL_C);
    });
    assert_eq!(result, Outcome::Cancelled(130));
    assert!(runner.calls().is_empty());
}

fn named(name: &str) -> Option<Repo> {
    Some(Repo::from_arg(name))
}

fn quick() -> Duration {
    Duration::from_millis(500)
}

#[test]
fn test_back_walks_every_screen_without_refetching_or_quitting() {
    let t = TestEnv::new();
    let (search, repo_calls) = (FakeSearch::new(), Arc::new(AtomicUsize::new(0)));
    let flow = wizard(&t, &search, &repo_calls, &[], true);
    let watched = search.clone();
    let result = run(&flow, Picks::default(), move |keys| {
        keys.wait(|s| s.labels == ["gpu-kernels", "other", "Clone from URL...", "No repository"]);
        keys.press_until(RIGHT, on("Mode"));
        keys.press_until(RIGHT, on("GPU"));
        keys.press_until(RIGHT, on("Datacenter Blackwell/Hopper"));
        keys.press_until(RIGHT, on("Offer: B200"));
        for title in ["Datacenter Blackwell/Hopper", "GPU", "Mode", "Repository"] {
            assert!(keys.press_until(BACK, on(title)) < quick());
        }
        // Forward again with different choices; earlier picks must not leak.
        keys.press_until("2", on("Mode"));
        keys.press(DOWN);
        keys.press_until(RIGHT, on("GPU"));
        keys.type_text("4090");
        keys.press_until(RIGHT, on("Offer: RTX 4090"));
        keys.wait_for("the vm offer search", || {
            watched.offer_calls().last() == Some(&("vm".to_string(), Some("RTX 4090".to_string())))
        });
        keys.wait(|s| !s.labels.is_empty());
        keys.press(RIGHT);
    });
    let expected = Picks {
        repo: named("other"),
        mode: Some("vm".into()),
        gpu: Some("RTX 4090".into()),
        offer: Some(offer(1, "RTX 4090", 0.5)),
        modal: None,
        tpu: None,
        cloud: None,
        ..Default::default()
    };
    assert_eq!(result, Outcome::Done(expected));
    assert_eq!(repo_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn test_repository_shortcuts_full_list_and_global_filter() {
    for action in ["0", "1", "7", "8", "9", "filter"] {
        let t = TestEnv::new();
        let flow = LaunchWizard::new(
            &t.env(),
            WizardOptions {
                repos: Job::new(|| Ok((1..=12).map(|i| format!("project-{i}")).collect())),
                models: MODELS.map(String::from).to_vec(),
                owner: "sampleuser".into(),
                ..Default::default()
            },
        );
        let result = run(
            &flow,
            Picks::new(None, Some("container"), Some("B200")),
            move |keys| {
                keys.wait(|s| s.labels.len() == 10);
                assert_eq!(
                    keys.labels(),
                    [
                        "project-1",
                        "project-2",
                        "project-3",
                        "project-4",
                        "project-5",
                        "project-6",
                        "project-7",
                        "All repositories...",
                        "Clone from URL...",
                        "No repository"
                    ]
                );
                let body = keys.snapshot().body;
                assert!(body[0].starts_with("›  1. project-1"));
                assert!(body.last().unwrap().contains("0. No repository"));
                match action {
                    "8" => {
                        keys.press_until("8", on("All repositories"));
                        assert_eq!(keys.labels().len(), 12);
                        keys.type_text("project-12");
                        assert_eq!(keys.labels(), ["project-12"]);
                        keys.press(BACK); // clear filter before returning
                        keys.press_until(BACK, on("Repository"));
                        keys.press_until("8", on("All repositories"));
                        keys.type_text("project-12");
                        keys.press(RIGHT);
                    }
                    "9" => {
                        keys.press_until("9", on("Clone from URL"));
                        keys.type_text("owner/external");
                        keys.press(RIGHT);
                    }
                    "filter" => {
                        keys.type_text("project-12");
                        assert_eq!(keys.labels(), ["project-12"]);
                        keys.press(RIGHT);
                    }
                    digit => keys.press(digit),
                }
            },
        );
        let repo = match action {
            "0" => Repo::NoRepo,
            "1" => Repo::from_arg("project-1"),
            "7" => Repo::from_arg("project-7"),
            "9" => Repo::from_arg("owner/external"),
            _ => Repo::from_arg("project-12"),
        };
        assert_eq!(
            result,
            Outcome::Done(Picks::new(Some(repo), Some("container"), Some("B200")))
        );
    }
}

#[test]
fn test_short_repository_lists_keep_static_digits_and_omit_all() {
    for count in [0, 7] {
        let t = TestEnv::new();
        let flow = LaunchWizard::new(
            &t.env(),
            WizardOptions {
                repos: Job::new(move || Ok((1..=count).map(|i| format!("project-{i}")).collect())),
                owner: "sampleuser".into(),
                ..Default::default()
            },
        );
        let result = run(
            &flow,
            Picks::new(None, Some("container"), Some("B200")),
            move |keys| {
                keys.wait(|s| s.labels.len() == count + 2);
                assert!(!keys.labels().contains(&"All repositories...".into()));
                keys.press_until("9", on("Clone from URL"));
                keys.press_until(BACK, on("Repository"));
                keys.type_text("No repository");
                assert_eq!(keys.labels(), ["No repository"]);
                keys.press(BACK);
                keys.press("0");
            },
        );
        assert_eq!(
            result,
            Outcome::Done(Picks::new(
                Some(Repo::NoRepo),
                Some("container"),
                Some("B200")
            ))
        );
    }
}

#[test]
fn test_left_twice_on_the_first_real_screen_quits() {
    let t = TestEnv::new();
    let repo_calls = Arc::new(AtomicUsize::new(0));
    let flow = wizard(&t, &FakeSearch::new(), &repo_calls, &[], true);
    // --repo and --mode given: GPU is the first screen.
    let result = run(
        &flow,
        Picks::new(named("gpu-kernels"), Some("container"), None),
        |keys| {
            keys.press_until(RIGHT, on("Datacenter Blackwell/Hopper"));
            keys.press_until(BACK, on("GPU"));
            keys.press(BACK);
            keys.press(BACK);
        },
    );
    assert_eq!(result, Outcome::Cancelled(1));
    assert_eq!(repo_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn test_flags_for_every_pick_skip_the_picker() {
    let t = TestEnv::new();
    let flow = wizard(
        &t,
        &FakeSearch::new(),
        &Arc::new(AtomicUsize::new(0)),
        &[],
        false,
    );
    let ctx = t.ctx().build();
    let picks = Picks::new(named("gpu-kernels"), Some("vm"), Some("RTX 4090"));
    assert_eq!(flow.run(&ctx, picks.clone()).unwrap(), picks);
}

#[test]
fn test_offer_search_never_blocks_navigation() {
    let t = TestEnv::new();
    let search = Arc::new(FakeSearch::default());
    let flow = wizard(&t, &search, &Arc::new(AtomicUsize::new(0)), &[], true);
    let watched = search.clone();
    let result = run(
        &flow,
        Picks::new(named("gpu-kernels"), Some("container"), None),
        move |keys| {
            keys.type_text("t4");
            assert!(keys.press_until(RIGHT, on("Offer: Tesla T4")) < quick());
            keys.wait_for("the offer search", || {
                watched.offer_calls() == [("container".to_string(), Some("Tesla T4".to_string()))]
            });
            keys.press(RIGHT); // still searching: nothing to select
            assert_eq!(keys.title(), "Offer: Tesla T4");
            assert!(keys.press_until(BACK, on("GPU")) < quick());
            keys.press(CTRL_C);
        },
    );
    search.gate.set();
    assert_eq!(result, Outcome::Cancelled(130));
}

#[test]
fn test_auto_prices_are_shared_by_gpu_screens_and_loaded_once_per_mode() {
    let t = TestEnv::new();
    let search = FakeSearch::new();
    let flow = wizard(&t, &search, &Arc::new(AtomicUsize::new(0)), &[], false);
    let gpu_labels = Arc::new(Mutex::new(Vec::new()));
    let (watched, labels) = (search.clone(), gpu_labels.clone());
    let result = run(&flow, Picks::default(), move |keys| {
        keys.wait(|s| s.labels.contains(&"gpu-kernels".to_string()));
        keys.press_until("0", on("Mode"));
        keys.press_until(RIGHT, on("GPU"));
        keys.wait_for("container prices", || {
            watched.stats_calls() == ["container"]
        });
        keys.wait(|s| s.labels.join(" ").contains("from $1.735/hr"));
        keys.press_until(RIGHT, on("Datacenter Blackwell/Hopper"));
        labels.lock().unwrap().extend(keys.labels());
        keys.press_until(BACK, on("GPU"));
        keys.press_until(BACK, on("Mode"));
        keys.press(DOWN);
        keys.press_until(RIGHT, on("GPU"));
        assert!(!keys.footer().contains("F2"));
        keys.wait_for("vm prices", || watched.stats_calls() == ["container", "vm"]);
        keys.type_text("4090");
        keys.press(RIGHT);
    });
    let expected = Picks {
        repo: Some(Repo::NoRepo),
        mode: Some("vm".into()),
        gpu: Some("RTX 4090".into()),
        offer: None,
        modal: None,
        tpu: None,
        cloud: None,
        ..Default::default()
    };
    assert_eq!(result, Outcome::Done(expected));
    let labels = gpu_labels.lock().unwrap();
    assert_hardware_grid(&labels);
    assert_eq!(
        labels
            .iter()
            .map(|label| hardware_cells(label))
            .collect::<Vec<_>>(),
        [
            vec!["B200", "$1.735/hr", "Spot -", "1 offer"],
            vec!["H100 SXM", "-", "Spot -", "-"],
        ]
    );
}

#[test]
fn test_gpu_screen_caps_valid_recents_at_three_and_always_lists_tpus() {
    let t = TestEnv::new();
    let search = FakeSearch::new();
    let repo_calls = Arc::new(AtomicUsize::new(0));
    let flow = wizard(
        &t,
        &search,
        &repo_calls,
        &[
            "Retired GPU",
            "RTX 4090",
            "B200",
            "Tesla T4",
            "RTX 5090",
            "H100 SXM",
        ],
        true,
    );
    let result = run(
        &flow,
        Picks::new(Some(Repo::NoRepo), Some("container"), None),
        |keys| {
            let labels = keys.labels();
            assert_eq!(
                labels
                    .iter()
                    .filter(|s| s.starts_with("Recent:"))
                    .map(|s| s.split("  ").next().unwrap().to_string())
                    .collect::<Vec<_>>(),
                ["Recent: RTX 4090", "Recent: B200", "Recent: Tesla T4"]
            );
            assert!(labels.iter().any(|l| l.starts_with(TPU_CATEGORY)));
            // Select the visible category without filtering; no offer screen.
            for _ in 0..labels
                .iter()
                .position(|s| s.starts_with(TPU_CATEGORY))
                .unwrap()
            {
                keys.press(DOWN);
            }
            keys.press(RIGHT);
        },
    );
    assert_eq!(
        result,
        Outcome::Done(Picks {
            tpu: Some(TpuChoice::ChooseType),
            ..Picks::new(Some(Repo::NoRepo), Some("container"), None)
        })
    );
    assert_eq!(repo_calls.load(Ordering::SeqCst), 0);
    assert!(search.offer_calls().is_empty());
    assert!(search.stats_calls().len() <= 1);
}

#[test]
fn test_gpu_search_finds_cached_tpu_category_and_types() {
    for (query, expected) in [
        ("tpu", TpuChoice::ChooseType),
        ("v5e", TpuChoice::Type("v5e".into())),
        ("v6e-1", TpuChoice::Type("v6e-1".into())),
    ] {
        let t = TestEnv::new();
        let search = FakeSearch::new();
        let flow = wizard(&t, &search, &Arc::new(AtomicUsize::new(0)), &[], true);
        let start = Picks::new(named("other"), Some("vm"), None);
        let result = run(&flow, start.clone(), move |keys| {
            keys.type_text(query);
            if query == "tpu" {
                assert!(keys.labels().len() <= 7, "show generations, not all sizes");
            }
            assert!(!keys.labels().is_empty());
            assert!(keys.labels().iter().all(|s| s.contains("TPU")));
            keys.press(RIGHT);
        });
        assert_eq!(
            result,
            Outcome::Done(Picks {
                tpu: Some(expected),
                ..start
            })
        );
        assert!(search.offer_calls().is_empty());
        assert!(search.stats_calls().len() <= 1);
    }
}

#[test]
fn test_recent_gpus_come_first_and_digits_filter_gpu_names() {
    let t = TestEnv::new();
    let flow = wizard(
        &t,
        &FakeSearch::new(),
        &Arc::new(AtomicUsize::new(0)),
        &["RTX 4090", "Retired GPU"],
        false,
    );
    let start = Picks::new(named("r"), Some("container"), None);
    let result = run(&flow, start.clone(), |keys| keys.press(RIGHT));
    assert_eq!(
        result,
        Outcome::Done(Picks {
            gpu: Some("RTX 4090".into()),
            ..start.clone()
        })
    );
    let result = run(&flow, start.clone(), |keys| {
        keys.type_text("5090");
        keys.press(RIGHT);
    });
    assert_eq!(
        result,
        Outcome::Done(Picks {
            gpu: Some("RTX 5090".into()),
            ..start
        })
    );
}

fn dir_wizard(t: &TestEnv, count: usize) -> LaunchWizard {
    LaunchWizard::new(
        &t.env(),
        WizardOptions {
            repos: Job::new(move || Ok((1..=count).map(|i| format!("project-{i}")).collect())),
            owner: "sampleuser".into(),
            local_dir: true,
            ..Default::default()
        },
    )
}

fn dir_picks() -> Picks {
    Picks::new(None, Some("container"), Some("B200"))
}

#[test]
fn test_local_directory_row_shortens_the_recent_list_by_one() {
    let t = TestEnv::new();
    let flow = dir_wizard(&t, 12);
    let result = run(&flow, dir_picks(), move |keys| {
        keys.wait(|s| s.labels.len() == 10);
        assert_eq!(
            keys.labels(),
            [
                "project-1",
                "project-2",
                "project-3",
                "project-4",
                "project-5",
                "project-6",
                "From local directory...",
                "All repositories...",
                "Clone from URL...",
                "No repository"
            ]
        );
        let body = keys.snapshot().body;
        assert!(body[6].contains("7. From local directory..."));
        assert!(body[7].contains("8. All repositories..."));
        keys.press("6");
    });
    assert_eq!(
        result,
        Outcome::Done(Picks::new(
            Some(Repo::from_arg("project-6")),
            Some("container"),
            Some("B200")
        ))
    );
}

#[test]
fn test_all_repositories_row_follows_the_shortened_list() {
    for (count, all) in [(0, false), (6, false), (7, true)] {
        let t = TestEnv::new();
        let flow = dir_wizard(&t, count);
        let result = run(&flow, dir_picks(), move |keys| {
            keys.wait(|s| s.labels.contains(&"No repository".to_string()));
            assert_eq!(
                keys.labels().contains(&"All repositories...".to_string()),
                all
            );
            assert_eq!(keys.labels().len(), count.min(6) + 3 + usize::from(all));
            keys.press("0");
        });
        assert_eq!(
            result,
            Outcome::Done(Picks::new(
                Some(Repo::NoRepo),
                Some("container"),
                Some("B200")
            ))
        );
    }
}

#[test]
fn test_local_directory_row_asks_for_a_valid_path() {
    let t = TestEnv::new();
    let target = t.home.join("proj");
    std::fs::create_dir_all(&target).unwrap();
    let file = t.home.join("note.txt");
    std::fs::write(&file, "x").unwrap();
    let (typed, bad_file) = (target.display().to_string(), file.display().to_string());
    let flow = dir_wizard(&t, 2);
    let result = run(&flow, dir_picks(), move |keys| {
        keys.wait(|s| s.labels.contains(&"From local directory...".to_string()));
        keys.press_until("7", on("From local directory"));
        keys.type_text("/definitely/missing/dir");
        keys.press(RIGHT);
        assert_eq!(keys.title(), "From local directory");
        keys.press(BACK);
        keys.type_text(&bad_file);
        keys.press(RIGHT);
        assert_eq!(keys.title(), "From local directory");
        keys.press(BACK);
        keys.type_text(&typed);
        keys.press(RIGHT);
    });
    let Outcome::Done(picks) = result else {
        panic!("{result:?}");
    };
    assert_eq!(picks.repo, Some(Repo::NoRepo));
    assert_eq!(picks.dir, Some(target.canonicalize().unwrap()));
}

#[test]
fn test_local_directory_defaults_to_the_current_directory() {
    let t = TestEnv::new();
    let flow = dir_wizard(&t, 2);
    let result = run(&flow, dir_picks(), move |keys| {
        keys.wait(|s| s.labels.contains(&"From local directory...".to_string()));
        keys.press_until("7", on("From local directory"));
        keys.press(RIGHT);
    });
    let Outcome::Done(picks) = result else {
        panic!("{result:?}");
    };
    assert_eq!(
        picks.dir,
        Some(std::env::current_dir().unwrap().canonicalize().unwrap())
    );
}
