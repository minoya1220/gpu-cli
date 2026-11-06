//! TPU choices use the same numbered keyboard picker as Vast. Catalog calls
//! are Jobs and never execute on a screen transition or the picker thread.
use crate::cli::UpFlags;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::gcp::{self, Settings};
use crate::picker::{pick, AnyJob, Job, JobState, Next, Opt, Screen};
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
    pub tpu: Option<String>,
    pub spot: Option<bool>,
    pub expiry_at: Option<jiff::Timestamp>,
}

/// One screen stack for the TPU choices: back restores the previous selection
/// and its loaded catalog rather than quitting a separate picker.
#[cfg(test)]
pub(crate) fn next(ctx: Ctx, settings: Settings, picks: Selection) -> Next<String, Selection> {
    match crate::config::load_config(&ctx.env) {
        Ok(cfg) => next_with(ctx, settings, cfg, picks),
        Err(e) => Next::Notice(e.text().into()),
    }
}
fn next_with(ctx: Ctx, settings: Settings, cfg: Dict, picks: Selection) -> Next<String, Selection> {
    if picks.tpu.is_none() {
        let mut initial = crate::gcp_facts::known_types(&ctx, &settings.project, None);
        initial.extend(crate::wizard::TPU_TYPES.iter().map(|s| s.to_string()));
        initial = initial
            .into_iter()
            .map(|t| {
                t.strip_prefix("v5litepod-")
                    .map(|n| format!("v5e-{n}"))
                    .unwrap_or(t)
            })
            .collect();
        initial.sort();
        initial.dedup();
        order_types(&mut initial);
        let catalog = crate::gcp_catalog::types_job(&ctx, &cfg);
        let live = catalog.clone();
        let prices = crate::gcp_prices::job_at(&ctx, &cfg, picks.zone.as_deref());
        let rates = prices.clone();
        let cached = picks
            .zone
            .as_deref()
            .and_then(|z| crate::gcp_prices::cached(&ctx, &cfg, z))
            .or_else(|| crate::gcp_prices::cached_default(&ctx, &cfg));
        let context = rates.clone();
        let fallback = cached.clone();
        return Next::Screen(
            Screen::new(
                "TPU type",
                move || {
                    let mut types = initial.clone();
                    types.extend(live.result().unwrap_or_default().into_iter().map(|t| {
                        t.strip_prefix("v5litepod-")
                            .map(|n| format!("v5e-{n}"))
                            .unwrap_or(t)
                    }));
                    types.sort();
                    types.dedup();
                    order_types(&mut types);
                    let rate = rates.result().or_else(|| cached.clone());
                    types
                        .into_iter()
                        .map(|t| {
                            let label = rate.as_ref().map(|p| p.label(&t)).unwrap_or_else(|| {
                                format!(
                                    "{t} · {}",
                                    if rates.state() == JobState::Failed {
                                        "price unavailable"
                                    } else {
                                        "…"
                                    }
                                )
                            });
                            Opt::new(t, label)
                        })
                        .collect()
                },
                move |tpu| {
                    next_with(
                        ctx.clone(),
                        settings.clone(),
                        cfg.clone(),
                        Selection {
                            tpu: Some(tpu),
                            ..picks.clone()
                        },
                    )
                },
            )
            .background(catalog)
            .prices(prices)
            .quick_select(false)
            .selection_note(move |_| {
                context
                    .result()
                    .or_else(|| fallback.clone())
                    .map(|p| format!("Estimates: {} · USD list {}", p.region, p.cache_note()))
                    .unwrap_or_default()
            }),
        );
    }
    if picks.spot.is_none() {
        return Next::Screen(Screen::new(
            "TPU capacity",
            || {
                vec![
                    Opt::new("on-demand".to_string(), "On-demand"),
                    Opt::new("spot".to_string(), "Spot — may be interrupted"),
                ]
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
                )
            },
        ));
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
            .input_hint("30m, 2h or 1d · Enter/→ continue")
            .back_clears_input(false),
        );
    }
    Next::Done(picks)
}

fn order_types(types: &mut [String]) {
    types.sort_by_key(|t| {
        (
            crate::wizard::TPU_TYPES
                .iter()
                .position(|known| *known == t)
                .unwrap_or(usize::MAX),
            t.split_once('-')
                .map(|(g, _)| g.to_string())
                .unwrap_or_default(),
            t.rsplit_once('-')
                .and_then(|(_, n)| n.parse::<u32>().ok())
                .unwrap_or(u32::MAX),
        )
    });
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
            "{tpu} {} · USD list",
            if spot { "Spot" } else { "on-demand" }
        ),
        crate::gcp_zones::Presentation {
            preferred: crate::gcp_preferences::last(ctx, &settings.project)
                .or_else(|| settings.zone.clone()),
            label: std::rc::Rc::new(move |z| {
                let rate = crate::gcp_geo::region(&z.name)
                    .and_then(|r| rates.lock().ok()?.get(r).cloned());
                let quote = match rate {
                    Some(Ok(p)) => p.quote(&kind, spot),
                    Some(Err(_)) => "price unavailable".into(),
                    None => {
                        if matches!(watch.state(), JobState::Idle | JobState::Running) {
                            "…".into()
                        } else {
                            "price unavailable".into()
                        }
                    }
                };
                format!("{} · {quote}", z.name)
            }),
            prices: Some(prices),
        },
        select,
    )
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

pub fn choose(
    ctx: &Ctx,
    flags: &UpFlags,
    settings: &Settings,
    cfg: &Dict,
    deadline: Option<jiff::Timestamp>,
) -> Result<Picks> {
    let missing = || {
        Error::msg("Specify --tpu, --zone and --repo or --no-repo (zone may come from config). Next: gpu up --provider gcp --help")
    };
    let selected = Selection {
        expiry_at: deadline,
        zone: flags.zone.clone().or_else(|| {
            if ctx.interactive() {
                None
            } else {
                crate::gcp_preferences::last(ctx, &settings.project)
                    .or_else(|| settings.zone.clone())
            }
        }),
        tpu: flags.tpu.clone(),
        spot: if ctx.interactive() && !flags.spot && !flags.on_demand {
            None
        } else {
            Some(flags.spot)
        },
    };
    let selected = if ctx.interactive() {
        match next_with(ctx.clone(), settings.clone(), cfg.clone(), selected) {
            Next::Screen(first) => pick(ctx, first)?,
            Next::Notice(message) => return Err(Error::msg(message)),
            Next::Done(done) => done,
        }
    } else {
        selected
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
    let repo = if flags.no_repo {
        Repo::NoRepo
    } else if let Some(repo) = &flags.repo {
        Repo::from_arg(repo)
    } else if ctx.interactive() {
        let worker = ctx.clone();
        let repo_owner = owner.clone();
        let wizard = crate::wizard::LaunchWizard::new(
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
                tpu_prices: None,
                tpu_cached_prices: None,
                tpu_types: Vec::new(),
                tpu_catalog: None,
                owner: owner.clone(),
            },
        );
        wizard
            .run(
                ctx,
                crate::wizard::Picks::new(None, Some("tpu"), Some(&tpu)),
            )?
            .repo
            .unwrap_or(Repo::NoRepo)
    } else {
        return Err(missing());
    };
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
