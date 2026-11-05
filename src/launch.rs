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
    if args.max_price.is_some_and(|p| p <= 0.0) {
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
        return Err(Error::msg(NO_OFFERS));
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
        return Err(Error::msg(NO_OFFERS));
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
        return Err(Error::msg(format!(
            "Vast did not create the instance: {}",
            redact_text(&reason)
        )));
    }
    match response.get("new_contract") {
        None | Some(Value::Null) => Ok("Created instance. Next: `gpu ls` to find it.".into()),
        Some(instance) => {
            let instance = py_str(instance);
            Ok(format!(
                "Created instance {instance}.\nConnect: gpu ssh {instance} --exec"
            ))
        }
    }
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
    };
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
                tpu_setup_error: crate::gcp::launch_setup(ctx, cfg)
                    .err()
                    .map(|e| e.text().to_string()),
                owner: owner.clone(),
            },
        );
        let picks = wizard.run(ctx, picks)?;
        if picks.tpu.is_some() {
            // The picker only returns a provider choice. Cloud work starts here,
            // after its input loop has exited, never in a screen transition.
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
    if args.dry_run {
        ctx.println("DRY RUN: no instance created")?;
        ctx.println(&pyjson::dumps(&Value::List(request), Some(2)))?;
        if let Some(expire) = args.expire() {
            ctx.println(&format!(
                "Auto-destroy requested after {expire}; no timer installed (dry run)."
            ))?;
        }
        return Ok(());
    }
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
    let response = run(ctx, &argv(&request)?, None)?;
    ctx.println(&created_summary(&response)?)?;
    record_picks(&ctx.env, &[("mode", &mode), ("gpu", &gpu)])?;
    let instance_id = match pyjson::loads(&response) {
        Ok(Value::Dict(dict)) => dict
            .get("new_contract")
            .filter(|v| **v != Value::Null)
            .map(py_str),
        _ => None,
    };
    if args.expire().is_some() {
        let Some(instance_id) = &instance_id else {
            return Err(Error::msg(
                "Instance created but no ID returned; expiry NOT armed. Run gpu ls, then gpu expire ID WHEN.",
            ));
        };
        match expire_instance(ctx, instance_id, "arm", deadline) {
            Err(Error::Msg(text)) => {
                let next = if text.contains(&format!("gpu expire {instance_id} ")) {
                    format!("Or stop billing: gpu down {instance_id}.")
                } else {
                    format!("Run gpu expire {instance_id} show or gpu down {instance_id}.")
                };
                return Err(Error::msg(format!(
                    "Instance {instance_id} created but expiry NOT verified; billing continues. {text}\n{next}"
                )));
            }
            Err(other) => return Err(other),
            Ok(()) => {}
        }
    }
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
