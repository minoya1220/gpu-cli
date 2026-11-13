//! `gpu setup`: each provider's readiness and the provider picker. Only a
//! selected setup session can bypass the cloud opt-in gate.

use crate::cli::SetupFlags;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::gcp::setup::{gcp_ready, remove_gcp, setup_gcp};
use crate::picker::{pick, Job, Next, Opt, Screen};
use crate::provider::Provider;
use crate::pyjson::{Dict, Value};

pub(crate) const READY: &str = "✓ set up";
const NOT_READY: &str = "not set up";

/// One status row per provider. Vast is ready when an authenticated read
/// succeeds (`None` while it runs), Google Cloud when its setup record is
/// complete, Modal when local credentials exist.
fn status_rows(ctx: &Ctx, cfg: &Dict, vast: Option<bool>) -> Vec<(Provider, String)> {
    let gcp = gcp_ready(cfg);
    let modal = crate::modal::configured(ctx);
    let cells = Provider::ALL.map(|provider| {
        let ready = match provider {
            Provider::Vast => vast,
            Provider::Gcp => Some(gcp),
            Provider::Modal => Some(modal),
        };
        let state = match ready {
            Some(true) => READY,
            Some(false) => NOT_READY,
            None => "checking…",
        };
        [provider.label().to_string(), state.to_string()]
    });
    Provider::ALL
        .into_iter()
        .zip(crate::table::aligned_rows(
            &cells,
            [crate::table::Column::Left, crate::table::Column::Plain],
        ))
        .collect()
}

pub(crate) fn provider_screen(ctx: &Ctx, cfg: &Dict) -> Screen<String, String> {
    let worker = ctx.clone();
    let vast = Job::new(move || Ok(crate::vast::list_instances(&worker, None).is_ok()));
    let status = vast.clone();
    let (ctx, cfg) = (ctx.clone(), cfg.clone());
    Screen::new(
        "Provider setup",
        move || {
            status_rows(&ctx, &cfg, status.result())
                .into_iter()
                .map(|(provider, row)| Opt::new(provider.key().to_string(), row))
                .collect()
        },
        Next::Done,
    )
    .load(vast)
    .load_required(false)
}

pub fn run(ctx: &Ctx, cfg: &Dict, flags: &SetupFlags) -> Result<()> {
    ctx.result(
        "provider",
        flags
            .provider
            .map_or(Value::Null, |provider| Value::str(provider.key())),
    );
    ctx.result(
        "project",
        flags.project.as_ref().map_or(Value::Null, Value::str),
    );
    ctx.result("dry_run", Value::Bool(flags.dry_run));
    ctx.result(
        "action",
        Value::str(if flags.remove {
            "remove"
        } else if flags.provider.is_none() {
            "status"
        } else {
            "configure"
        }),
    );
    if flags.zone.is_some() {
        return Err(Error::msg("Setup needs no TPU zone. Choose it at launch")
            .next("gpu setup --provider gcp"));
    }
    if flags.remove && flags.location.is_some() {
        return Err(Error::msg("Removal uses the setup record. Omit --location")
            .next("gpu setup --provider gcp --remove --help"));
    }
    if flags.provider != Some(crate::provider::Provider::Gcp)
        && (flags.project.is_some()
            || flags.zone.is_some()
            || flags.confirm_project.is_some()
            || flags.dry_run
            || flags.remove
            || flags.location.is_some())
    {
        return Err(Error::msg("Select Google Cloud for these setup flags")
            .next("gpu setup --provider gcp --help"));
    }
    let provider = match flags.provider {
        Some(provider) => provider,
        None if ctx.interactive() => {
            let key = pick(ctx, provider_screen(ctx, cfg))?;
            Provider::from_key(&key)
                .ok_or_else(|| Error::msg(format!("Unknown provider: {key}")))?
        }
        None => {
            let vast = crate::vast::list_instances(ctx, None).is_ok();
            ctx.result("vast_ready", Value::Bool(vast));
            ctx.result("gcp_ready", Value::Bool(gcp_ready(cfg)));
            ctx.result("modal_ready", Value::Bool(crate::modal::configured(ctx)));
            for (_, row) in status_rows(ctx, cfg, Some(vast)) {
                ctx.println(&row)?;
            }
            return ctx.println("Next: gpu setup --provider gcp --help");
        }
    };
    if provider == Provider::Modal {
        return crate::modal::check(ctx);
    }
    if provider == Provider::Vast {
        crate::vast::list_instances(ctx, None).map_err(|error| match error {
            Error::Status(_, _) => error,
            _ => Error::status(
                crate::error::ExitCode::NotSetup,
                "Set up Vast.ai authentication",
            )
            .next("vastai login"),
        })?;
        ctx.result("ready", Value::Bool(true));
        return ctx.println(&format!(
            "{}  {READY}\nNext: gpu up",
            Provider::Vast.label()
        ));
    }
    // No status probe, hardware screen, or other command receives this capability.
    let cloud = ctx.for_gcp_setup();
    let recorded = cfg
        .get("gcp")
        .and_then(Value::as_dict)
        .is_some_and(|d| d.get("setup").is_some());
    let remove = if !flags.remove && recorded && ctx.interactive() && !flags.dry_run {
        pick::<String, String>(
            &cloud,
            Screen::new(
                "Google Cloud setup",
                || {
                    vec![
                        Opt::new("configure".into(), "Configure / resume setup"),
                        Opt::new("remove".into(), "Remove gpu setup"),
                    ]
                },
                Next::Done,
            ),
        )? == "remove"
    } else {
        flags.remove
    };
    let result = if remove {
        remove_gcp(&cloud, cfg, flags)
    } else {
        setup_gcp(&cloud, cfg, flags)
    };
    result.map_err(|error| {
        if matches!(
            error,
            Error::Cancelled(_) | Error::Interrupted(_) | Error::Remote(_)
        ) {
            return error;
        }
        let error = match error.next_step() {
            Some(_) => error,
            None => error.next_line("gpu setup"),
        };
        let text = error.text().to_string();
        error.with_text(text)
    })
}
