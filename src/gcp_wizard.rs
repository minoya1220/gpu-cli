//! TPU choices use the same numbered keyboard picker as Vast. Catalog calls
//! are Jobs and never execute on a screen transition or the picker thread.
use crate::cli::UpFlags;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::gcp::{self, Settings};
use crate::picker::{pick, Job, Next, Opt, Screen};
use crate::pyjson::Dict;
use crate::repos::{resolve_repo, Repo};

pub struct Picks {
    pub zone: String,
    pub tpu: String,
    pub runtime: String,
    pub spot: bool,
    pub repo: Repo,
    pub directory: String,
}

pub(crate) fn catalog_job(
    ctx: &Ctx,
    settings: &Settings,
    zone: Option<&str>,
    kind: &str,
) -> Job<Vec<String>> {
    let worker = ctx.clone();
    let project = settings.project.clone();
    let zone = zone.map(String::from);
    let kind = kind.to_string();
    Job::new(move || {
        let load = || -> Result<Vec<String>> {
            if kind == "locations" {
                return gcp::locations(&worker, &project);
            }
            let mut args = vec![
                "gcloud".into(),
                "compute".into(),
                "tpus".into(),
                "tpu-vm".into(),
                kind.clone(),
                "list".into(),
                format!("--project={project}"),
                "--quiet".into(),
            ];
            if let Some(zone) = &zone {
                args.push(format!("--zone={zone}"));
            }
            let items = gcp::objects(gcp::json(&worker, args, "gpu up --provider gcp --help")?)?;
            items
                .iter()
                .map(|d| {
                    gcp::text(
                        d,
                        if kind == "versions" {
                            "version"
                        } else {
                            "type"
                        },
                    )
                    .or_else(|| gcp::text(d, "name").and_then(|n| n.rsplit('/').next()))
                    .map(String::from)
                    .ok_or_else(gcp::bad_reply)
                })
                .collect()
        };
        load().map_err(|e| e.text().to_string())
    })
}

pub(crate) fn catalog_screen<R: 'static>(
    job: Job<Vec<String>>,
    title: &str,
    select: impl Fn(String) -> Next<String, R> + 'static,
) -> Screen<String, R> {
    let options = job.clone();
    Screen::new(
        title,
        move || {
            options
                .result()
                .unwrap_or_default()
                .into_iter()
                .map(|s| Opt::new(s.clone(), s))
                .collect()
        },
        select,
    )
    .load(job)
    .wait_initial(true)
    .quick_select(false)
    .loading("Loading Google Cloud catalog...")
    .empty("No matching TPU catalog entries")
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Selection {
    pub zone: Option<String>,
    pub tpu: Option<String>,
    pub runtime: Option<String>,
    pub spot: Option<bool>,
}

/// One screen stack for the TPU choices: back restores the previous selection
/// and its loaded catalog rather than quitting a separate picker.
pub(crate) fn next(ctx: Ctx, settings: Settings, picks: Selection) -> Next<String, Selection> {
    let missing = if picks.zone.is_none() {
        Some(("locations", "TPU zone"))
    } else if picks.tpu.is_none() {
        Some(("accelerator-types", "TPU type"))
    } else if picks.runtime.is_none() {
        Some(("versions", "TPU runtime image"))
    } else {
        None
    };
    if let Some((kind, title)) = missing {
        let job = catalog_job(&ctx, &settings, picks.zone.as_deref(), kind);
        return Next::Screen(catalog_screen(job, title, move |value| {
            let mut chosen = picks.clone();
            match kind {
                "locations" => chosen.zone = Some(value),
                "accelerator-types" => chosen.tpu = Some(value),
                _ => chosen.runtime = Some(value),
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
    Next::Done(picks)
}

pub fn choose(ctx: &Ctx, flags: &UpFlags, settings: &Settings, cfg: &Dict) -> Result<Picks> {
    let missing = || {
        Error::msg("Specify --tpu, --zone, --runtime and --repo or --no-repo (zone/runtime may come from config). Next: gpu up --provider gcp --help")
    };
    let selected = Selection {
        zone: flags.zone.clone().or_else(|| settings.zone.clone()),
        tpu: flags.tpu.clone(),
        runtime: flags.runtime.clone().or_else(|| settings.runtime.clone()),
        spot: if ctx.interactive() && !flags.spot && !flags.on_demand {
            None
        } else {
            Some(flags.spot)
        },
    };
    let selected = if ctx.interactive() {
        match next(ctx.clone(), settings.clone(), selected) {
            Next::Screen(first) => pick(ctx, first)?,
            Next::Done(done) => done,
        }
    } else {
        selected
    };
    let zone = selected.zone.ok_or_else(missing)?;
    let tpu = selected.tpu.ok_or_else(missing)?;
    let runtime = selected.runtime.ok_or_else(missing)?;
    let spot = selected.spot.unwrap_or(false);
    // v5e's TPU API accelerator name predates its public product name.
    let tpu = tpu
        .strip_prefix("v5e-")
        .map(|size| format!("v5litepod-{size}"))
        .unwrap_or(tpu);
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
    })
}
