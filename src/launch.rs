//! `gpu up`: validate, pick, search offers, build the request, confirm,
//! create, arm expiry, open an editor.

use std::sync::Arc;

use jiff::Timestamp;

use crate::config;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::history::{recent_picks, record_picks_best_effort as record_picks};
use crate::picker::Job;
use crate::process::run;
use crate::prompt::{choose, choose_gpu, confirm, Choices, Pick};
use crate::pyfmt::{fixed, py_str};
use crate::pyjson::{self, Dict, Value};
use crate::repos::{resolve_repo, save_cached_repos, Repo};
use crate::vast::instance::expire_instance;
use crate::vast::offers::{
    gpu_stats, known_gpu_models, offer_label, offer_price, OfferSearch, NO_OFFERS,
};
use crate::vast::{argv, RequestArgs, Vast};
use crate::wizard::{LaunchWizard, Picks, TpuOptions, WizardOptions, MODES};

const CLONE_URL: &str = ":clone-url:";

/// What `gpu up` created, for the `--wait` that follows.
#[derive(Debug, Default)]
pub(crate) struct Launched {
    /// The created instance; `None` for a dry run or a cancelled launch.
    pub id: Option<String>,
    /// The managed SSH alias when SSH already answered during the launch.
    pub ready_alias: Option<String>,
}

/// The `gpu up` result fields every provider reports, in contract order,
/// before anything is created.
pub(crate) fn planned_result(
    ctx: &Ctx,
    provider: crate::provider::Provider,
    id: Option<&str>,
    rate: Option<f64>,
    deadline: Option<Timestamp>,
) {
    ctx.result("provider", Value::str(provider.key()));
    ctx.result("id", id.map_or(Value::Null, Value::str));
    ctx.result("ssh_alias", Value::Null);
    ctx.result("price_per_hour", rate.map_or(Value::Null, Value::Float));
    ctx.result(
        "expires_at",
        deadline.map_or(Value::Null, |at| Value::str(at.to_string())),
    );
    ctx.result("ready", Value::Bool(false));
    ctx.result("created", Value::Bool(false));
    ctx.result("expiry_armed", Value::Bool(false));
}

/// The `gpu up` flags.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UpArgs {
    pub repo: Option<Repo>,
    pub mode: Option<String>,
    pub gpu: Option<String>,
    pub num_gpus: i64,
    pub max_price: Option<f64>,
    pub interruptible: bool,
    pub offer: Option<i64>,
    pub disk: Option<i64>,
    pub yes: bool,
    pub dry_run: bool,
    pub expire: Option<String>,
    pub reliability: Option<f64>,
    pub unverified: bool,
    /// For a directory chosen in the picker, as `--dir` has them.
    pub max_upload: String,
}

impl From<&crate::cli::UpFlags> for UpArgs {
    fn from(flags: &crate::cli::UpFlags) -> UpArgs {
        UpArgs {
            repo: if flags.no_repo {
                Some(Repo::NoRepo)
            } else {
                flags.repo.as_deref().map(Repo::from_arg)
            },
            mode: flags.mode.clone(),
            gpu: flags.gpu.clone(),
            num_gpus: flags.num_gpus,
            max_price: flags.max_price,
            interruptible: flags.interruptible,
            offer: flags.offer,
            disk: flags.disk,
            yes: flags.yes,
            dry_run: flags.dry_run,
            expire: flags.expire.clone(),
            reliability: flags.reliability,
            unverified: flags.unverified,
            max_upload: flags.max_upload.clone(),
        }
    }
}

impl UpArgs {
    /// `--expire` as given, when set and non-empty.
    pub fn expire(&self) -> Option<&str> {
        self.expire.as_deref().filter(|e| !e.is_empty())
    }
}

/// `--max-price` is a finite, positive hourly price on every provider.
pub(crate) fn check_max_price(max_price: Option<f64>) -> Result<()> {
    if max_price.is_some_and(|p| !p.is_finite() || p <= 0.0) {
        return Err(Error::usage("--max-price must be greater than 0"));
    }
    Ok(())
}

pub fn validate_launch_args(args: &UpArgs) -> Result<()> {
    if args.num_gpus < 1 {
        return Err(Error::usage("--num-gpus must be at least 1"));
    }
    if args.disk.is_some_and(|d| d < 1) {
        return Err(Error::usage("--disk must be at least 1"));
    }
    check_max_price(args.max_price)?;
    if args.reliability.is_some_and(|r| !(0.0..=1.0).contains(&r)) {
        return Err(Error::usage("--reliability must be between 0 and 1"));
    }
    if args.offer.is_some_and(|o| o < 1) {
        return Err(Error::usage("--offer must be a positive ID"));
    }
    Ok(())
}

/// Repository names under `owner`. Blocks on the GitHub CLI.
pub fn github_repos(ctx: &Ctx, owner: &str) -> Result<Vec<String>> {
    github_names(&gh_repo_list(ctx, owner)?)
}

fn gh_repo_list(ctx: &Ctx, owner: &str) -> Result<String> {
    run(
        ctx,
        &[
            "gh", "repo", "list", owner, "--json", "name", "--limit", "1000",
        ],
        Some(60.0),
    )
}

fn github_names(output: &str) -> Result<Vec<String>> {
    let value = pyjson::loads(output)?;
    let items = match value {
        Value::List(items) => items,
        other => {
            return Err(Error::msg(format!(
                "GitHub returned {}; expected a repository list. Check: gh auth status",
                other.kind()
            )))
        }
    };
    items
        .iter()
        .map(|repo| match repo {
            Value::Dict(dict) => dict
                .get("name")
                .map(py_str)
                .ok_or_else(|| Error::key("name")),
            other => Err(Error::msg(format!(
                "GitHub returned a repository entry that is {}; expected an object. Check: gh auth status",
                other.kind()
            ))),
        })
        .collect()
}

pub fn sync_repos(ctx: &Ctx, owner: &str) -> Result<Vec<String>> {
    let names = github_repos(ctx, owner)?;
    save_cached_repos(&ctx.env, owner, &names)?;
    Ok(names)
}

/// Line-based prompts for the missing picks; also returns the chosen GPU's offers.
pub fn prompt_for_picks(
    ctx: &Ctx,
    picks: Picks,
    search: &dyn crate::wizard::Search,
    owner: &str,
) -> Result<(Picks, Vec<Dict>)> {
    let repo = match picks.repo {
        Some(repo) => repo,
        None => {
            let repos = github_repos_for_prompt(ctx, owner)?;
            let mut choices = vec![String::new(), CLONE_URL.to_string()];
            choices.extend(repos);
            let labels = choices
                .iter()
                .map(|r| {
                    if r == CLONE_URL {
                        "Clone from URL...".to_string()
                    } else {
                        Repo::from_arg(r).label().to_string()
                    }
                })
                .collect();
            let mut repo = choose(
                ctx,
                &Choices::labelled(choices, labels),
                "Repository",
                &Pick::REPO,
                None,
            )?;
            if repo == CLONE_URL {
                repo = ctx.input_or(
                    "Repository URL or owner/name: ",
                    Error::msg("Pass --repo URL or --no-repo"),
                )?;
                repo = repo.trim().to_string();
                if repo.is_empty() {
                    return Err(Error::msg(
                        "Repository URL is required; use --no-repo instead",
                    ));
                }
            }
            resolve_repo(&repo, owner)?;
            Repo::from_arg(&repo)
        }
    };
    let mode = match picks.mode {
        Some(mode) => mode,
        None => choose(
            ctx,
            &Choices::strings(MODES.map(String::from).to_vec()),
            "Mode",
            &Pick::MODE,
            None,
        )?,
    };
    // Without a GPU, search every model so the prompt can show real prices.
    let mut offers = search.offers(&mode, picks.gpu.as_deref())?;
    if offers.is_empty() {
        return Err(Error::status(crate::error::ExitCode::NoCapacity, NO_OFFERS));
    }
    let gpu = match picks.gpu {
        Some(gpu) => gpu,
        None => {
            let mut models = Vec::new();
            for offer in &offers {
                models.push(py_str(crate::vast::offers::field(offer, "gpu_name")?));
            }
            models.sort();
            models.dedup();
            let gpu = choose_gpu(ctx, &models, &gpu_stats(&offers)?)?;
            offers.retain(|o| o.get("gpu_name").map(py_str).as_deref() == Some(gpu.as_str()));
            gpu
        }
    };
    Ok((Picks::new(Some(repo), Some(&mode), Some(&gpu)), offers))
}

/// `github_repos` for the line prompt: CLI failures are reported, not fatal.
fn github_repos_for_prompt(ctx: &Ctx, owner: &str) -> Result<Vec<String>> {
    match gh_repo_list(ctx, owner) {
        Ok(output) => github_names(&output),
        Err(err) => {
            ctx.eprintln(&format!("Could not list repositories: {}", err.text()))?;
            Ok(Vec::new())
        }
    }
}

/// Offer chosen by `--offer`, `--yes` (cheapest), or a line-based prompt.
pub fn select_offer(ctx: &Ctx, offers: Vec<Dict>, args: &UpArgs) -> Result<Dict> {
    if offers.is_empty() {
        return Err(Error::status(crate::error::ExitCode::NoCapacity, NO_OFFERS));
    }
    let displayed: Vec<Dict> = offers.iter().take(10).cloned().collect();
    let mut labels = Vec::new();
    for offer in &displayed {
        labels.push(offer_label(offer)?);
    }
    if args.offer.is_some() || args.yes {
        ctx.println("Matching offers (hourly, reliability, location, verification):")?;
        for (n, label) in labels.iter().enumerate() {
            ctx.println(&format!("{}. {label}", n + 1))?;
        }
    }
    if let Some(wanted) = args.offer {
        let wanted_value = Value::Int(wanted);
        return offers
            .into_iter()
            .find(|o| o.get("id").is_some_and(|id| id.py_eq(&wanted_value)))
            .ok_or_else(|| {
                Error::msg(format!("Offer {wanted} does not match the current search"))
            });
    }
    if args.yes {
        return Ok(displayed[0].clone());
    }
    let choices = Choices {
        options: displayed,
        labels,
        key: &|_: &Dict| None,
    };
    choose(ctx, &choices, "Offer", &Pick::OFFER, None)
}

/// What Ctrl-C must say once a create request may have gone out: the
/// instance, when known, and when its boot-time guard destroys it.
fn interrupt_notice(ctx: &Ctx, id: Option<&str>, deadline: Option<Timestamp>) -> String {
    let when = deadline.map(|at| crate::expiry::local_time(ctx, at));
    match (id, when) {
        (Some(id), Some(when)) => format!(
            "Instance {id} keeps running; it self-destroys at {when}. Check: gpu expire {id} show. Stop billing now: gpu down {id}"
        ),
        (Some(id), None) => format!(
            "Instance {id} keeps running and billing, with no auto-destroy. Stop billing: gpu down {id}"
        ),
        (None, Some(when)) => format!(
            "Vast may have created an instance; if so, it self-destroys at {when}. Check: gpu ls"
        ),
        (None, None) => "Vast may have created an instance; if so, it keeps billing with no auto-destroy. Check: gpu ls, then gpu down ID".into(),
    }
}

#[derive(Clone)]
enum ApprovedLaunch {
    Vast(Box<(Picks, Dict)>),
    Gcp(
        Box<(
            crate::gcp::TpuArgs,
            crate::gcp::Prepared,
            Option<std::path::PathBuf>,
        )>,
    ),
}

fn confirmation(
    ctx: &Ctx,
    args: &UpArgs,
    cfg: &Dict,
    picks: Picks,
    deadline: Option<Timestamp>,
) -> crate::picker::Next<crate::wizard::Choice, ApprovedLaunch> {
    use crate::picker::Next;
    if picks.tpu.is_some() {
        let selection = match picks.cloud.clone() {
            Some(selection) => selection,
            None => return Next::Notice("Choose a TPU type and zone".into()),
        };
        let flags = match crate::gcp::TpuArgs::picked(args, &picks) {
            Ok(flags) => flags,
            Err(e) => return Next::Notice(e.text().into()),
        };
        let dir = picks.dir.clone();
        let prepared = crate::gcp::wizard::complete(
            &flags,
            cfg,
            selection,
            picks.repo.clone().unwrap_or(Repo::NoRepo),
        );
        return match prepared {
            Ok(picks) => Next::Screen(
                crate::gcp::confirmation::<crate::wizard::Choice>(ctx, cfg, picks).remap(
                    std::rc::Rc::new(|v| v),
                    std::rc::Rc::new(|v| v),
                    std::rc::Rc::new(move |prepared| {
                        Next::Done(ApprovedLaunch::Gcp(Box::new((
                            flags.clone(),
                            prepared,
                            dir.clone(),
                        ))))
                    }),
                ),
            ),
            Err(e) => Next::Notice(e.text().into()),
        };
    }
    let worker = ctx.clone();
    let args = args.clone();
    let cfg = cfg.clone();
    let job = Job::new(move || {
        let plan = || -> Result<_> {
            let mode = picks.mode.as_deref().unwrap_or("");
            let repo = picks.repo.clone().unwrap_or(Repo::NoRepo);
            let offer = match &picks.offer {
                Some(offer) => offer.clone(),
                None if args.offer.is_some() => {
                    let search = OfferSearch::new(Vast::new(&worker), &args, &cfg)?;
                    select_offer(&worker, search.offers(mode, picks.gpu.as_deref())?, &args)?
                }
                None => return Err(Error::msg("Choose an offer before confirming")),
            };
            let request = Vast::new(&worker).request(
                &offer,
                &RequestArgs {
                    repo: &repo,
                    mode,
                    interruptible: args.interruptible,
                    disk: args.disk,
                    deadline,
                },
                &cfg,
            )?;
            argv(&request)?;
            let gpu = offer
                .get("gpu_name")
                .map(py_str)
                .unwrap_or_else(|| picks.gpu.clone().unwrap_or_default());
            let workspace = if repo == Repo::NoRepo {
                String::new()
            } else {
                let owner = py_str(config::get(&cfg, "owner")?);
                format!(" for {}", resolve_repo(repo.as_arg(), &owner)?.1)
            };
            let title = format!(
                "Launch {gpu} {mode}{workspace} at {}?",
                crate::table::hourly(offer_price(&offer)?)
            );
            Ok(crate::prompt::Confirmation {
                title,
                value: ApprovedLaunch::Vast(Box::new((picks.clone(), offer))),
            })
        };
        plan().map_err(|e| e.text().to_string())
    });
    Next::Screen(crate::prompt::confirmation_screen(job))
}

pub(crate) fn launch(
    ctx: &Ctx,
    args: &UpArgs,
    cfg: &Dict,
    deadline: Option<Timestamp>,
) -> Result<Launched> {
    validate_launch_args(args)?;
    // Whole seconds, as the boot guard and the SSH timer hold it.
    let deadline = deadline
        .map(|at| Timestamp::from_second(crate::expiry::deadline_seconds(at)).unwrap_or(at));
    let owner = py_str(config::get(cfg, "owner")?);
    if let Some(repo) = &args.repo {
        resolve_repo(repo.as_arg(), &owner)?;
    }
    let vast = Vast::new(ctx);
    let search = OfferSearch::new(vast.clone(), args, cfg)?;
    let (picks, offer, tty_confirmed) = match pick(ctx, args, cfg, deadline, &owner, &search)? {
        Picked::Vast {
            picks,
            offer,
            confirmed,
        } => (*picks, offer, confirmed),
        Picked::Gcp(launched) => return Ok(launched),
    };
    let repo = picks.repo.clone().unwrap_or(Repo::NoRepo);
    let mode = picks.mode.clone().unwrap_or_default();
    let gpu = picks.gpu.clone().unwrap_or_default();
    let request_args = RequestArgs {
        repo: &repo,
        mode: &mode,
        interruptible: args.interruptible,
        disk: args.disk,
        deadline,
    };
    let request = vast.request(&offer, &request_args, cfg)?;
    let rate = if args.interruptible {
        crate::vast::offers::offer_number(
            "min_bid",
            crate::vast::offers::field(&offer, "min_bid")?,
        )?
    } else {
        offer_price(&offer)?
    };
    let rate = rate.is_finite().then_some(rate);
    crate::guardrails::spending(ctx, cfg, rate, deadline)?;
    planned_result(ctx, crate::provider::Provider::Vast, None, rate, deadline);
    if args.dry_run {
        ctx.result("request", Value::List(request.clone()));
        ctx.println("DRY RUN: no instance created")?;
        ctx.println(&pyjson::dumps(&Value::List(request), Some(2)))?;
        if let Some(expire) = args.expire() {
            ctx.println(&format!(
                "Auto-destroy requested after {expire}; no timer installed (dry run)."
            ))?;
        }
        return Ok(Launched::default());
    }
    if !tty_confirmed {
        ctx.println(&format!(
            "Launch {mode} for {} on offer {} at ${}/hr?",
            repo.label(),
            py_str(crate::vast::offers::field(&offer, "id")?),
            fixed(offer_price(&offer)?, 3)
        ))?;
        if !args.yes && !confirm(ctx, None)? {
            ctx.println("Cancelled.")?;
            return Ok(Launched::default());
        }
    }
    if deadline.is_some_and(|at| at <= ctx.clock.now()) {
        return Err(
            Error::msg("Expiry deadline passed; no instance created").next("gpu up --expire 2h")
        );
    }
    let (summary, instance_id) = create(ctx, &vast, &request, deadline)?;
    // Preserve the created ID and attempt expiry before propagating a failed announcement.
    let announced = ctx.println(&summary);
    if ctx.json() && instance_id.is_none() {
        return Err(Error::msg(
            "Create accepted but no instance ID returned; completion unverified",
        )
        .next("gpu ls"));
    }
    let ready_alias = arm_expiry(ctx, args, instance_id.as_deref(), deadline)?;
    announced?;
    record_picks(ctx, &[("mode", &mode), ("gpu", &gpu)]);
    if let Some(id) = &instance_id {
        crate::editors::offer_after_launch(ctx, cfg, &crate::provider::VastId(id.clone()))?;
    }
    Ok(Launched {
        id: instance_id,
        ready_alias,
    })
}

/// What `gpu up` launches: a Vast offer (`confirmed` when the TTY
/// confirmation screen already approved it), or a TPU launch that the
/// picker handed off and that has already run.
enum Picked {
    Vast {
        picks: Box<Picks>,
        offer: Dict,
        confirmed: bool,
    },
    Gcp(Launched),
}

/// Choose the hardware: the TTY wizard, or line prompts without a terminal.
fn pick(
    ctx: &Ctx,
    args: &UpArgs,
    cfg: &Dict,
    deadline: Option<Timestamp>,
    owner: &str,
    search: &OfferSearch,
) -> Result<Picked> {
    let picks = Picks::new(args.repo.clone(), args.mode.as_deref(), args.gpu.as_deref());
    if !ctx.interactive() {
        let (picks, offers) = prompt_for_picks(ctx, picks, search, owner)?;
        let offer = select_offer(ctx, offers, args)?;
        return Ok(Picked::Vast {
            picks: Box::new(picks),
            offer,
            confirmed: false,
        });
    }
    let wizard = launch_wizard(ctx, args, cfg, deadline, owner, search);
    if !args.yes && !args.dry_run {
        let worker = ctx.clone();
        let config = cfg.clone();
        let flags = args.clone();
        let step = crate::picker::then(wizard.next(picks), move |picks| {
            confirmation(&worker, &flags, &config, picks, deadline)
        });
        return Ok(match crate::picker::resolve(ctx, step)? {
            ApprovedLaunch::Vast(plan) => {
                let (picks, offer) = *plan;
                adopt_dir(ctx, args, picks.dir.as_deref())?;
                Picked::Vast {
                    picks: Box::new(picks),
                    offer,
                    confirmed: true,
                }
            }
            ApprovedLaunch::Gcp(plan) => {
                let (flags, prepared, dir) = *plan;
                adopt_dir(ctx, args, dir.as_deref())?;
                Picked::Gcp(crate::gcp::launch_prepared(ctx, &flags, cfg, prepared)?)
            }
        });
    }
    let picks = wizard.run(ctx, picks)?;
    adopt_dir(ctx, args, picks.dir.as_deref())?;
    if picks.tpu.is_some() {
        return Ok(Picked::Gcp(crate::gcp::launch_from_picker(
            ctx, args, &picks, cfg, deadline,
        )?));
    }
    let offer = match &picks.offer {
        Some(offer) => offer.clone(),
        None => {
            let mode = picks.mode.clone().unwrap_or_default();
            select_offer(ctx, search.offers(&mode, picks.gpu.as_deref())?, args)?
        }
    };
    Ok(Picked::Vast {
        picks: Box::new(picks),
        offer,
        confirmed: false,
    })
}

/// A directory chosen in the picker continues as `gpu up --dir PATH` does:
/// the same upload check and autosync question before anything is rented,
/// and `dispatch` then copies and links it after the launch.
fn adopt_dir(ctx: &Ctx, args: &UpArgs, dir: Option<&std::path::Path>) -> Result<()> {
    let Some(dir) = dir else {
        return Ok(());
    };
    if !args.dry_run {
        crate::dirsync::preflight(ctx, dir, &args.max_upload)?;
        if !args.yes {
            crate::dirsync::ask_autosync(ctx, dir)?;
        }
    }
    ctx.set_picked_dir(dir.to_path_buf());
    ctx.defer_editor_offer();
    Ok(())
}

/// The TTY launch wizard over Vast offers, Google Cloud TPUs and Modal GPUs.
fn launch_wizard(
    ctx: &Ctx,
    args: &UpArgs,
    cfg: &Dict,
    deadline: Option<Timestamp>,
    owner: &str,
    search: &OfferSearch,
) -> LaunchWizard {
    let repo_ctx = ctx.clone();
    let repo_owner = owner.to_string();
    let mut recent = recent_picks(&ctx.env, "gpu");
    let mut seen = std::collections::HashSet::new();
    recent.retain(|g| seen.insert(g.clone()));
    LaunchWizard::new(
        &ctx.env,
        WizardOptions {
            repos: Job::new(move || {
                sync_repos(&repo_ctx, &repo_owner).map_err(|e| e.text().to_string())
            }),
            search: Some(Arc::new(search.clone())),
            models: known_gpu_models(&ctx.env),
            recent_gpus: recent,
            choose_offer: !args.yes && args.offer.is_none(),
            owner: owner.to_string(),
            local_dir: true,
            tpu: TpuOptions {
                prices: Some(crate::gcp::prices::job(ctx, cfg)),
                cached_prices: crate::gcp::prices::cached_default(ctx, cfg),
                types: crate::gcp::facts::known_types(
                    ctx,
                    cfg.get("gcp")
                        .and_then(crate::pyjson::Value::as_dict)
                        .and_then(|d| crate::gcp::text(d, "project"))
                        .unwrap_or(""),
                    None,
                ),
                catalog: Some(crate::gcp::catalog::types_job(ctx, cfg)),
                transition: Some(crate::gcp::wizard::transition(ctx, args, cfg, deadline)),
                setup_error: crate::gcp::launch_setup(ctx, cfg)
                    .err()
                    .map(|e| e.text().to_string()),
            },
        },
    )
}

/// Send the create request. From here on an instance may exist, so Ctrl-C
/// says what remains. The reply summary, and the instance ID when known.
fn create(
    ctx: &Ctx,
    vast: &Vast,
    request: &[Value],
    deadline: Option<Timestamp>,
) -> Result<(String, Option<String>)> {
    ctx.result("created", Value::Null);
    ctx.set_interrupt_notice(Some(interrupt_notice(ctx, None, deadline)));
    let reply = vast.create(request)?;
    ctx.result(
        "created",
        reply
            .as_ref()
            .and_then(|d| d.get("success"))
            .filter(|v| matches!(v, Value::Bool(_)))
            .cloned()
            .unwrap_or(Value::Null),
    );
    let summary = crate::vast::created_summary(reply.as_ref())
        .inspect_err(|_| ctx.set_interrupt_notice(None))?;
    let instance_id = reply.as_ref().and_then(crate::vast::created_id);
    if let Some(id) = &instance_id {
        ctx.result("id", Value::str(id));
        ctx.set_interrupt_notice(Some(interrupt_notice(ctx, Some(id), deadline)));
    }
    Ok((summary, instance_id))
}

/// Arm the instance's own expiry when `--expire` asked for one; the managed
/// SSH alias once it answered.
fn arm_expiry(
    ctx: &Ctx,
    args: &UpArgs,
    instance_id: Option<&str>,
    deadline: Option<Timestamp>,
) -> Result<Option<String>> {
    if args.expire().is_none() {
        return Ok(None);
    }
    let guard = deadline.map(|at| {
        format!(
            " Its boot guard should still destroy it at {}.",
            crate::expiry::local_time(ctx, at)
        )
    });
    let Some(instance_id) = instance_id else {
        return Err(Error::msg(format!(
            "Instance created but no ID returned; expiry NOT verified.{}\nRun gpu ls, then gpu expire ID show.",
            guard.unwrap_or_default()
        )));
    };
    match expire_instance(ctx, instance_id, "arm", deadline) {
        Err(error) => {
            let text = error.text();
            let names_expire = error
                .next_step()
                .is_some_and(|step| step.starts_with(&format!("gpu expire {instance_id} ")));
            let next = if names_expire {
                format!("Or stop billing: gpu down {instance_id}.")
            } else {
                format!("Run gpu expire {instance_id} show or gpu down {instance_id}.")
            };
            let message = format!(
                "Instance {instance_id} created but expiry NOT verified; billing continues.{}\n{text}\n{next}",
                guard.unwrap_or_default()
            );
            Err(error.with_text(message))
        }
        Ok(alias) => {
            ctx.result("ssh_alias", Value::str(&alias));
            ctx.result("expiry_armed", Value::Bool(true));
            ctx.result("ready", Value::Bool(true));
            Ok(Some(alias))
        }
    }
}
