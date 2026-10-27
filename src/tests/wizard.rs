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
use crate::wizard::{LaunchWizard, Picks, Search, WizardOptions};

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
        Next::Done(picks) => Outcome::Done(picks),
    }
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
        keys.wait(|s| s.labels == ["No repository", "Clone from URL...", "gpu-kernels", "other"]);
        keys.press_until(ENTER, on("Mode"));
        keys.press_until(ENTER, on("GPU"));
        keys.press_until(ENTER, on("Datacenter Blackwell/Hopper"));
        keys.press_until(ENTER, on("Offer: B200"));
        for title in ["Datacenter Blackwell/Hopper", "GPU", "Mode", "Repository"] {
            assert!(keys.press_until(BACK, on(title)) < quick());
        }
        // Forward again with different choices; earlier picks must not leak.
        keys.press_until("4", on("Mode"));
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
    };
    assert_eq!(result, Outcome::Done(expected));
    assert_eq!(repo_calls.load(Ordering::SeqCst), 1);
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
fn test_f2_prices_are_shared_by_gpu_screens_and_loaded_once_per_mode() {
    let t = TestEnv::new();
    let search = FakeSearch::new();
    let flow = wizard(&t, &search, &Arc::new(AtomicUsize::new(0)), &[], false);
    let gpu_labels = Arc::new(Mutex::new(Vec::new()));
    let (watched, labels) = (search.clone(), gpu_labels.clone());
    let result = run(&flow, Picks::default(), move |keys| {
        keys.wait(|s| s.labels.contains(&"gpu-kernels".to_string()));
        keys.press_until(ENTER, on("Mode"));
        keys.press_until(ENTER, on("GPU"));
        keys.press(F2);
        keys.wait_for("container prices", || {
            watched.stats_calls() == ["container"]
        });
        keys.wait(|s| s.labels.join(" ").contains("(1 model, from $1.735/hr)"));
        keys.press_until(ENTER, on("Datacenter Blackwell/Hopper"));
        labels.lock().unwrap().extend(keys.labels());
        keys.press(F2);
        keys.press_until(BACK, on("GPU"));
        keys.press_until(BACK, on("Mode"));
        keys.press(DOWN);
        keys.press_until(ENTER, on("GPU"));
        assert!(!keys.labels().join(" ").contains("from $")); // vm prices not loaded yet
        keys.press(F2);
        keys.wait_for("vm prices", || watched.stats_calls() == ["container", "vm"]);
        keys.type_text("4090");
        keys.press(ENTER);
    });
    let expected = Picks {
        repo: Some(Repo::NoRepo),
        mode: Some("vm".into()),
        gpu: Some("RTX 4090".into()),
        offer: None,
    };
    assert_eq!(result, Outcome::Done(expected));
    assert_eq!(
        *gpu_labels.lock().unwrap(),
        ["B200  $1.735/hr (1 offer)", "H100 SXM"]
    );
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
