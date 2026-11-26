//! `gpu up --provider gcp`: flags, picker confirmation, create and capacity retry.
use jiff::Timestamp;

use super::{
    catalog, expiry, gcloud::run, id::TpuId, inventory::absent, preferences, prices, retry,
    settings::Settings, startup::startup, types, wizard,
};
use crate::cli::UpFlags;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::provider::Provider;
use crate::pyjson::{self, Dict, Value};

/// Validate launch configuration locally, before leaving the hardware picker.
/// This does not call gcloud or verify remote IAM/queue readiness.
pub fn launch_setup(ctx: &Ctx, cfg: &Dict) -> Result<(Settings, expiry::Queue)> {
    let load = || {
        ctx.require_gcloud()?;
        let settings = Settings::load(cfg)?;
        let queue = expiry::Queue::load(&settings)?;
        Ok((settings, queue))
    };
    load().map_err(|error: Error| {
        let message = format!(
            "{}\nConfig: {}",
            error.text(),
            crate::config::config_path(&ctx.env).display()
        );
        Error::status(crate::error::ExitCode::NotSetup, message)
    })
}

/// What a TPU launch reads from `gpu up` flags or the launch picker.
#[derive(Clone, Debug, Default)]
pub(crate) struct TpuArgs {
    pub tpu: Option<String>,
    pub zone: Option<String>,
    pub runtime: Option<String>,
    /// `Some(true)` for spot, `Some(false)` for on-demand, `None` to ask.
    pub spot: Option<bool>,
    pub repo: Option<crate::repos::Repo>,
    pub yes: bool,
    pub dry_run: bool,
}

impl TpuArgs {
    /// TPU arguments from `gpu up` flags. Vast-only flags are rejected, never
    /// silently ignored.
    pub(crate) fn from_flags(flags: &UpFlags) -> Result<TpuArgs> {
        let args = crate::launch::UpArgs::from(flags);
        reject_vast_flags(&args)?;
        Ok(TpuArgs {
            tpu: flags.tpu.clone(),
            zone: flags.zone.clone(),
            runtime: flags.runtime.clone(),
            spot: match (flags.spot, flags.on_demand) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            },
            repo: args.repo,
            yes: args.yes,
            dry_run: args.dry_run,
        })
    }

    /// The launch picker's repository and TPU choice. Only the mode chosen
    /// *in the UI* is dropped: explicit Vast-only flags are still rejected.
    pub(crate) fn picked(
        args: &crate::launch::UpArgs,
        picks: &crate::wizard::Picks,
    ) -> Result<TpuArgs> {
        reject_vast_flags(args)?;
        let cloud = picks.cloud.as_ref();
        let chosen = match &picks.tpu {
            Some(crate::wizard::TpuChoice::Type(tpu)) => Some(tpu.clone()),
            _ => None,
        };
        Ok(TpuArgs {
            tpu: cloud.and_then(|s| s.tpu.clone()).or(chosen),
            zone: cloud.and_then(|s| s.zone.clone()),
            runtime: None,
            spot: cloud.and_then(|s| s.spot),
            repo: Some(picks.repo.clone().unwrap_or(crate::repos::Repo::NoRepo)),
            yes: args.yes,
            dry_run: args.dry_run,
        })
    }
}

/// Launch what the normal wizard picked through the TPU flow.
pub fn launch_from_picker(
    ctx: &Ctx,
    args: &crate::launch::UpArgs,
    picks: &crate::wizard::Picks,
    cfg: &Dict,
    deadline: Option<Timestamp>,
) -> Result<crate::launch::Launched> {
    let tpu = TpuArgs::picked(args, picks)?;
    let deadline = picks.cloud.as_ref().and_then(|s| s.expiry_at).or(deadline);
    launch(ctx, &tpu, cfg, deadline)
}

fn reject_vast_flags(args: &crate::launch::UpArgs) -> Result<()> {
    if args.mode.is_some()
        || args.gpu.is_some()
        || args.num_gpus != 1
        || args.max_price.is_some()
        || args.interruptible
        || args.offer.is_some()
        || args.disk.is_some()
        || args.reliability.is_some()
        || args.unverified
    {
        return Err(Error::msg("Vast GPU/offer/disk flags do not apply to TPUs")
            .next("gpu up --provider gcp --help"));
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct Prepared {
    pub(crate) picks: wizard::TpuPicks,
    title: String,
    price: String,
    rate: Option<f64>,
}

fn quote(ctx: &Ctx, cfg: &Dict, picks: &wizard::TpuPicks) -> (String, Option<f64>) {
    match prices::load_for(ctx, cfg, &picks.zone, Some((&picks.tpu, picks.spot))) {
        Ok(prices) => (
            prices.quote(&picks.tpu, picks.spot),
            prices.hourly(&picks.tpu, picks.spot),
        ),
        Err(e) => (format!("price unavailable ({})", e.summary()), None),
    }
}

fn launch_title(ctx: &Ctx, picks: &wizard::TpuPicks, price: &str) -> String {
    format!(
        "Launch {} {} in {} for {price}, deletes at {}? (storage and networking charges apply)",
        types::public(picks.tpu.clone()),
        if picks.spot { "Spot" } else { "on-demand" },
        picks.zone,
        deadline_label(ctx, picks.deadline)
    )
}

fn prepare(ctx: &Ctx, cfg: &Dict, picks: wizard::TpuPicks) -> Result<Prepared> {
    let (settings, queue) = launch_setup(ctx, cfg)?;
    expiry::validate_deadline(ctx, picks.deadline)?;
    startup(&picks.repo, cfg)?;
    queue.reconcile(ctx)?;
    catalog::verify(ctx, &settings.project, &picks.zone, &picks.tpu)?;
    let (price, rate) = quote(ctx, cfg, &picks);
    let title = launch_title(ctx, &picks, &price);
    Ok(Prepared {
        picks,
        title,
        price,
        rate,
    })
}

fn preparation_job(
    ctx: &Ctx,
    cfg: &Dict,
    picks: wizard::TpuPicks,
) -> crate::picker::Job<crate::prompt::Confirmation<Prepared>> {
    let worker = ctx.clone();
    let cfg = cfg.clone();
    crate::picker::Job::new(move || {
        let prepared = prepare(&worker, &cfg, picks.clone()).map_err(|e| e.text().to_string())?;
        Ok(crate::prompt::Confirmation {
            title: prepared.title.clone(),
            value: prepared,
        })
    })
}

pub(crate) fn confirmation<V: crate::picker::PickValue>(
    ctx: &Ctx,
    cfg: &Dict,
    picks: wizard::TpuPicks,
) -> crate::picker::Screen<V, Prepared> {
    crate::prompt::confirmation_screen(preparation_job(ctx, cfg, picks))
}

fn retry_confirmation(
    ctx: &Ctx,
    cfg: &Dict,
    picks: &wizard::TpuPicks,
    screen: crate::picker::Screen<String, Option<retry::Choice>>,
    approved: std::rc::Rc<std::cell::RefCell<Option<Prepared>>>,
) -> crate::picker::Screen<String, Option<retry::Choice>> {
    use crate::picker::Next;
    use std::rc::Rc;
    let worker = ctx.clone();
    let cfg = cfg.clone();
    let picks = picks.clone();
    screen.remap(
        Rc::new(|v| v),
        Rc::new(|v| v),
        Rc::new(move |choice| {
            let Some(choice) = choice else {
                return Next::Done(None);
            };
            let mut picks = picks.clone();
            picks.zone = choice.zone.clone();
            let job = preparation_job(&worker, &cfg, picks);
            let estimate = choice.rate;
            let already_approved = choice.approved;
            let watched = job.clone();
            let approved = approved.clone();
            let screen =
                crate::prompt::confirmation_screen::<String, _>(job).on_settled(move || {
                    let prepared = watched.result()?.value;
                    (already_approved && prepared.rate == estimate).then_some(Next::Done(prepared))
                });
            Next::Screen(screen.remap(
                Rc::new(|v| v),
                Rc::new(|v| v),
                Rc::new(move |prepared| {
                    *approved.borrow_mut() = Some(prepared);
                    Next::Done(Some(choice.clone()))
                }),
            ))
        }),
    )
}

pub(crate) fn launch_prepared(
    ctx: &Ctx,
    flags: &TpuArgs,
    cfg: &Dict,
    prepared: Prepared,
) -> Result<crate::launch::Launched> {
    launch_loop(
        ctx,
        flags,
        cfg,
        Some(prepared.picks.deadline),
        Some(prepared),
        |screen| crate::picker::pick(ctx, screen),
    )
}

pub(crate) fn launch(
    ctx: &Ctx,
    flags: &TpuArgs,
    cfg: &Dict,
    deadline: Option<Timestamp>,
) -> Result<crate::launch::Launched> {
    launch_with_retry(ctx, flags, cfg, deadline, |screen| {
        crate::picker::pick(ctx, screen)
    })
}
pub(crate) fn launch_with_retry(
    ctx: &Ctx,
    flags: &TpuArgs,
    cfg: &Dict,
    deadline: Option<Timestamp>,
    retry: impl FnMut(
        crate::picker::Screen<String, Option<retry::Choice>>,
    ) -> Result<Option<retry::Choice>>,
) -> Result<crate::launch::Launched> {
    launch_loop(ctx, flags, cfg, deadline, None, retry)
}

/// The TPU label carrying the launch's owner (see `crate::owner`).
pub(crate) const OWNER_LABEL: &str = "gpu-owner";

fn launch_loop(
    ctx: &Ctx,
    flags: &TpuArgs,
    cfg: &Dict,
    deadline: Option<Timestamp>,
    mut prepared: Option<Prepared>,
    mut retry: impl FnMut(
        crate::picker::Screen<String, Option<retry::Choice>>,
    ) -> Result<Option<retry::Choice>>,
) -> Result<crate::launch::Launched> {
    let (settings, queue) = launch_setup(ctx, cfg)?;
    if prepared.is_none() && ctx.interactive() && !flags.yes && !flags.dry_run {
        prepared = Some(wizard::choose_confirmed(
            ctx, flags, &settings, cfg, deadline,
        )?);
    }
    let mut picks = match &prepared {
        Some(prepared) => prepared.picks.clone(),
        None => wizard::choose(ctx, flags, &settings, cfg, deadline)?,
    };
    let deadline = picks.deadline;
    if !flags.dry_run {
        crate::owner::warn_duplicate(ctx, cfg, Provider::Gcp)?;
    }
    let mut confirm = !flags.yes;
    let mut approved_rate = None;
    let mut check_rate = false;
    let mut attempted = std::collections::BTreeSet::new();
    loop {
        attempted.insert(picks.zone.clone());
        expiry::validate_deadline(ctx, deadline)?;
        let approved = prepared.take();
        if !flags.dry_run {
            if approved.is_none() {
                queue.reconcile(ctx)?;
                catalog::verify(ctx, &settings.project, &picks.zone, &picks.tpu)?;
            }
            preferences::remember(ctx, &settings.project, &picks.zone);
            preferences::remember_type(ctx, &settings.project, &picks.tpu);
        }
        let id = TpuId {
            project: settings.project.clone(),
            zone: picks.zone.clone(),
            name: unique_name(ctx, "gpu")?,
        };
        let mut args = id.args("create");
        args.extend([
            format!("--accelerator-type={}", picks.tpu),
            format!("--version={}", picks.runtime),
            match crate::owner::from_cfg(cfg) {
                Some(owner) => format!("--labels=gpu-cli=true,{OWNER_LABEL}={owner}"),
                None => "--labels=gpu-cli=true".into(),
            },
        ]);
        if picks.spot {
            args.push("--spot".into());
        }
        if let Some(sa) = &settings.service_account {
            args.push(format!("--service-account={sa}"));
        }
        let script = startup(&picks.repo, cfg)?;
        // These locally validated scripts/paths contain no '|'. gcloud receives
        // one argv value, never a locally executed shell string.
        args.push(format!(
            "--metadata=^|^startup-script={script}|gpu-expiry-queue={}|gpu-workspace={}",
            queue.resource(),
            picks.directory
        ));
        if flags.dry_run {
            let rate = prices::cached(ctx, cfg, &picks.zone)
                .and_then(|prices| prices.hourly(&picks.tpu, picks.spot));
            crate::guardrails::spending(ctx, cfg, rate, Some(deadline))?;
            crate::launch::planned_result(ctx, Provider::Gcp, None, rate, Some(deadline));
            crate::guardrails::dry_run_budget(ctx, cfg)?;
            ctx.println("DRY RUN: no TPU or expiry task created")?;
            let requests = [queue.create_args(ctx, &id, deadline)?, args];
            let requests = Value::List(
                requests
                    .into_iter()
                    .map(|args| Value::List(args.into_iter().map(Value::Str).collect()))
                    .collect(),
            );
            ctx.result("request", requests.clone());
            ctx.println(&pyjson::dumps(&requests, Some(2)))?;
            return Ok(crate::launch::Launched::default());
        }
        let (price, rate) = approved
            .as_ref()
            .map(|p| (p.price.clone(), p.rate))
            .unwrap_or_else(|| quote(ctx, cfg, &picks));
        let budget = crate::guardrails::spending(ctx, cfg, rate, Some(deadline))?;
        crate::launch::planned_result(ctx, Provider::Gcp, Some(&id.cli()), rate, Some(deadline));
        let local = deadline_label(ctx, deadline);
        if approved.is_none() {
            ctx.println(&launch_title(ctx, &picks, &price))?;
        }
        if check_rate && rate != approved_rate {
            confirm = true;
        }
        if approved.is_none() && confirm && !crate::prompt::confirm(ctx, None)? {
            ctx.println("Cancelled.")?;
            return Ok(crate::launch::Launched::default());
        }
        if deadline.as_second() <= ctx.clock.now().as_second() {
            return Err(Error::msg("Expiry deadline passed. No TPU created")
                .next("gpu up --provider gcp --expire 2h"));
        }
        let reservation = crate::spend::reserve(ctx, budget)?;
        let wait = match ctx
            .waiting("Waiting for Google Cloud to create the TPU")
            .and_then(|wait| queue.check(ctx).map(|()| wait))
        {
            Ok(wait) => wait,
            Err(e) => {
                crate::spend::release_best_effort(ctx, reservation);
                return Err(e);
            }
        };
        // Arm and read back the control-plane task BEFORE issuing create. A laptop
        // crash, SSH failure, stopped VM or guest failure cannot remove this timer.
        if let Err(e) = queue.arm(ctx, &id, deadline) {
            crate::spend::release_best_effort(ctx, reservation);
            return Err(e);
        }
        ctx.result("expiry_armed", Value::Bool(true));
        if deadline.as_second() <= ctx.clock.now().as_second() {
            crate::spend::release_best_effort(ctx, reservation);
            return Err(Error::msg(
                "Expiry deadline passed. No TPU created. Its deletion task may remain",
            )
            .next(format!("gpu expire {} cancel", id.cli())));
        }
        ctx.result("created", Value::Null);
        if let Err(e) = run(ctx, &args, "gpu up --provider gcp --spot --expire 2h") {
            // A known request rejection plus an authenticated 404 is terminal.
            // Keep the timer after ambiguous errors/timeouts (create may be in flight).
            let rejected = e.is_rejection();
            if rejected && absent(ctx, &id).unwrap_or(false) && queue.cancel(ctx, &id).is_ok() {
                crate::spend::release_best_effort(ctx, reservation);
                ctx.result("expiry_armed", Value::Bool(false));
                ctx.result("created", Value::Bool(false));
                drop(wait);
                if matches!(e, Error::Status(crate::error::ExitCode::NoCapacity, _)) {
                    let failure = format!(
                        "No {} capacity in {}. No node created. Deletion task removed.",
                        if picks.spot { "Spot" } else { "on-demand" },
                        picks.zone
                    );
                    if ctx.interactive() {
                        ctx.println(&failure)?;
                        let approved_retry = std::rc::Rc::new(std::cell::RefCell::new(None));
                        let screen = retry::screen(ctx, &settings, cfg, &picks, &attempted);
                        let screen = if !flags.yes {
                            retry_confirmation(ctx, cfg, &picks, screen, approved_retry.clone())
                        } else {
                            screen
                        };
                        if let Some(choice) = retry(screen)? {
                            prepared = approved_retry.borrow_mut().take();
                            picks.zone = choice.zone;

                            confirm = !choice.approved;
                            approved_rate = choice.rate;
                            check_rate = choice.approved;
                            continue;
                        }
                        ctx.println("Cancelled.")?;
                        return Ok(crate::launch::Launched::default());
                    }
                    let alternatives = retry::alternatives(ctx, &settings, &picks, &attempted, 3);
                    let zones = alternatives
                        .iter()
                        .map(|z| z.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let next = alternatives
                        .first()
                        .map(|z| retry::command(&picks, &z.name))
                        .unwrap_or_else(|| "gpu up --provider gcp".into());
                    return Err(Error::status(
                        crate::error::ExitCode::NoCapacity,
                        format!(
                            "{failure}\nOther {} zones: {}.",
                            picks.tpu,
                            if zones.is_empty() {
                                "none verified"
                            } else {
                                &zones
                            }
                        ),
                    )
                    .next_line(next));
                }
                let message = format!(
                    "Create rejected: {}. No node created. Deletion task removed",
                    e.summary()
                );
                return Err(e.with_text(message).next("gpu up --provider gcp"));
            }
            let message = format!(
                "{}\nCreate {} unverified. Deletion task remains armed",
                e.summary(),
                id.cli()
            );
            return Err(e.with_text(message).next("gpu ls --provider gcp"));
        }
        drop(wait);
        ctx.result("created", Value::Bool(true));
        crate::spend::claim_best_effort(ctx, reservation, &crate::spend::key("gcp", &id.cli()));
        ctx.println(&format!(
            "Created TPU {}. Deletes at {local}.\nNext: gpu ssh {} --exec",
            id.cli(),
            id.cli()
        ))?;
        crate::editors::offer_after_launch(ctx, cfg, &id)?;
        return Ok(crate::launch::Launched {
            id: Some(id.cli()),
            ready_alias: None,
        });
    }
}

/// Keep today's deadline short, but never hide a different date or year.
pub(crate) fn deadline_label(ctx: &Ctx, deadline: Timestamp) -> String {
    let zone = crate::expiry::local_zone(&ctx.env);
    let now = ctx.clock.now().to_zoned(zone.clone());
    let local = deadline.to_zoned(zone);
    let format = if local.date() == now.date() {
        "%-I:%M %p"
    } else if local.year() == now.year() {
        "%b %-d at %-I:%M %p"
    } else {
        "%b %-d %Y at %-I:%M %p"
    };
    local.strftime(format).to_string()
}

pub fn unique_name(ctx: &Ctx, prefix: &str) -> Result<String> {
    // Generate directly, without creating a throwaway file. Never offer user
    // names: a lingering deletion task must not target a replacement instance.
    let mut random = [0u8; 6];
    getrandom::fill(&mut random)
        .map_err(|_| Error::msg("Could not generate a unique TPU name."))?;
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let suffix: String = random
        .into_iter()
        .map(|byte| alphabet[byte as usize % alphabet.len()] as char)
        .collect();
    Ok(format!("{prefix}-{}-{suffix}", ctx.clock.now().as_second()))
}
