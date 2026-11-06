//! Port of tests/test_wizard.py: the `gpu up` screen flow, driven with real
//! key presses through a pipe.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::picker::Gate;
use crate::error::Result;
use crate::offers::Stats;
use crate::picker::testing::*;
use crate::picker::{Job, Next, Outcome};
use crate::pyjson::Dict;
use crate::repos::Repo;
use crate::testing::{dict, TestEnv};
use crate::wizard::{LaunchWizard, Picks, Search, TpuChoice, WizardOptions, TPU_CATEGORY};

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
            tpu_setup_error: None,
            tpu_transition: None,
            tpu_prices: None,
            tpu_cached_prices: None,
            tpu_types: Vec::new(),
            tpu_catalog: None,
            owner: "minoya1220".into(),
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

#[test]
fn metadata_jobs_are_running_on_repository_and_reused_through_mode_gpu_and_tpu_size() {
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
                    None,
                )),
                tpu_prices: Some(prices),
                tpu_cached_prices: None,
                tpu_types: Vec::new(),
                tpu_catalog: Some(catalog),
                owner: "example".into(),
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
            assert_eq!(watched.stats_calls(), [warm]);
            assert_eq!(counted_prices.load(Ordering::SeqCst), 1);
            assert_eq!(counted_catalog.load(Ordering::SeqCst), 1);
            keys.press(CTRL_C);
        });
        assert_eq!(result, Outcome::Cancelled(130));
        assert!(
            runner.calls().is_empty(),
            "generation/size must reuse the original Jobs, not create new cloud requests"
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
    let prices = crate::gcp_prices::job(&ctx, &cfg);
    let catalog = crate::gcp_catalog::types_job(&ctx, &cfg);
    let price_state = prices.observer();
    let catalog_state = catalog.observer();
    let flow = LaunchWizard::new(
        &env.env(),
        WizardOptions {
            repos: Job::new(|| Ok(Vec::new())),
            search: Some(FakeSearch::new()),
            models: Vec::new(),
            recent_gpus: Vec::new(),
            choose_offer: false,
            tpu_setup_error: Some("needs gpu setup".into()),
            tpu_transition: None,
            tpu_prices: Some(prices),
            tpu_cached_prices: None,
            tpu_types: Vec::new(),
            tpu_catalog: Some(catalog),
            owner: "example".into(),
        },
    );
    let result = run(&flow, Picks::default(), move |keys| {
        keys.wait(on("Repository"));
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
        keys.press_until(ENTER, on("Mode"));
        keys.press_until(ENTER, on("GPU"));
        keys.press_until(ENTER, on("Datacenter Blackwell/Hopper"));
        keys.press_until(ENTER, on("Offer: B200"));
        for title in ["Datacenter Blackwell/Hopper", "GPU", "Mode", "Repository"] {
            assert!(keys.press_until(BACK, on(title)) < quick());
        }
        // Forward again with different choices; earlier picks must not leak.
        keys.press_until("2", on("Mode"));
        keys.press(DOWN);
        keys.press_until(ENTER, on("GPU"));
        keys.type_text("4090");
        keys.press_until(ENTER, on("Offer: RTX 4090"));
        keys.wait_for("the vm offer search", || {
            watched.offer_calls().last() == Some(&("vm".to_string(), Some("RTX 4090".to_string())))
        });
        keys.wait(|s| !s.labels.is_empty());
        keys.press(ENTER);
    });
    let expected = Picks {
        repo: named("other"),
        mode: Some("vm".into()),
        gpu: Some("RTX 4090".into()),
        offer: Some(offer(1, "RTX 4090", 0.5)),
        tpu: None,
        cloud: None,
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
                search: None,
                models: MODELS.map(String::from).to_vec(),
                recent_gpus: Vec::new(),
                choose_offer: false,
                tpu_setup_error: None,
                tpu_transition: None,
                tpu_prices: None,
                tpu_cached_prices: None,
                tpu_types: Vec::new(),
                tpu_catalog: None,
                owner: "minoya1220".into(),
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
                        keys.press(ENTER);
                    }
                    "9" => {
                        keys.press_until("9", on("Clone from URL"));
                        keys.type_text("owner/external");
                        keys.press(ENTER);
                    }
                    "filter" => {
                        keys.type_text("project-12");
                        assert_eq!(keys.labels(), ["project-12"]);
                        keys.press(ENTER);
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
                search: None,
                models: Vec::new(),
                recent_gpus: Vec::new(),
                choose_offer: false,
                tpu_setup_error: None,
                tpu_transition: None,
                tpu_prices: None,
                tpu_cached_prices: None,
                tpu_types: Vec::new(),
                tpu_catalog: None,
                owner: "minoya1220".into(),
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
fn test_backtick_twice_on_the_first_real_screen_quits() {
    let t = TestEnv::new();
    let repo_calls = Arc::new(AtomicUsize::new(0));
    let flow = wizard(&t, &FakeSearch::new(), &repo_calls, &[], true);
    // --repo and --mode given: GPU is the first screen.
    let result = run(
        &flow,
        Picks::new(named("gpu-kernels"), Some("container"), None),
        |keys| {
            keys.press_until(ENTER, on("Datacenter Blackwell/Hopper"));
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
            assert!(keys.press_until(ENTER, on("Offer: Tesla T4")) < quick());
            keys.wait_for("the offer search", || {
                watched.offer_calls() == [("container".to_string(), Some("Tesla T4".to_string()))]
            });
            keys.press(ENTER); // still searching: nothing to select
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
        keys.press_until(ENTER, on("GPU"));
        keys.wait_for("container prices", || {
            watched.stats_calls() == ["container"]
        });
        keys.wait(|s| s.labels.join(" ").contains("(1 model, from $1.735/hr)"));
        keys.press_until(ENTER, on("Datacenter Blackwell/Hopper"));
        labels.lock().unwrap().extend(keys.labels());
        keys.press_until(BACK, on("GPU"));
        keys.press_until(BACK, on("Mode"));
        keys.press(DOWN);
        keys.press_until(ENTER, on("GPU"));
        assert!(!keys.footer().contains("F2"));
        keys.wait_for("vm prices", || watched.stats_calls() == ["container", "vm"]);
        keys.type_text("4090");
        keys.press(ENTER);
    });
    let expected = Picks {
        repo: Some(Repo::NoRepo),
        mode: Some("vm".into()),
        gpu: Some("RTX 4090".into()),
        offer: None,
        tpu: None,
        cloud: None,
    };
    assert_eq!(result, Outcome::Done(expected));
    assert_eq!(
        *gpu_labels.lock().unwrap(),
        ["B200  $1.735/hr (1 offer)", "H100 SXM  -"]
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
            keys.press(ENTER);
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
fn test_gpu_search_finds_tpu_category_and_types_without_cloud_config() {
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
            keys.press(ENTER);
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
    let result = run(&flow, start.clone(), |keys| keys.press(ENTER));
    assert_eq!(
        result,
        Outcome::Done(Picks {
            gpu: Some("RTX 4090".into()),
            ..start.clone()
        })
    );
    let result = run(&flow, start.clone(), |keys| {
        keys.type_text("5090");
        keys.press(ENTER);
    });
    assert_eq!(
        result,
        Outcome::Done(Picks {
            gpu: Some("RTX 5090".into()),
            ..start
        })
    );
}
