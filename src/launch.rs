//! `gpu up`: validate, pick, search offers, build the request, confirm,
//! create, arm expiry, open an editor.

use std::sync::Arc;

use jiff::Timestamp;

use crate::commands::{edit_instance, expire_instance, last_editor};
use crate::config;
use crate::ctx::Ctx;
use crate::editors::after_up_screen;
use crate::error::{Error, Result};
use crate::history::{recent_picks, record_picks};
use crate::offers::{
    gpu_stats, known_gpu_models, offer_label, offer_price, OfferSearch, NO_OFFERS,
};
use crate::picker::{pick, Job};
use crate::process::run;
use crate::prompt::{choose, choose_gpu, confirm, Choices};
use crate::pyfmt::{fixed, py_str, truthy};
use crate::pyjson::{self, Dict, Value};
use crate::redact::redact_text;
use crate::repos::{resolve_repo, save_cached_repos, Repo};
use crate::vast::{argv, RequestArgs, Vast};
use crate::wizard::{LaunchWizard, Picks, WizardOptions, MODES};

const CLONE_URL: &str = ":clone-url:";

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
}

impl UpArgs {
    /// `--expire` as given, when set and non-empty.
    pub fn expire(&self) -> Option<&str> {
        self.expire.as_deref().filter(|e| !e.is_empty())
    }
}

pub fn validate_launch_args(args: &UpArgs) -> Result<()> {
    if args.num_gpus < 1 {
        return Err(Error::msg("--num-gpus must be at least 1"));
    }
    if args.disk.is_some_and(|d| d < 1) {
        return Err(Error::msg("--disk must be at least 1"));
    }
    if args.max_price.is_some_and(|p| !p.is_finite() || p <= 0.0) {
        return Err(Error::msg("--max-price must be greater than 0"));
    }
    if args.reliability.is_some_and(|r| !(0.0..=1.0).contains(&r)) {
        return Err(Error::msg("--reliability must be between 0 and 1"));
    }
    if args.offer.is_some_and(|o| o < 1) {
        return Err(Error::msg("--offer must be a positive ID"));
    }
    Ok(())
}

/// Repository names under `owner`. Blocks on the GitHub CLI.
pub fn github_repos(ctx: &Ctx, owner: &str) -> Result<Vec<String>> {
    let output = run(
        ctx,
        &[
            "gh", "repo", "list", owner, "--json", "name", "--limit", "1000",
        ],
        None,
    )?;
    github_names(&output)
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
                None,
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
            Some("mode"),
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
                models.push(py_str(crate::offers::field(offer, "gpu_name")?));
            }
            models.sort();
            models.dedup();
            let gpu = choose_gpu(ctx, &models, &gpu_stats(&offers)?)?;
            offers.retain(|o| o.get("gpu_name").map(py_str).as_deref() == Some(gpu.as_str()));
            gpu
        }
    };
    Ok((
        Picks {
            repo: Some(repo),
            mode: Some(mode),
            gpu: Some(gpu),
            offer: None,
            tpu: None,
            cloud: None,
        },
        offers,
    ))
}

/// `github_repos` for the line prompt: CLI failures are reported, not fatal.
fn github_repos_for_prompt(ctx: &Ctx, owner: &str) -> Result<Vec<String>> {
    match run(
        ctx,
        &[
            "gh", "repo", "list", owner, "--json", "name", "--limit", "1000",
        ],
        None,
    ) {
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
    choose(ctx, &choices, "Offer", None, None)
}

fn created_id(response: &Dict) -> Option<String> {
    response.get("new_contract").map(py_str).filter(|id| {
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) && id.bytes().any(|b| b != b'0')
    })
}

/// Summarize `vastai create instance --raw` without echoing its secrets
/// (`instance_api_key`).
pub fn created_summary(output: &str) -> Result<String> {
    let response = match pyjson::loads(output) {
        Ok(Value::Dict(dict)) => dict,
        _ => {
            return Ok("Vast accepted the create request but its response was unreadable; run `gpu ls` to find the instance."
                .into())
        }
    };
    if !response.get("success").is_some_and(truthy) {
        let reason = ["msg", "error"]
            .iter()
            .filter_map(|k| response.get(k))
            .find(|v| truthy(v))
            .map(py_str)
            .unwrap_or_else(|| "unknown error".into());
        if let Some(error) = crate::provider_errors::vast(&reason) {
            return Err(error);
        }
        return Err(Error::msg(format!(
            "Vast did not create the instance: {}",
            redact_text(&reason)
        )));
    }
    match created_id(&response) {
        None => Ok("Created instance. Next: `gpu ls` to find it.".into()),
        Some(instance) => Ok(format!(
            "Created instance {instance}.\nConnect: gpu ssh {instance} --exec"
        )),
    }
}

#[derive(Clone)]
enum ApprovedLaunch {
    Vast(Box<(Picks, Dict)>),
    Gcp(Box<(crate::cli::UpFlags, crate::gcp::Prepared)>),
}

fn confirmation(
    ctx: &Ctx,
    args: &UpArgs,
    cfg: &Dict,
    picks: Picks,
) -> crate::picker::Next<crate::wizard::Choice, ApprovedLaunch> {
    use crate::picker::Next;
    if picks.tpu.is_some() {
        let flags = crate::gcp::picker_flags(args, &picks);
        let selection = match picks.cloud.clone() {
            Some(selection) => selection,
            None => return Next::Notice("Choose a TPU type and zone".into()),
        };
        let prepared = crate::gcp::validate_tpu_flags(&flags).and_then(|_| {
            crate::gcp_wizard::complete(
                &flags,
                cfg,
                selection,
                picks.repo.clone().unwrap_or(Repo::NoRepo),
            )
        });
        return match prepared {
            Ok(picks) => Next::Screen(
                crate::gcp::confirmation::<crate::wizard::Choice>(ctx, cfg, picks).remap(
                    std::rc::Rc::new(|v| v),
                    std::rc::Rc::new(|v| v),
                    std::rc::Rc::new(move |prepared| {
                        Next::Done(ApprovedLaunch::Gcp(Box::new((flags.clone(), prepared))))
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
                "Launch {gpu} {mode}{workspace} at ${}/hr?",
                fixed(offer_price(&offer)?, 2)
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

pub fn launch(ctx: &Ctx, args: &UpArgs, cfg: &Dict, deadline: Option<Timestamp>) -> Result<()> {
    validate_launch_args(args)?;
    let owner = py_str(config::get(cfg, "owner")?);
    if let Some(repo) = &args.repo {
        resolve_repo(repo.as_arg(), &owner)?;
    }
    let vast = Vast::new(ctx);
    let search = OfferSearch::new(vast.clone(), args, cfg)?;
    let picks = Picks {
        repo: args.repo.clone(),
        mode: args.mode.clone(),
        gpu: args.gpu.clone(),
        offer: None,
        tpu: None,
        cloud: None,
    };
    let mut tty_confirmed = false;
    let (picks, offer) = if ctx.interactive() {
        let repo_ctx = ctx.clone();
        let repo_owner = owner.clone();
        let mut recent = recent_picks(&ctx.env, "gpu");
        let mut seen = std::collections::HashSet::new();
        recent.retain(|g| seen.insert(g.clone()));
        let wizard = LaunchWizard::new(
            &ctx.env,
            WizardOptions {
                repos: Job::new(move || {
                    sync_repos(&repo_ctx, &repo_owner).map_err(|e| e.text().to_string())
                }),
                search: Some(Arc::new(search.clone())),
                models: known_gpu_models(&ctx.env),
                recent_gpus: recent,
                choose_offer: !args.yes && args.offer.is_none(),
                tpu_prices: Some(crate::gcp_prices::job(ctx, cfg)),
                tpu_cached_prices: crate::gcp_prices::cached_default(ctx, cfg),
                tpu_types: crate::gcp_facts::known_types(
                    ctx,
                    cfg.get("gcp")
                        .and_then(crate::pyjson::Value::as_dict)
                        .and_then(|d| crate::gcp::text(d, "project"))
                        .unwrap_or(""),
                    None,
                ),
                tpu_catalog: Some(crate::gcp_catalog::types_job(ctx, cfg)),
                tpu_transition: Some(crate::gcp_wizard::transition(ctx, args, cfg, deadline)),
                tpu_setup_error: crate::gcp::launch_setup(ctx, cfg)
                    .err()
                    .map(|e| e.text().to_string()),
                owner: owner.clone(),
            },
        );
        if !args.yes && !args.dry_run {
            let worker = ctx.clone();
            let config = cfg.clone();
            let flags = args.clone();
            let step = crate::picker::then(wizard.next(picks), move |picks| {
                confirmation(&worker, &flags, &config, picks)
            });
            match crate::picker::resolve(ctx, step)? {
                ApprovedLaunch::Vast(plan) => {
                    tty_confirmed = true;
                    *plan
                }
                ApprovedLaunch::Gcp(plan) => {
                    let (flags, prepared) = *plan;
                    return crate::gcp::launch_prepared(ctx, &flags, cfg, prepared);
                }
            }
        } else {
            let picks = wizard.run(ctx, picks)?;
            if picks.tpu.is_some() {
                return crate::gcp::launch_from_picker(ctx, args, &picks, cfg, deadline);
            }
            let offer = match &picks.offer {
                Some(offer) => offer.clone(),
                None => {
                    let mode = picks.mode.clone().unwrap_or_default();
                    select_offer(ctx, search.offers(&mode, picks.gpu.as_deref())?, args)?
                }
            };
            (picks, offer)
        }
    } else {
        let (picks, offers) = prompt_for_picks(ctx, picks, &search, &owner)?;
        let offer = select_offer(ctx, offers, args)?;
        (picks, offer)
    };
    let repo = picks.repo.clone().unwrap_or(Repo::NoRepo);
    let mode = picks.mode.clone().unwrap_or_default();
    let gpu = picks.gpu.clone().unwrap_or_default();
    let request_args = RequestArgs {
        repo: &repo,
        mode: &mode,
        interruptible: args.interruptible,
        disk: args.disk,
    };
    let request = vast.request(&offer, &request_args, cfg)?;
    let rate = if args.interruptible {
        crate::offers::offer_number("min_bid", crate::offers::field(&offer, "min_bid")?)?
    } else {
        offer_price(&offer)?
    };
    let rate = rate.is_finite().then_some(rate);
    crate::guardrails::spending(ctx, cfg, rate, deadline)?;
    ctx.result("provider", Value::str("vast"));
    ctx.result("id", Value::Null);
    ctx.result("ssh_alias", Value::Null);
    ctx.result("price_per_hour", rate.map_or(Value::Null, Value::Float));
    ctx.result(
        "expires_at",
        deadline.map_or(Value::Null, |at| Value::str(at.to_string())),
    );
    ctx.result("ready", Value::Bool(false));
    ctx.result("created", Value::Bool(false));
    ctx.result("expiry_armed", Value::Bool(false));
    if args.dry_run {
        ctx.result(
            "request",
            crate::redact::redact(&Value::List(request.clone())),
        );
        ctx.println("DRY RUN: no instance created")?;
        ctx.println(&pyjson::dumps(&Value::List(request), Some(2)))?;
        if let Some(expire) = args.expire() {
            ctx.println(&format!(
                "Auto-destroy requested after {expire}; no timer installed (dry run)."
            ))?;
        }
        return Ok(());
    }
    if !tty_confirmed {
        ctx.println(&format!(
            "Launch {mode} for {} on offer {} at ${}/hr?",
            repo.label(),
            py_str(crate::offers::field(&offer, "id")?),
            fixed(offer_price(&offer)?, 3)
        ))?;
        if !args.yes && !confirm(ctx, "Type 'yes' to create this paid instance: ")? {
            ctx.println("Cancelled.")?;
            return Ok(());
        }
    }
    if deadline.is_some_and(|at| at <= ctx.clock.now()) {
        return Err(Error::msg(
            "Expiry deadline passed; no instance created. Next: gpu up --expire 2h",
        ));
    }
    ctx.result("created", Value::Null);
    let response = run(ctx, &argv(&request)?, None)?;
    let parsed = pyjson::loads(&response).ok();
    ctx.result(
        "created",
        parsed
            .as_ref()
            .and_then(Value::as_dict)
            .and_then(|d| d.get("success"))
            .filter(|v| matches!(v, Value::Bool(_)))
            .cloned()
            .unwrap_or(Value::Null),
    );
    let summary = created_summary(&response)?;
    let instance_id = parsed
        .as_ref()
        .and_then(Value::as_dict)
        .and_then(created_id);
    if let Some(id) = &instance_id {
        ctx.result("id", Value::str(id));
        ctx.result("ssh_alias", Value::str(format!("vast-{id}")));
    }
    // Preserve the created ID and attempt expiry before propagating a failed announcement.
    let announced = ctx.println(&summary);
    if ctx.json() && instance_id.is_none() {
        return Err(Error::msg(
            "Create accepted but no instance ID returned; completion unverified. Next: gpu ls",
        ));
    }
    if args.expire().is_some() {
        let Some(instance_id) = &instance_id else {
            return Err(Error::msg(
                "Instance created but no ID returned; expiry NOT armed. Run gpu ls, then gpu expire ID WHEN.",
            ));
        };
        match expire_instance(ctx, instance_id, "arm", deadline) {
            Err(error) => {
                let text = error.text();
                let next = if text.contains(&format!("gpu expire {instance_id} ")) {
                    format!("Or stop billing: gpu down {instance_id}.")
                } else {
                    format!("Run gpu expire {instance_id} show or gpu down {instance_id}.")
                };
                let message = format!(
                    "Instance {instance_id} created but expiry NOT verified; billing continues. {text}\n{next}"
                );
                return Err(match error {
                    Error::Status(code, _) => Error::status(code, message),
                    _ => Error::msg(message),
                });
            }
            Ok(()) => {
                ctx.result("expiry_armed", Value::Bool(true));
                ctx.result("ready", Value::Bool(true));
            }
        }
    }
    announced?;
    record_picks(&ctx.env, &[("mode", &mode), ("gpu", &gpu)])?;
    if let Some(instance_id) = instance_id {
        if ctx.interactive() {
            let editor = pick(ctx, after_up_screen(last_editor(&ctx.env)))?;
            if editor != "none" {
                edit_instance(
                    ctx,
                    &instance_id,
                    Some(&editor),
                    config::nvim_config(cfg)?.as_ref(),
                    120.0,
                )?;
            }
        }
    }
    Ok(())
}
