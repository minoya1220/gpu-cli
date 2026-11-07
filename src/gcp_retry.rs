//! Capacity recovery only after authenticated absence and timer cleanup.
//! All alternative/type/price reads for the retry menu run in a Job.
use crate::{
    ctx::Ctx,
    gcp::Settings,
    gcp_catalog::Zone,
    gcp_wizard::Picks,
    picker::{Job, Next, Opt, Screen},
    pyjson::Dict,
};
use std::collections::BTreeSet;
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Choice {
    pub zone: String,
    pub approved: bool,
    pub rate: Option<f64>,
}
#[derive(Clone)]
struct Offer {
    zone: String,
    price: String,
    rate: Option<f64>,
}

pub(crate) fn alternatives(
    ctx: &Ctx,
    settings: &Settings,
    picks: &Picks,
    excluded: &BTreeSet<String>,
    count: usize,
) -> Vec<Zone> {
    let mut found = Vec::new();
    for zone in crate::gcp_catalog::known(ctx, &settings.project, Some(&picks.tpu)) {
        if excluded.contains(&zone.name) {
            continue;
        }
        if picks.deadline.as_second() <= ctx.clock.now().as_second() {
            break;
        }
        let types = match crate::gcp_catalog::types(ctx, &settings.project, &zone.name) {
            Ok(types) => types,
            // Do not fan out after a transport/auth failure, or present it as
            // proof that a zone lacks this type. Manual selection stays usable.
            Err(_) => break,
        };
        if types.contains(&crate::gcp_catalog::canonical(&picks.tpu)) {
            found.push(zone);
            if found.len() == count {
                break;
            }
        }
    }
    found
}
pub(crate) fn command(picks: &Picks, zone: &str) -> String {
    let mut args = vec![
        "gpu".to_string(),
        "up".into(),
        "--tpu".into(),
        picks.tpu.clone(),
        "--zone".into(),
        zone.into(),
        if picks.spot { "--spot" } else { "--on-demand" }.into(),
        "--runtime".into(),
        picks.runtime.clone(),
        "--expire".into(),
        picks.deadline.to_string(),
    ];
    if picks.repo == crate::repos::Repo::NoRepo {
        args.push("--no-repo".into());
    } else {
        args.extend(["--repo".into(), picks.repo.as_arg().into()]);
    }
    args.iter()
        .map(|s| crate::pyfmt::shell_quote(s))
        .collect::<Vec<_>>()
        .join(" ")
}
pub(crate) fn screen(
    ctx: &Ctx,
    settings: &Settings,
    cfg: &Dict,
    picks: &Picks,
    excluded: &BTreeSet<String>,
) -> Screen<String, Option<Choice>> {
    let worker = ctx.clone();
    let config = cfg.clone();
    let selected = picks.clone();
    let project = settings.clone();
    let tried = excluded.clone();
    let offer = Job::new(move || {
        Ok(alternatives(&worker, &project, &selected, &tried, 1)
            .into_iter()
            .next()
            .map(|zone| {
                let (price, rate) = match crate::gcp_prices::load_for(
                    &worker,
                    &config,
                    &zone.name,
                    Some((&selected.tpu, selected.spot)),
                ) {
                    Ok(p) => (
                        p.quote(&selected.tpu, selected.spot),
                        p.hourly(&selected.tpu, selected.spot),
                    ),
                    Err(e) => (
                        format!(
                            "price unavailable ({})",
                            e.text().split(". Next:").next().unwrap_or("read failed")
                        ),
                        None,
                    ),
                };
                Offer {
                    zone: zone.name,
                    price,
                    rate,
                }
            }))
    });
    let approved = offer.clone();
    let result = offer.clone();
    let tpu = crate::gcp_types::public(picks.tpu.clone());
    let mode = if picks.spot { "Spot" } else { "on-demand" };
    let picker_config = cfg.clone();
    let picker_spot = picks.spot;
    let parent = ctx.clone();
    let settings = settings.clone();
    let tpu_filter = picks.tpu.clone();
    let excluded = excluded.clone();
    let local = crate::gcp::deadline_label(ctx, picks.deadline);
    Screen::new(
        "Retry TPU launch",
        move || {
            let mut rows = Vec::new();
            if let Some(Some(offer)) = result.result() {
                rows.push(Opt::new(
                    format!("@retry:{}", offer.zone),
                    crate::offers::aligned_rows(
                        &[[format!("Retry {tpu} {mode} in {}", offer.zone), offer.price]],
                        [crate::offers::Column::Left, crate::offers::Column::Plain],
                    )
                    .remove(0),
                ));
            }
            rows.push(Opt::new("@choose".into(), "Choose another zone"));
            rows.push(Opt::new("@cancel".into(), "Cancel"));
            rows
        },
        move |value: String| {
            if let Some(zone) = value.strip_prefix("@retry:") {
                return Next::Done(Some(Choice {
                    zone: zone.into(),
                    approved: true,
                    rate: approved.result().flatten().and_then(|o| o.rate),
                }));
            }
            if value != "@choose" {
                return Next::Done(None);
            }
            Next::Screen(crate::gcp_wizard::priced_zone_screen(
                &parent,
                &settings,
                &picker_config,
                &tpu_filter,
                picker_spot,
                &excluded,
                |zone| {
                    Next::Done(Some(Choice {
                        zone,
                        approved: false,
                        rate: None,
                    }))
                },
            ))
        },
    )
    .load(offer)
    .loading("Finding next zone and price…")
    .notice(format!(
        "Deletes at {local} (storage and networking charges apply)."
    ))
}
