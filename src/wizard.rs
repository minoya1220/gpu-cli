//! The keyboard-driven `gpu up` flow: repository, mode, GPU, and offer screens.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use crate::ctx::{Ctx, Env};
use crate::error::{Error, Result};
use crate::picker::{pick, AnyJob, Job, JobState, Next, Opt, PickValue, Screen};
use crate::pyjson::Dict;
use crate::repos::{load_cached_repos, resolve_repo, Repo};
use crate::table::{hardware_labels, HardwareColumns};
use crate::vast::offers::{
    category_columns, group_by_category, model_columns, offer_rows, OfferSearch, Stats, NO_OFFERS,
};

pub const MODES: [&str; 2] = ["container", "vm"];

pub const TPU_CATEGORY: &str = "TPU (Google Cloud)";

const MODAL_NOTE: &str =
    "Modal total compute: GPU + capped CPU + memory. Capacity unknown; GPU Sandboxes can be preempted.";

/// A failed job's error as a one-line, redacted selection note.
fn error_note(error: &str) -> String {
    crate::redact::redact_text(error)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

#[derive(Clone, Debug, PartialEq)]
pub enum TpuChoice {
    ChooseType,
    /// A generation shortcut, or a complete generation-size accelerator name.
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
    pub modal: Option<String>,
    pub(crate) cloud: Option<crate::gcp::wizard::TpuSelection>,
}

impl Picks {
    pub fn new(repo: Option<Repo>, mode: Option<&str>, gpu: Option<&str>) -> Picks {
        Picks {
            repo,
            mode: mode.map(String::from),
            gpu: gpu.map(String::from),
            ..Picks::default()
        }
    }

    /// Picks whose hardware is a TPU chosen outside this wizard; only the
    /// repository is still asked.
    pub(crate) fn for_tpu(repo: Option<Repo>, tpu: &str) -> Picks {
        Picks {
            repo,
            tpu: Some(TpuChoice::Type(tpu.into())),
            ..Picks::default()
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
    Cloud(String),
    Mode(String),
    Gpu(String),
    Modal(String),
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

pub type CloudTransition = Rc<dyn Fn(LaunchWizard, Picks) -> Next<Choice, Picks>>;

type WizardScreen = Screen<Choice, Picks>;

fn hardware_options(rows: Vec<(Choice, HardwareColumns)>, query: Option<&str>) -> Vec<Opt<Choice>> {
    let needle = query.map(crate::pyfmt::casefold);
    // Measure visible matches together, not hidden long model names. The picker
    // repeats the same casefolded match, preserving its normal filter semantics.
    let (values, cells): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .filter(|(_, cells)| {
            needle
                .as_ref()
                .is_none_or(|needle| crate::pyfmt::casefold(&cells.join("  ")).contains(needle))
        })
        .unzip();
    values
        .into_iter()
        .zip(hardware_labels(&cells))
        .map(|(value, label)| Opt::new(value, label))
        .collect()
}
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
    tpu: TpuOptions,
    /// `tpu.prices`, or a job that reports TPUs as not set up.
    tpu_prices: Job<Arc<crate::gcp::prices::Prices>>,
    prices: RefCell<HashMap<String, Job<Stats>>>,
    prefetch_attached: Cell<bool>,
    preferred_mode: String,
    modal: Option<ModalPrices>,
    modal_only: bool,
}

pub type ModalPrices = (Job<crate::modal::Rates>, i64, f64, f64);

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
    pub owner: String,
    pub tpu: TpuOptions,
}

impl Default for WizardOptions {
    fn default() -> Self {
        WizardOptions {
            repos: Job::new(|| Ok(Vec::new())),
            search: None,
            models: Vec::new(),
            recent_gpus: Vec::new(),
            choose_offer: false,
            owner: String::new(),
            tpu: TpuOptions::default(),
        }
    }
}

/// The Google Cloud TPU rows of the hardware screen.
#[derive(Default)]
pub struct TpuOptions {
    /// Why TPUs are unavailable. Local config preflight only; no cloud calls
    /// on a screen transition.
    pub setup_error: Option<String>,
    pub transition: Option<CloudTransition>,
    /// Prefetched from the first screen when present.
    pub prices: Option<Job<Arc<crate::gcp::prices::Prices>>>,
    pub cached_prices: Option<Arc<crate::gcp::prices::Prices>>,
    pub types: Vec<String>,
    pub catalog: Option<Job<Vec<String>>>,
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
                tpu_prices: options
                    .tpu
                    .prices
                    .clone()
                    .unwrap_or_else(|| Job::new(|| Err("needs gpu setup".into()))),
                tpu: options.tpu,
                prices: RefCell::new(HashMap::new()),
                prefetch_attached: Cell::new(false),
                modal: None,
                modal_only: false,
                preferred_mode: crate::history::recent_picks(env, "mode")
                    .into_iter()
                    .find(|m| MODES.contains(&m.as_str()))
                    .unwrap_or_else(|| MODES[0].into()),
            }),
        }
    }

    /// Only the run wizard opts into Modal; up's rows and defaults stay intact.
    /// `only` hides the Vast and TPU rows for an explicit `--provider modal`.
    pub fn with_modal(mut self, prices: Option<ModalPrices>, only: bool) -> Self {
        let state = Rc::get_mut(&mut self.state).expect("unshared wizard");
        state.modal_only = only && prices.is_some();
        state.modal = prices;
        self
    }

    pub fn run(&self, ctx: &Ctx, picks: Picks) -> Result<Picks> {
        match self.next(picks) {
            Next::Screen(first) => pick(ctx, first),
            Next::Notice(message) => Err(Error::msg(message)),
            Next::Done(picks) => Ok(picks),
        }
    }

    pub fn next(&self, picks: Picks) -> Step {
        let mode = picks
            .mode
            .clone()
            .unwrap_or_else(|| self.state.preferred_mode.clone());
        match self.next_step(picks) {
            Next::Screen(mut screen) if !self.state.prefetch_attached.replace(true) => {
                // Jobs need the picker's Runtime. Attach to its first screen,
                // not just GPU: reads overlap repository/mode input, never draw.
                if let Some((job, ..)) = &self.state.modal {
                    screen = screen.prices(job.clone());
                }
                if self.state.search.is_some() {
                    screen = screen.prices(self.price_job(&mode));
                }
                if self.state.tpu.setup_error.is_none() {
                    if self.state.tpu.prices.is_some() {
                        screen = screen.prices(self.state.tpu_prices.clone());
                    }
                    if let Some(catalog) = &self.state.tpu.catalog {
                        screen = screen.background(catalog.clone());
                    }
                }
                Next::Screen(screen)
            }
            step => step,
        }
    }

    fn next_step(&self, picks: Picks) -> Step {
        let s = &self.state;
        if picks.repo.is_none() {
            return Next::Screen(self.repo_screen(picks, String::new()));
        }
        // A TPU needs no Vast mode.
        if picks.mode.is_none() && picks.tpu.is_none() {
            return Next::Screen(self.mode_screen(picks));
        }
        if picks.gpu.is_none() && picks.tpu.is_none() && picks.modal.is_none() {
            return Next::Screen(self.gpu_screen(picks));
        }
        if picks.tpu.is_none() && picks.modal.is_none() && picks.offer.is_none() && s.choose_offer {
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
                return Err("Type a repository, or \u{2190} back to choose No repository".into());
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
            .input_hint("HTTPS/git@ URL or owner/name")
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

    fn tpu_types(&self) -> Vec<String> {
        if self.state.tpu.setup_error.is_some() {
            return Vec::new();
        }
        let mut types = self
            .state
            .tpu
            .catalog
            .as_ref()
            .and_then(Job::result)
            .unwrap_or_else(|| self.state.tpu.types.clone());
        types = types.into_iter().map(crate::gcp::types::public).collect();
        types.sort();
        types.dedup();
        types
    }
    pub(crate) fn tpu_data(&self) -> crate::gcp::wizard::Types {
        crate::gcp::wizard::Types {
            initial: self.tpu_types(),
            catalog: self.state.tpu.catalog.clone().unwrap_or_else(|| {
                let types = self.tpu_types();
                Job::new(move || Ok(types.clone()))
            }),
            rates: self.state.tpu_prices.clone(),
            cached: self.state.tpu.cached_prices.clone(),
        }
    }
    fn tpu_prices(&self) -> Option<Arc<crate::gcp::prices::Prices>> {
        if self.state.tpu.setup_error.is_some() {
            None
        } else {
            self.state
                .tpu_prices
                .result()
                .or_else(|| self.state.tpu.cached_prices.clone())
        }
    }
    fn tpu_rows(&self, shortcuts: Vec<String>) -> Vec<(Choice, HardwareColumns)> {
        let shortcuts = if self.state.tpu.setup_error.is_some() {
            Vec::new()
        } else {
            shortcuts
        };
        let values = std::iter::once(None)
            .chain(shortcuts.into_iter().map(Some))
            .collect::<Vec<_>>();
        let prices = self.tpu_prices();
        let types = self.tpu_types();
        values
            .into_iter()
            .map(|value| {
                let name = value
                    .as_ref()
                    .map(|t| format!("TPU {t}"))
                    .unwrap_or_else(|| TPU_CATEGORY.into());
                let [demand, spot] = if let Some(p) = &prices {
                    let (members, minimum) = match &value {
                        Some(t) if t.contains('-') => (vec![t.clone()], false),
                        Some(g) => (crate::gcp::types::members(&types, g), true),
                        None => (types.clone(), true),
                    };
                    p.rate_columns(
                        &members.iter().map(String::as_str).collect::<Vec<_>>(),
                        minimum,
                    )
                } else if self.state.tpu.setup_error.is_some() {
                    ["needs gpu setup".into(), "Spot -".into()]
                } else {
                    let value = self.state.tpu_prices.state().placeholder();
                    [value.into(), format!("Spot {value}")]
                };
                (
                    Choice::Tpu(value.map(TpuChoice::Type).unwrap_or(TpuChoice::ChooseType)),
                    [name, demand, spot, "-".into()],
                )
            })
            .collect()
    }
    fn modal_rows(&self) -> Vec<(Choice, HardwareColumns)> {
        let Some((job, count, cpu, memory)) = &self.state.modal else {
            return Vec::new();
        };
        let rates = job.result();
        crate::modal::GPUS
            .iter()
            .map(|(name, _, _)| {
                let price = rates
                    .as_ref()
                    .and_then(|r| r.price(name, *count, *cpu, *memory).ok())
                    .map(|p| format!("${}/hr", crate::pyfmt::fixed(p, 3)))
                    .unwrap_or_else(|| job.state().placeholder().into());
                (
                    Choice::Modal((*name).into()),
                    [format!("Modal {name}"), price, "Spot -".into(), "-".into()],
                )
            })
            .collect()
    }

    fn gpu_screen(&self, picks: Picks) -> WizardScreen {
        let prices = self.price_job(picks.mode.as_deref().unwrap_or(""));
        let this = self.clone();
        let opts = self.clone();
        let search = self.clone();
        let cloud = self.clone();
        let vast_context = prices.clone();
        let options = {
            let prices = prices.clone();
            move || {
                if opts.state.modal_only {
                    return hardware_options(opts.modal_rows(), None);
                }
                let stats = prices.result().unwrap_or_default();
                let status = prices.state();
                let pending = status.placeholder();
                let mut rows = Vec::new();
                for model in &opts.state.recent_gpus {
                    let mut cells = model_columns(model, &stats, pending);
                    cells[0] = format!("Recent: {model}");
                    rows.push((Choice::Gpu(model.clone()), cells));
                }
                for (category, models) in &opts.state.categories {
                    rows.push((
                        Choice::Category(category.clone()),
                        category_columns(category, models, &stats, pending),
                    ));
                }
                rows.extend(opts.tpu_rows(Vec::new()));
                rows.extend(opts.modal_rows());
                hardware_options(rows, None)
            }
        };
        let searching = {
            let prices = prices.clone();
            move |query: &str| {
                if search.state.modal_only {
                    return hardware_options(search.modal_rows(), Some(query));
                }
                let stats = prices.result().unwrap_or_default();
                let status = prices.state();
                let mut rows = search
                    .state
                    .models
                    .iter()
                    .map(|model| {
                        (
                            Choice::Gpu(model.clone()),
                            model_columns(model, &stats, status.placeholder()),
                        )
                    })
                    .collect::<Vec<_>>();

                let types = search.tpu_types();
                let values = if query.contains('-') {
                    types
                } else {
                    crate::gcp::types::generations(&types)
                };
                rows.extend(search.tpu_rows(values));
                rows.extend(search.modal_rows());
                hardware_options(rows, Some(query))
            }
        };
        let follow = picks.clone();
        let mut screen = Screen::new("GPU", options, move |choice| match choice {
            Choice::Modal(name) => this.next(Picks {
                modal: Some(name),
                ..follow.clone()
            }),
            Choice::Gpu(gpu) => this.next(Picks {
                gpu: Some(gpu),
                ..follow.clone()
            }),
            Choice::Tpu(tpu) => match &this.state.tpu.setup_error {
                Some(message) => Next::Notice(format!("{message}\nNo TPU created.")),
                None => {
                    let picks = Picks {
                        tpu: Some(tpu),
                        ..follow.clone()
                    };
                    if let Some(flow) = &this.state.tpu.transition {
                        flow(this.clone(), picks)
                    } else {
                        this.next(picks)
                    }
                }
            },
            Choice::Category(category) => {
                Next::Screen(this.category_screen(follow.clone(), category))
            }
            _ => unreachable!(),
        })
        .search_filtered(searching)
        .quick_select(false)
        .prices(prices)
        .prices(self.state.tpu_prices.clone())
        .selection_note(move |choice| {
            if matches!(choice, Choice::Modal(_)) {
                cloud
                    .state
                    .modal
                    .as_ref()
                    .map(|(job, ..)| {
                        if job.state() == JobState::Failed {
                            error_note(&job.error())
                        } else {
                            MODAL_NOTE.into()
                        }
                    })
                    .unwrap_or_default()
            } else if matches!(choice, Choice::Tpu(_)) {
                if let Some(prices) = cloud.tpu_prices() {
                    let cache = prices.cache_note();
                    format!(
                        "TPU prices: {}  USD list{}",
                        prices.region,
                        if cache.is_empty() {
                            String::new()
                        } else {
                            format!("  {cache}")
                        }
                    )
                } else if cloud.state.tpu.setup_error.is_some() {
                    String::new()
                } else {
                    error_note(&cloud.state.tpu_prices.error())
                }
            } else {
                error_note(&vast_context.error())
            }
        });
        if self.state.tpu.setup_error.is_none() {
            screen = screen.prices(self.state.tpu_prices.clone());
        }
        if let Some(job) = &self.state.tpu.catalog {
            screen = screen.background(job.clone());
        }
        screen
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
            let rows = members
                .iter()
                .map(|gpu| {
                    (
                        Choice::Gpu(gpu.clone()),
                        model_columns(gpu, &stats, stats_job.state().placeholder()),
                    )
                })
                .collect();
            hardware_options(rows, None)
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

    /// One shared price search per mode, including the launch-time prefetch.
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
