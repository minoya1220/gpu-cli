//! The keyboard-driven `gpu up` flow: repository, mode, GPU, and offer screens.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::ctx::{Ctx, Env};
use crate::error::{Error, Result};
use crate::offers::{
    category_label, group_by_category, model_label, offer_rows, OfferSearch, Stats, NO_OFFERS,
};
use crate::picker::{pick, AnyJob, Job, JobState, Next, Opt, PickValue, Screen};
use crate::pyjson::Dict;
use crate::repos::{load_cached_repos, resolve_repo, Repo};

pub const MODES: [&str; 2] = ["container", "vm"];

/// Offline discovery hints, not a capacity/zone catalog. Selecting the category
/// loads Google's live catalog later; direct type matches carry the choice over.
pub const TPU_TYPES: [&str; 10] = [
    "v5e-1", "v5e-4", "v5e-8", "v6e-1", "v6e-4", "v6e-8", "v5p-8", "v4-8", "v3-8", "v2-8",
];
pub const TPU_CATEGORY: &str = "TPU (Google Cloud)";

#[derive(Clone, Debug, PartialEq)]
pub enum TpuChoice {
    ChooseType,
    Type(String),
}

/// Launch choices; `None` means not chosen yet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Picks {
    pub repo: Option<Repo>,
    pub mode: Option<String>,
    pub gpu: Option<String>,
    pub offer: Option<Dict>,
    pub tpu: Option<TpuChoice>,
}

impl Picks {
    pub fn new(repo: Option<Repo>, mode: Option<&str>, gpu: Option<&str>) -> Picks {
        Picks {
            repo,
            mode: mode.map(String::from),
            gpu: gpu.map(String::from),
            offer: None,
            tpu: None,
        }
    }
}

/// The option values of the launch flow.
#[derive(Clone, Debug, PartialEq)]
pub enum Choice {
    Repo(Repo),
    AllRepos,
    CloneUrl,
    Text(String),
    Mode(String),
    Gpu(String),
    Tpu(TpuChoice),
    Category(String),
    Offer(Dict),
}

impl PickValue for Choice {
    fn from_text(text: String) -> Self {
        Choice::Text(text)
    }
}

/// Offer queries the wizard needs; `OfferSearch` in the CLI, fakes in tests.
pub trait Search: Send + Sync {
    fn offers(&self, mode: &str, gpu: Option<&str>) -> Result<Vec<Dict>>;
    fn stats(&self, mode: &str) -> Result<Stats>;
}

impl Search for OfferSearch {
    fn offers(&self, mode: &str, gpu: Option<&str>) -> Result<Vec<Dict>> {
        OfferSearch::offers(self, mode, gpu)
    }

    fn stats(&self, mode: &str) -> Result<Stats> {
        OfferSearch::stats(self, mode)
    }
}

type WizardScreen = Screen<Choice, Picks>;
type Step = Next<Choice, Picks>;

struct State {
    repos: Job<Vec<String>>,
    owner: String,
    cached: Option<Vec<String>>,
    chosen_cached: RefCell<Option<String>>,
    entered_repo_screen: Cell<bool>,
    invalidated: Cell<bool>,
    selected_repo: Cell<bool>,
    search: Option<Arc<dyn Search>>,
    models: Vec<String>,
    categories: Vec<(String, Vec<String>)>,
    recent_gpus: Vec<String>,
    choose_offer: bool,
    tpu_setup_error: Option<String>,
    prices: RefCell<HashMap<String, Job<Stats>>>,
}

/// Builds the screens for whatever `Picks` are still missing.
///
/// Each screen's `select` returns the screen for the next missing pick, so
/// going back is just popping the picker's stack: nothing is refetched and
/// earlier choices stay intact.
#[derive(Clone)]
pub struct LaunchWizard {
    state: Rc<State>,
}

pub struct WizardOptions {
    pub repos: Job<Vec<String>>,
    pub search: Option<Arc<dyn Search>>,
    pub models: Vec<String>,
    pub recent_gpus: Vec<String>,
    pub choose_offer: bool,
    /// Local config preflight only; no cloud calls on a screen transition.
    pub tpu_setup_error: Option<String>,
    pub owner: String,
}

impl LaunchWizard {
    pub fn new(env: &Env, options: WizardOptions) -> LaunchWizard {
        let cached = load_cached_repos(env, &options.owner);
        let recent_gpus = options
            .recent_gpus
            .into_iter()
            .filter(|g| options.models.contains(g))
            .take(3)
            .collect();
        LaunchWizard {
            state: Rc::new(State {
                repos: options.repos,
                owner: options.owner,
                cached,
                chosen_cached: RefCell::new(None),
                entered_repo_screen: Cell::new(false),
                invalidated: Cell::new(false),
                selected_repo: Cell::new(false),
                search: options.search,
                categories: group_by_category(&options.models),
                models: options.models,
                recent_gpus,
                choose_offer: options.choose_offer,
                tpu_setup_error: options.tpu_setup_error,
                prices: RefCell::new(HashMap::new()),
            }),
        }
    }

    pub fn run(&self, ctx: &Ctx, picks: Picks) -> Result<Picks> {
        match self.next(picks) {
            Next::Screen(first) => pick(ctx, first),
            Next::Notice(message) => Err(Error::msg(message)),
            Next::Done(picks) => Ok(picks),
        }
    }

    pub fn next(&self, picks: Picks) -> Step {
        let s = &self.state;
        if picks.repo.is_none() {
            return Next::Screen(self.repo_screen(picks, String::new()));
        }
        if picks.mode.is_none() {
            return Next::Screen(self.mode_screen(picks));
        }
        if picks.gpu.is_none() && picks.tpu.is_none() {
            return Next::Screen(self.gpu_screen(picks));
        }
        if picks.tpu.is_none() && picks.offer.is_none() && s.choose_offer {
            return Next::Screen(self.offer_screen(picks));
        }
        if s.entered_repo_screen.get() {
            match s.repos.state() {
                JobState::Failed => {
                    *s.chosen_cached.borrow_mut() = None;
                    return Next::Screen(self.repo_screen(Picks::default(), s.repos.error()));
                }
                JobState::Done => {}
                _ => {
                    let (wizard, done) = (self.clone(), picks.clone());
                    let screen = Screen::new("Checking repositories", Vec::new, move |_| {
                        Next::Done(done.clone())
                    })
                    .load(s.repos.clone())
                    .loading("Checking repositories...")
                    .on_settled(move || {
                        wizard.sync_result().map(Next::Screen).or_else(|| {
                            if wizard.state.invalidated.get() {
                                None
                            } else {
                                Some(Next::Done(picks.clone()))
                            }
                        })
                    });
                    return Next::Screen(screen);
                }
            }
            let chosen = s.chosen_cached.borrow().clone();
            if let Some(name) = chosen {
                if !s.repos.result().unwrap_or_default().contains(&name) {
                    let notice = format!("Repository not available or deleted: {name}");
                    return Next::Screen(self.repo_screen(Picks::default(), notice));
                }
            }
        }
        Next::Done(picks)
    }

    fn sync_result(&self) -> Option<WizardScreen> {
        let s = &self.state;
        if !s.selected_repo.get() {
            return None;
        }
        match s.repos.state() {
            JobState::Failed => {
                *s.chosen_cached.borrow_mut() = None;
                s.invalidated.set(true);
                Some(self.repo_screen(Picks::default(), s.repos.error()))
            }
            JobState::Done => {
                let chosen = s.chosen_cached.borrow().clone();
                match chosen {
                    Some(name) if !s.repos.result().unwrap_or_default().contains(&name) => {
                        *s.chosen_cached.borrow_mut() = None;
                        s.invalidated.set(true);
                        Some(self.repo_screen(
                            Picks::default(),
                            format!("Repository not available or deleted: {name}"),
                        ))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn repo_options(&self, all: bool, searching: bool) -> Vec<Opt<Choice>> {
        let s = &self.state;
        let names = if s.repos.state() == JobState::Done {
            s.repos.result()
        } else {
            s.cached.clone()
        }
        .unwrap_or_default();
        let count = names.len();
        let mut options: Vec<_> = names
            .into_iter()
            .take(if all || searching { usize::MAX } else { 7 })
            .enumerate()
            .map(|(i, n)| {
                let option = Opt::new(Choice::Repo(Repo::Named(n.clone())), n);
                if all || searching {
                    option
                } else {
                    option.digit((i + 1) as u8)
                }
            })
            .collect();
        if !all {
            if count > 7 && !searching {
                options.push(Opt::new(Choice::AllRepos, "All repositories...").digit(8));
            }
            options.push(Opt::new(Choice::CloneUrl, "Clone from URL...").digit(9));
            options.push(Opt::new(Choice::Repo(Repo::NoRepo), "No repository").digit(0));
        }
        options
    }

    fn repo_screen(&self, picks: Picks, notice: String) -> WizardScreen {
        self.repo_list_screen(picks, notice, false)
    }

    fn repo_list_screen(&self, picks: Picks, notice: String, all: bool) -> WizardScreen {
        let s = &self.state;
        s.entered_repo_screen.set(true);
        let wizard = self.clone();
        let options = move || wizard.repo_options(all, false);
        let wizard = self.clone();
        let select = move |choice: Choice| -> Step {
            let s = &wizard.state;
            let repo = match choice {
                Choice::AllRepos => {
                    return Next::Screen(wizard.repo_list_screen(
                        picks.clone(),
                        String::new(),
                        true,
                    ))
                }
                Choice::CloneUrl => return Next::Screen(wizard.url_screen(picks.clone())),
                Choice::Repo(repo) => repo,
                _ => return Next::Done(picks.clone()),
            };
            s.invalidated.set(false);
            s.selected_repo.set(true);
            *s.chosen_cached.borrow_mut() = match &repo {
                Repo::Named(name) => Some(name.clone()),
                Repo::NoRepo => None,
            };
            wizard.next(Picks {
                repo: Some(repo),
                ..picks.clone()
            })
        };
        let wizard = self.clone();
        let search_wizard = self.clone();
        Screen::new(
            if all {
                "All repositories"
            } else {
                "Repository"
            },
            options,
            select,
        )
        .search(move || search_wizard.repo_options(all, true))
        .load(s.repos.clone())
        .load_required(false)
        .wait_initial(s.cached.is_none())
        .loading("Loading repositories...")
        .notice(notice)
        .on_settled(move || wizard.sync_result().map(Next::Screen))
    }

    fn url_screen(&self, picks: Picks) -> WizardScreen {
        let owner = self.state.owner.clone();
        let validate = move |value: &str| -> std::result::Result<(), String> {
            if value.is_empty() {
                return Err(
                    "Enter a repository, or `/\u{2190} back to choose No repository".into(),
                );
            }
            resolve_repo(value, &owner)
                .map(|_| ())
                .map_err(|e| e.text().to_string())
        };
        let wizard = self.clone();
        let select = move |choice: Choice| -> Step {
            let Choice::Text(text) = choice else {
                return Next::Done(picks.clone());
            };
            wizard.state.selected_repo.set(true);
            *wizard.state.chosen_cached.borrow_mut() = None;
            wizard.next(Picks {
                repo: Some(Repo::from_arg(&text)),
                ..picks.clone()
            })
        };
        Screen::new("Clone from URL", Vec::new, select)
            .text_input(Some(Box::new(validate)))
            .input_label("Repository")
            .input_hint("Type HTTPS/git@ URL or owner/name, then Enter/→")
            .quick_select(false)
    }

    fn mode_screen(&self, picks: Picks) -> WizardScreen {
        let wizard = self.clone();
        Screen::new(
            "Mode",
            || {
                MODES
                    .iter()
                    .map(|m| Opt::new(Choice::Mode(m.to_string()), *m))
                    .collect()
            },
            move |choice| match choice {
                Choice::Mode(mode) => wizard.next(Picks {
                    mode: Some(mode),
                    ..picks.clone()
                }),
                _ => Next::Done(picks.clone()),
            },
        )
    }

    fn gpu_screen(&self, picks: Picks) -> WizardScreen {
        let prices = self.price_job(picks.mode.as_deref().unwrap_or(""));
        let (wizard, stats_job) = (self.clone(), prices.clone());
        let options = move || {
            let stats = stats_job.result().unwrap_or_default();
            let s = &wizard.state;
            let mut options: Vec<Opt<Choice>> = s
                .recent_gpus
                .iter()
                .map(|gpu| {
                    Opt::new(
                        Choice::Gpu(gpu.clone()),
                        format!("Recent: {}", model_label(gpu, &stats)),
                    )
                })
                .collect();
            options.extend(s.categories.iter().map(|(name, members)| {
                Opt::new(
                    Choice::Category(name.clone()),
                    category_label(name, members, &stats),
                )
            }));
            options.push(Opt::new(Choice::Tpu(TpuChoice::ChooseType), TPU_CATEGORY));
            options
        };
        let (wizard, stats_job) = (self.clone(), prices.clone());
        let search = move || {
            let stats = stats_job.result().unwrap_or_default();
            let mut options: Vec<_> = wizard
                .state
                .models
                .iter()
                .map(|gpu| Opt::new(Choice::Gpu(gpu.clone()), model_label(gpu, &stats)))
                .collect();
            options.push(Opt::new(Choice::Tpu(TpuChoice::ChooseType), TPU_CATEGORY));
            options.extend(TPU_TYPES.iter().map(|tpu| {
                Opt::new(
                    Choice::Tpu(TpuChoice::Type((*tpu).into())),
                    format!("TPU {tpu} (Google Cloud)"),
                )
            }));
            options
        };
        let wizard = self.clone();
        let select = move |choice: Choice| match choice {
            Choice::Category(name) => Next::Screen(wizard.category_screen(picks.clone(), name)),
            Choice::Tpu(tpu) => match &wizard.state.tpu_setup_error {
                Some(message) => {
                    let hint = "No TPU created. Press \u{2190} or ` to return; Ctrl-C quits.";
                    Next::Notice(format!("{message}\n\n{hint}"))
                }
                None => wizard.next(Picks {
                    tpu: Some(tpu),
                    ..picks.clone()
                }),
            },
            Choice::Gpu(gpu) => wizard.next(Picks {
                gpu: Some(gpu),
                ..picks.clone()
            }),
            _ => Next::Done(picks.clone()),
        };
        Screen::new("GPU", options, select)
            .search(search)
            .prices(prices)
            // Digits belong to names such as "RTX 4090".
            .quick_select(false)
            .empty("No known GPU models")
    }

    fn category_screen(&self, picks: Picks, category: String) -> WizardScreen {
        let prices = self.price_job(picks.mode.as_deref().unwrap_or(""));
        let (wizard, stats_job, name) = (self.clone(), prices.clone(), category.clone());
        let options = move || {
            let stats = stats_job.result().unwrap_or_default();
            let members = wizard
                .state
                .categories
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, m)| m.clone())
                .unwrap_or_default();
            members
                .iter()
                .map(|gpu| Opt::new(Choice::Gpu(gpu.clone()), model_label(gpu, &stats)))
                .collect()
        };
        let wizard = self.clone();
        let select = move |choice: Choice| match choice {
            Choice::Gpu(gpu) => wizard.next(Picks {
                gpu: Some(gpu),
                ..picks.clone()
            }),
            _ => Next::Done(picks.clone()),
        };
        Screen::new(category, options, select)
            .prices(prices)
            .quick_select(false)
    }

    fn offer_screen(&self, picks: Picks) -> WizardScreen {
        // Offers are live market data, so every visit searches again.
        let gpu = picks.gpu.clone().unwrap_or_default();
        let mode = picks.mode.clone().unwrap_or_default();
        let search = self.state.search.clone();
        let query_gpu = gpu.clone();
        let offers: Job<Vec<(Dict, String)>> = Job::new(move || {
            let search = search.as_ref().ok_or("No offer search available")?;
            let found: Vec<Dict> = search
                .offers(&mode, Some(&query_gpu))
                .map_err(|e| e.text().to_string())?
                .into_iter()
                .take(10)
                .collect();
            let rows = offer_rows(&found).map_err(|e| e.text().to_string())?;
            Ok(found.into_iter().zip(rows).collect())
        });
        let rows_job = offers.clone();
        let options = move || {
            rows_job
                .result()
                .unwrap_or_default()
                .into_iter()
                .map(|(offer, row)| Opt::new(Choice::Offer(offer), row))
                .collect()
        };
        let wizard = self.clone();
        let select = move |choice: Choice| match choice {
            Choice::Offer(offer) => wizard.next(Picks {
                offer: Some(offer),
                ..picks.clone()
            }),
            _ => Next::Done(picks.clone()),
        };
        Screen::new(format!("Offer: {gpu}"), options, select)
            .load(offers)
            .loading(format!("Searching {gpu} offers..."))
            .empty(NO_OFFERS)
    }

    /// One shared price search per mode, so F2 on any GPU screen prices them all.
    fn price_job(&self, mode: &str) -> Job<Stats> {
        let mut prices = self.state.prices.borrow_mut();
        prices
            .entry(mode.to_string())
            .or_insert_with(|| {
                let search = self.state.search.clone();
                let mode = mode.to_string();
                Job::new(move || {
                    let search = search.as_ref().ok_or("No offer search available")?;
                    search.stats(&mode).map_err(|e| e.text().to_string())
                })
            })
            .clone()
    }
}
