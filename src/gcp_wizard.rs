//! TPU choices use the same numbered keyboard picker as Vast. Catalog calls
//! are Jobs and never execute on a screen transition or the picker thread.
use crate::cli::UpFlags;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::gcp::{self, Settings};
use crate::picker::{pick, Job, Next, Opt, Screen};
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

pub(crate) fn zone_job(
    ctx: &Ctx,
    settings: &Settings,
    tpu: Option<&str>,
) -> Job<Vec<crate::gcp_catalog::Zone>> {
    let worker = ctx.clone();
    let project = settings.project.clone();
    let tpu = tpu.map(String::from);
    Job::new(move || {
        crate::gcp_catalog::zones(&worker, &project, tpu.as_deref()).map_err(|e| e.text().into())
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
pub(crate) fn next(ctx: Ctx, settings: Settings, picks: Selection) -> Next<String, Selection> {
    let missing = if picks.zone.is_none() {
        Some(("locations", "TPU zone"))
    } else if picks.tpu.is_none() {
        Some(("accelerator-types", "TPU type"))
    } else {
        None
    };
    if let Some((kind, title)) = missing {
        if kind == "locations" {
            let job = zone_job(&ctx, &settings, picks.tpu.as_deref());
            let initial = crate::gcp_catalog::known(&ctx, &settings.project, picks.tpu.as_deref());
            return Next::Screen(crate::gcp_zones::screen(
                job,
                initial,
                title,
                "",
                move |zone| {
                    next(
                        ctx.clone(),
                        settings.clone(),
                        Selection {
                            zone: Some(zone),
                            ..picks.clone()
                        },
                    )
                },
            ));
        }
        let job = catalog_job(
            &ctx,
            &settings,
            picks.zone.as_deref(),
            kind,
            picks.tpu.as_deref(),
        );
        let initial = if kind == "locations" {
            crate::gcp_catalog::known(&ctx, &settings.project, picks.tpu.as_deref())
                .into_iter()
                .map(|z| z.name)
                .collect()
        } else {
            crate::gcp_facts::known_types(&ctx, &settings.project, picks.zone.as_deref())
        };
        return Next::Screen(catalog_screen_seeded(job, title, initial, move |value| {
            let mut chosen = picks.clone();
            match kind {
                "locations" => chosen.zone = Some(value),
                _ => chosen.tpu = Some(value),
            }
            next(ctx.clone(), settings.clone(), chosen)
        }));
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
                next(
                    ctx.clone(),
                    settings.clone(),
                    Selection {
                        spot: Some(value == "spot"),
                        ..picks.clone()
                    },
                )
            },
        ));
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
                    Ok(at) => next(
                        ctx.clone(),
                        settings.clone(),
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
                settings.zone.clone()
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
        match next(ctx.clone(), settings.clone(), selected) {
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
