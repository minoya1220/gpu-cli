//! TPU choices use the same numbered keyboard picker as Vast. Catalog calls
//! are Jobs and never execute on a screen transition or the picker thread.
use crate::cli::UpFlags;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::gcp::{self, Settings};
use crate::picker::{AnyJob, Job, JobState, Next, Opt, Screen};
use crate::pyjson::Dict;
use crate::repos::{resolve_repo, Repo};

#[derive(Clone)]
pub struct Picks {
    pub zone: String,
    pub tpu: String,
    pub runtime: String,
    pub spot: bool,
    pub repo: Repo,
    pub directory: String,
    pub deadline: jiff::Timestamp,
}

#[cfg(test)]
pub(crate) fn catalog_job(
    ctx: &Ctx,
    settings: &Settings,
    zone: Option<&str>,
    kind: &str,
    tpu: Option<&str>,
) -> Job<Vec<String>> {
    let worker = ctx.clone();
    let project = settings.project.clone();
    let zone = zone.map(String::from);
    let kind = kind.to_string();
    let tpu = tpu.map(String::from);
    Job::new(move || {
        let load = || -> Result<Vec<String>> {
            if kind == "locations" {
                return crate::gcp_catalog::zones(&worker, &project, tpu.as_deref())
                    .map(|zs| zs.into_iter().map(|z| z.name).collect());
            }
            crate::gcp_catalog::cached_types(
                &worker,
                &project,
                zone.as_deref().ok_or_else(gcp::bad_reply)?,
            )
        };
        load().map_err(|e| e.text().to_string())
    })
}

#[cfg(test)]
pub(crate) fn catalog_screen<R: 'static>(
    job: Job<Vec<String>>,
    title: &str,
    select: impl Fn(String) -> Next<String, R> + 'static,
) -> Screen<String, R> {
    catalog_screen_seeded(job, title, Vec::new(), select)
}
#[cfg(test)]
fn catalog_screen_seeded<R: 'static>(
    job: Job<Vec<String>>,
    title: &str,
    initial: Vec<String>,
    select: impl Fn(String) -> Next<String, R> + 'static,
) -> Screen<String, R> {
    let options = job.clone();
    Screen::new(
        title,
        move || {
            options
                .result()
                .unwrap_or_else(|| initial.clone())
                .into_iter()
                .map(|s| Opt::new(s.clone(), s))
                .collect()
        },
        select,
    )
    .load(job)
    .load_required(false)
    .wait_initial(false)
    .quick_select(false)
    .loading("Checking TPU catalog…")
    .empty("No matching TPU types or zones")
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Selection {
    pub zone: Option<String>,
    pub generation: Option<String>,
    pub tpu: Option<String>,
    pub spot: Option<bool>,
    pub expiry_at: Option<jiff::Timestamp>,
}

/// One screen stack for the TPU choices: back restores the previous selection
/// and its loaded catalog rather than quitting a separate picker.
#[cfg(test)]
pub(crate) fn next(ctx: Ctx, settings: Settings, picks: Selection) -> Next<String, Selection> {
    match crate::config::load_config(&ctx.env) {
        Ok(cfg) => next_with(ctx, settings, cfg, picks, None),
        Err(e) => Next::Notice(e.text().into()),
    }
}
fn next_with(
    ctx: Ctx,
    settings: Settings,
    cfg: Dict,
    picks: Selection,
    data: Option<Types>,
) -> Next<String, Selection> {
    if picks.tpu.is_none() {
        return Next::Screen(type_screen(ctx, settings, cfg, picks, data));
    }
    if picks.spot.is_none() {
        return Next::Screen(capacity_screen(ctx, settings, cfg, picks, data));
    }
    if picks.zone.is_none() {
        let tpu = picks.tpu.clone().expect("type chosen first");
        let spot = picks.spot.unwrap_or(false);
        let screen = priced_zone_screen(
            &ctx,
            &settings,
            &cfg,
            &tpu,
            spot,
            &std::collections::BTreeSet::new(),
            {
                let ctx = ctx.clone();
                let settings = settings.clone();
                let cfg = cfg.clone();
                move |zone| {
                    next_with(
                        ctx.clone(),
                        settings.clone(),
                        cfg.clone(),
                        Selection {
                            zone: Some(zone),
                            ..picks.clone()
                        },
                        None,
                    )
                }
            },
        );
        return Next::Screen(screen);
    }
    if picks.expiry_at.is_none() {
        return Next::Screen(
            Screen::new(
                "Destroy after (e.g. 2h)",
                Vec::new,
                move |duration: String| match crate::expiry::parse_deadline_duration(
                    &duration,
                    ctx.clock.now(),
                ) {
                    Ok(at) => next_with(
                        ctx.clone(),
                        settings.clone(),
                        cfg.clone(),
                        Selection {
                            expiry_at: Some(at),
                            ..picks.clone()
                        },
                        None,
                    ),
                    Err(message) => Next::Notice(message),
                },
            )
            .text_input(Some(Box::new(|s| {
                let seconds = crate::expiry::parse_duration(s)?;
                if !(60..=29 * 86400).contains(&seconds) {
                    return Err("Choose 1 minute to 29 days".into());
                }
                Ok(())
            })))
            .input_label("Duration")
            .input_hint("30m, 2h or 1d")
            .back_clears_input(false),
        );
    }
    Next::Done(picks)
}

pub(crate) fn capacity_screen(
    ctx: Ctx,
    settings: Settings,
    cfg: Dict,
    picks: Selection,
    data: Option<Types>,
) -> Screen<String, Selection> {
    // Carry the size screen's in-flight price job forward instead of starting
    // another read. Explicit --tpu launches enter here without that screen.
    let (rates, cached) = match data {
        Some(data) => (data.rates, data.cached),
        None => (
            crate::gcp_prices::job_at(&ctx, &cfg, picks.zone.as_deref()),
            picks
                .zone
                .as_deref()
                .and_then(|z| crate::gcp_prices::cached(&ctx, &cfg, z))
                .or_else(|| crate::gcp_prices::cached_default(&ctx, &cfg)),
        ),
    };
    let rows = rates.clone();
    let tpu = picks.tpu.clone().expect("type chosen first");
    let context = rates.clone();
    let context_cache = cached.clone();
    Screen::new(
        "TPU capacity",
        move || {
            let price = rows.result().or_else(|| cached.clone());
            let cells = [
                [
                    "On-demand".into(),
                    crate::gcp_prices::money(price.as_ref().and_then(|p| p.hourly(&tpu, false))),
                    String::new(),
                ],
                [
                    "Spot".into(),
                    crate::gcp_prices::money(price.as_ref().and_then(|p| p.hourly(&tpu, true))),
                    "(may be interrupted)".into(),
                ],
            ];
            crate::offers::aligned_rows(
                &cells,
                [
                    crate::offers::Column::Left,
                    crate::offers::Column::Left,
                    crate::offers::Column::Plain,
                ],
            )
            .into_iter()
            .zip(["on-demand", "spot"])
            .map(|(label, value)| Opt::new(value.into(), label.trim_end().to_string()))
            .collect()
        },
        move |value| {
            next_with(
                ctx.clone(),
                settings.clone(),
                cfg.clone(),
                Selection {
                    spot: Some(value == "spot"),
                    ..picks.clone()
                },
                None,
            )
        },
    )
    .prices(rates)
    .selection_note(move |_| {
        context
            .result()
            .or_else(|| context_cache.clone())
            .map(|p| format!("Estimates: {}  USD list {}", p.region, p.cache_note()))
            .unwrap_or_default()
    })
}

#[derive(Clone)]
pub(crate) struct Types {
    pub(crate) initial: Vec<String>,
    pub(crate) catalog: Job<Vec<String>>,
    pub(crate) rates: Job<std::sync::Arc<crate::gcp_prices::Prices>>,
    pub(crate) cached: Option<std::sync::Arc<crate::gcp_prices::Prices>>,
}
impl Types {
    fn names(&self) -> Vec<String> {
        let mut names = self
            .catalog
            .result()
            .map(|types| {
                types
                    .into_iter()
                    .map(crate::gcp_types::public)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| self.initial.clone());
        names.sort();
        names.dedup();
        names
    }
    fn price(&self) -> Option<std::sync::Arc<crate::gcp_prices::Prices>> {
        self.rates.result().or_else(|| self.cached.clone())
    }
    fn suffix(&self) -> &'static str {
        if self.rates.state() == JobState::Failed {
            "-"
        } else {
            "…"
        }
    }
}
fn type_screen(
    ctx: Ctx,
    settings: Settings,
    cfg: Dict,
    picks: Selection,
    data: Option<Types>,
) -> Screen<String, Selection> {
    let data = data.unwrap_or_else(|| {
        let initial = crate::gcp_facts::known_types(&ctx, &settings.project, None)
            .into_iter()
            .map(crate::gcp_types::public)
            .collect();
        Types {
            initial,
            catalog: crate::gcp_catalog::types_job(&ctx, &cfg),
            rates: crate::gcp_prices::job_at(&ctx, &cfg, picks.zone.as_deref()),
            cached: picks
                .zone
                .as_deref()
                .and_then(|z| crate::gcp_prices::cached(&ctx, &cfg, z))
                .or_else(|| crate::gcp_prices::cached_default(&ctx, &cfg)),
        }
    });
    let generation = picks.generation.clone();
    let size_step = generation.is_some();
    let title = generation
        .as_ref()
        .map(|g| format!("{g} size"))
        .unwrap_or_else(|| "TPU generation".into());
    let preferred = generation
        .as_deref()
        .and_then(|g| crate::gcp_preferences::accelerator(&ctx, &settings.project, g))
        .or_else(|| crate::gcp_preferences::generation(&ctx, &settings.project));
    let rows = data.clone();
    let group = generation.clone();
    let highlight = data.clone();
    let selected_group = generation.clone();
    let context = data.clone();
    let follow = data.clone();
    Screen::new(
        title,
        move || {
            let names = rows.names();
            let price = rows.price();
            let values = group
                .as_deref()
                .map(|g| crate::gcp_types::members(&names, g))
                .unwrap_or_else(|| crate::gcp_types::generations(&names));
            let cells = values
                .iter()
                .map(|value| {
                    let types = if size_step {
                        vec![value.clone()]
                    } else {
                        crate::gcp_types::members(&names, value)
                    };
                    let [demand, spot] = price
                        .as_ref()
                        .map(|p| {
                            p.rate_columns(
                                &types.iter().map(String::as_str).collect::<Vec<_>>(),
                                !size_step,
                            )
                        })
                        .unwrap_or_else(|| {
                            [rows.suffix().into(), format!("Spot {}", rows.suffix())]
                        });
                    [
                        if size_step {
                            crate::gcp_types::label(value)
                        } else {
                            value.clone()
                        },
                        demand,
                        spot,
                    ]
                })
                .collect::<Vec<_>>();
            let labels = crate::offers::aligned_rows(
                &cells,
                [
                    crate::offers::Column::Left,
                    crate::offers::Column::Left,
                    crate::offers::Column::Plain,
                ],
            );
            values
                .into_iter()
                .zip(labels)
                .map(|(value, label)| Opt::new(value, label))
                .collect()
        },
        move |value| {
            if size_step {
                next_with(
                    ctx.clone(),
                    settings.clone(),
                    cfg.clone(),
                    Selection {
                        tpu: Some(value),
                        ..picks.clone()
                    },
                    Some(follow.clone()),
                )
            } else {
                Next::Screen(type_screen(
                    ctx.clone(),
                    settings.clone(),
                    cfg.clone(),
                    Selection {
                        generation: Some(value),
                        ..picks.clone()
                    },
                    Some(follow.clone()),
                ))
            }
        },
    )
    .background(data.catalog)
    .prices(data.rates)
    .highlight(move || {
        let names = highlight.names();
        let values = selected_group
            .as_deref()
            .map(|g| crate::gcp_types::members(&names, g))
            .unwrap_or_else(|| crate::gcp_types::generations(&names));
        preferred
            .as_ref()
            .and_then(|p| values.iter().position(|v| v == p))
            .unwrap_or(0)
    })
    .selection_note(move |value| {
        let rate = context
            .price()
            .map(|p| format!("Estimates: {}  USD list {}", p.region, p.cache_note()))
            .unwrap_or_default();
        if size_step && rate.is_empty() {
            value.clone()
        } else if size_step {
            format!("{value}  {rate}")
        } else {
            rate
        }
    })
}

type ZoneRates = std::sync::Arc<
    std::sync::Mutex<
        std::collections::BTreeMap<
            String,
            std::result::Result<std::sync::Arc<crate::gcp_prices::Prices>, String>,
        >,
    >,
>;
pub(crate) fn priced_zone_screen<R: 'static>(
    ctx: &Ctx,
    settings: &Settings,
    cfg: &Dict,
    tpu: &str,
    spot: bool,
    excluded: &std::collections::BTreeSet<String>,
    select: impl Fn(String) -> Next<String, R> + 'static,
) -> Screen<String, R> {
    let mut initial = crate::gcp_catalog::known(ctx, &settings.project, Some(tpu));
    initial.retain(|z| !excluded.contains(&z.name));
    let worker = ctx.clone();
    let project = settings.project.clone();
    let filter = tpu.to_string();
    let skip = excluded.clone();
    let zones = Job::new(move || {
        crate::gcp_catalog::zones(&worker, &project, Some(&filter))
            .map(|mut zs| {
                zs.retain(|z| !skip.contains(&z.name));
                zs
            })
            .map_err(|e| e.text().into())
    });
    let rates: ZoneRates = Default::default();
    if let Some(p) = crate::gcp_prices::cached_default(ctx, cfg) {
        rates.lock().unwrap().insert(p.region.clone(), Ok(p));
    }
    // Fetch nearby regions first and only once per region, not once per zone.
    let mut regions = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for zone in &initial {
        if let Some(region) = crate::gcp_geo::region(&zone.name) {
            if seen.insert(region.to_string()) {
                regions.push((region.to_string(), zone.name.clone()));
            }
        }
    }
    let worker = ctx.clone();
    let config = cfg.clone();
    let kind = tpu.to_string();
    let shared = rates.clone();
    let prices = Job::progress(move |wake| {
        let mut error = None;
        let mut success = false;
        for (region, zone) in &regions {
            let rate = crate::gcp_prices::load_for(&worker, &config, zone, Some((&kind, spot)))
                .map(std::sync::Arc::new)
                .map_err(|e| e.text().to_string());
            if rate.is_ok() {
                success = true;
            } else {
                error = rate.as_ref().err().cloned();
            }
            shared
                .lock()
                .map_err(|_| "Price cache unavailable".to_string())?
                .insert(region.clone(), rate);
            wake.wake();
        }
        if success || regions.is_empty() {
            Ok(())
        } else {
            Err(error.unwrap_or_else(|| "Price unavailable".into()))
        }
    });
    let watch = prices.clone();
    let kind = tpu.to_string();
    crate::gcp_zones::present(
        zones,
        initial,
        "TPU zone",
        &format!(
            "{tpu} {}  USD list",
            if spot { "Spot" } else { "on-demand" }
        ),
        crate::gcp_zones::Presentation {
            preferred: crate::gcp_preferences::last(ctx, &settings.project)
                .or_else(|| settings.zone.clone()),
            fields: std::rc::Rc::new(move |z| {
                let rate = crate::gcp_geo::region(&z.name)
                    .and_then(|r| rates.lock().ok()?.get(r).cloned());
                match rate {
                    Some(Ok(p)) => {
                        let amount = p.hourly(&kind, spot);
                        [
                            crate::gcp_prices::money(amount),
                            if amount.is_some() {
                                p.cache_note()
                            } else {
                                String::new()
                            },
                        ]
                    }
                    Some(Err(_)) => ["-".into(), String::new()],
                    None => [
                        if matches!(watch.state(), JobState::Idle | JobState::Running) {
                            "…"
                        } else {
                            "-"
                        }
                        .into(),
                        String::new(),
                    ],
                }
            }),
            prices: Some(prices),
        },
        select,
    )
}

pub(crate) fn transition(
    ctx: &Ctx,
    args: &crate::launch::UpArgs,
    cfg: &Dict,
    deadline: Option<jiff::Timestamp>,
) -> crate::wizard::CloudTransition {
    let ctx = ctx.clone();
    let args = args.clone();
    let cfg = cfg.clone();
    std::rc::Rc::new(move |wizard, picks| {
        let settings = match gcp::launch_setup(&ctx, &cfg).and_then(|(settings, _)| {
            gcp::validate_tpu_flags(&gcp::picker_flags(&args, &picks))?;
            Ok(settings)
        }) {
            Ok(s) => s,
            Err(e) => return Next::Notice(e.text().into()),
        };
        let tpu = match &picks.tpu {
            Some(crate::wizard::TpuChoice::Type(t)) => Some(t.clone()),
            _ => None,
        };
        let selection = Selection {
            generation: tpu.as_ref().filter(|t| !t.contains('-')).cloned(),
            tpu: tpu.filter(|t| t.contains('-')),
            expiry_at: deadline,
            ..Selection::default()
        };
        let data = wizard.tpu_data();
        let finish = std::rc::Rc::new(move |selection: Selection| {
            wizard.next(crate::wizard::Picks {
                tpu: selection.tpu.clone().map(crate::wizard::TpuChoice::Type),
                cloud: Some(selection),
                ..picks.clone()
            })
        });
        let map = |screen: Screen<String, Selection>| {
            screen.remap(
                std::rc::Rc::new(crate::wizard::Choice::Cloud),
                std::rc::Rc::new(|value| match value {
                    crate::wizard::Choice::Cloud(s) | crate::wizard::Choice::Text(s) => s,
                    _ => unreachable!("mapped cloud value"),
                }),
                finish.clone(),
            )
        };
        let step = next_with(ctx.clone(), settings, cfg.clone(), selection, Some(data));
        match step {
            Next::Screen(s) => Next::Screen(map(s)),
            Next::Notice(m) => Next::Notice(m),
            Next::Done(s) => finish(s),
        }
    })
}

/// Google's common JAX/PyTorch images; install the chosen framework in the VM.
/// https://cloud.google.com/tpu/docs/runtimes#pytorch_and_jax
/// Never use a saved runtime: a later type selection can change generations.
pub(crate) fn runtime_for(tpu: &str) -> Result<&'static str> {
    match tpu.split_once('-').map(|(generation, _)| generation) {
        Some("v2" | "v3" | "v4") => Ok("tpu-ubuntu2204-base"),
        Some("v5e" | "v5litepod") => Ok("v2-alpha-tpuv5-lite"),
        Some("v5p") => Ok("v2-alpha-tpuv5"),
        Some("v6e") => Ok("v2-alpha-tpuv6e"),
        _ => Err(Error::msg(format!("Unknown runtime for {tpu}; supply --runtime. Next: gpu up --tpu {} --runtime <image> --expire 2h", crate::pyfmt::shell_quote(tpu)))),
    }
}

pub(crate) fn initial_selection(
    ctx: &Ctx,
    flags: &UpFlags,
    settings: &Settings,
    deadline: Option<jiff::Timestamp>,
) -> Selection {
    Selection {
        expiry_at: deadline,
        zone: flags.zone.clone().or_else(|| {
            if ctx.interactive() {
                None
            } else {
                crate::gcp_preferences::last(ctx, &settings.project)
                    .or_else(|| settings.zone.clone())
            }
        }),
        generation: flags.tpu.as_ref().filter(|s| !s.contains('-')).map(|g| {
            if g == "v5litepod" {
                "v5e".into()
            } else {
                g.clone()
            }
        }),
        tpu: flags.tpu.clone().filter(|s| s.contains('-')),
        spot: if ctx.interactive() && !flags.spot && !flags.on_demand {
            None
        } else {
            Some(flags.spot)
        },
    }
}

/// Resolve complete selections without a picker or dependency call. Launch
/// preflight Jobs use this after the repository/zone screens have completed.
pub(crate) fn complete(
    flags: &UpFlags,
    cfg: &Dict,
    selected: Selection,
    repo: Repo,
) -> Result<Picks> {
    let missing = || {
        Error::msg("Specify --tpu, --zone and --repo or --no-repo (zone may come from config). Next: gpu up --provider gcp --help")
    };
    let deadline = selected.expiry_at.ok_or_else(|| {
        Error::msg(
            "TPU launches require a destruction deadline. Next: gpu up --provider gcp --expire 2h",
        )
    })?;
    let zone = selected.zone.ok_or_else(missing)?;
    let tpu = selected.tpu.ok_or_else(missing)?;
    let spot = selected.spot.unwrap_or(false);
    // v5e's TPU API accelerator name predates its public product name.
    let tpu = tpu
        .strip_prefix("v5e-")
        .map(|size| format!("v5litepod-{size}"))
        .unwrap_or(tpu);
    let runtime = match &flags.runtime {
        Some(runtime) => runtime.clone(),
        None => runtime_for(&tpu)?.to_string(),
    };
    if ![&zone, &tpu, &runtime].iter().all(|s| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
    }) || !gcp::component(&zone)
    {
        return Err(Error::msg(
            "Invalid TPU type, zone or runtime. Next: gpu up --provider gcp --help",
        ));
    }
    let owner = crate::pyfmt::py_str(crate::config::get(cfg, "owner")?);
    let (_, name) = resolve_repo(repo.as_arg(), &owner)?;
    let directory = if repo == Repo::NoRepo {
        "/workspace".into()
    } else {
        format!("/workspace/{name}")
    };
    Ok(Picks {
        zone,
        tpu,
        runtime,
        spot,
        repo,
        directory,
        deadline,
    })
}

fn flag_repo(flags: &UpFlags) -> Option<Repo> {
    if flags.no_repo {
        Some(Repo::NoRepo)
    } else {
        flags.repo.as_deref().map(Repo::from_arg)
    }
}

fn repository_wizard(ctx: &Ctx, cfg: &Dict) -> Result<crate::wizard::LaunchWizard> {
    let owner = crate::pyfmt::py_str(crate::config::get(cfg, "owner")?);
    let worker = ctx.clone();
    let repo_owner = owner.clone();
    Ok(crate::wizard::LaunchWizard::new(
        &ctx.env,
        crate::wizard::WizardOptions {
            repos: Job::new(move || {
                crate::launch::sync_repos(&worker, &repo_owner).map_err(|e| e.text().into())
            }),
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
            owner,
        },
    ))
}

fn launch_next(
    ctx: &Ctx,
    settings: &Settings,
    cfg: &Dict,
    selected: Selection,
) -> Next<String, Selection> {
    let type_selected = selected.tpu.is_some();
    let duration_only = type_selected && selected.spot.is_some() && selected.zone.is_some();
    let zone = selected.zone.clone();
    let next = next_with(ctx.clone(), settings.clone(), cfg.clone(), selected, None);
    match next {
        Next::Screen(mut screen) if type_selected => {
            screen = screen.background(crate::gcp_catalog::types_job(ctx, cfg));
            if duration_only {
                screen = screen.prices(crate::gcp_prices::job_at(ctx, cfg, zone.as_deref()));
            }
            Next::Screen(screen)
        }
        other => other,
    }
}

pub fn choose(
    ctx: &Ctx,
    flags: &UpFlags,
    settings: &Settings,
    cfg: &Dict,
    deadline: Option<jiff::Timestamp>,
) -> Result<Picks> {
    let selected = initial_selection(ctx, flags, settings, deadline);
    let selected = if ctx.interactive() {
        crate::picker::resolve(ctx, launch_next(ctx, settings, cfg, selected))?
    } else {
        selected
    };
    // Preserve validation order before the optional repository picker.
    let draft = complete(flags, cfg, selected.clone(), Repo::NoRepo)?;
    let repo = match flag_repo(flags) {
        Some(repo) => repo,
        None if ctx.interactive() => repository_wizard(ctx, cfg)?.run(ctx,
            crate::wizard::Picks::new(None, Some("tpu"), Some(&draft.tpu)))?.repo.unwrap_or(Repo::NoRepo),
        None => return Err(Error::msg("Specify --tpu, --zone and --repo or --no-repo (zone may come from config). Next: gpu up --provider gcp --help")),
    };
    complete(flags, cfg, selected, repo)
}

/// Append repository selection and paid approval to the same TPU screen stack.
/// Left from approval therefore restores the actual zone/duration/repo page.
pub(crate) fn choose_confirmed(
    ctx: &Ctx,
    flags: &UpFlags,
    settings: &Settings,
    cfg: &Dict,
    deadline: Option<jiff::Timestamp>,
) -> Result<crate::gcp::Prepared> {
    use crate::wizard::Choice;
    use std::rc::Rc;
    let first = launch_next(
        ctx,
        settings,
        cfg,
        initial_selection(ctx, flags, settings, deadline),
    );
    let worker = ctx.clone();
    let flags = flags.clone();
    let cfg = cfg.clone();
    let step = crate::picker::map_next(
        first,
        Rc::new(Choice::Cloud),
        Rc::new(|choice| match choice {
            Choice::Cloud(value) | Choice::Text(value) => value,
            _ => unreachable!("mapped TPU value"),
        }),
        Rc::new(move |selection: Selection| {
            let wizard = match repository_wizard(&worker, &cfg) {
                Ok(wizard) => wizard,
                Err(e) => return Next::Notice(e.text().into()),
            };
            let repo_step = wizard.next(crate::wizard::Picks::new(
                flag_repo(&flags),
                Some("tpu"),
                Some("tpu"),
            ));
            let worker = worker.clone();
            let flags = flags.clone();
            let cfg = cfg.clone();
            crate::picker::then(repo_step, move |picks| {
                match complete(
                    &flags,
                    &cfg,
                    selection.clone(),
                    picks.repo.unwrap_or(Repo::NoRepo),
                ) {
                    Ok(picks) => Next::Screen(crate::gcp::confirmation(&worker, &cfg, picks)),
                    Err(e) => Next::Notice(e.text().into()),
                }
            })
        }),
    );
    crate::picker::resolve(ctx, step)
}
