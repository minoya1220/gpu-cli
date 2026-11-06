//! Public USD prices: generated Catalog SKU metadata + live per-SKU prices.
//! Tokens are captured, passed to curl on stdin, and never cached or displayed.
use crate::{
    ctx::Ctx,
    error::{Error, Result},
    gcp,
    picker::Job,
    pyjson::{Dict, Value},
};
use jiff::Timestamp;

#[derive(Clone)]
pub struct Prices {
    pub region: String,
    pub skus: Vec<Dict>,
    pub at: Timestamp,
    pub cache_age: Option<i64>,
    pub(crate) clock: Option<std::sync::Arc<dyn crate::ctx::Clock>>,
    pub unavailable: Option<String>,
}
fn number(d: &Dict, key: &str) -> Option<f64> {
    crate::pyfmt::py_float(d.get(key)?)
        .ok()
        .filter(|n| n.is_finite())
}
fn generation(s: &str) -> Option<&'static str> {
    crate::gcp_facts::generation(s)
}
fn chips(tpu: &str) -> Option<(&'static str, f64)> {
    let (g, n) = tpu.rsplit_once('-')?;
    let g = generation(g)?;
    let n: u32 = n.parse().ok()?;
    if n == 0 {
        return None;
    }
    let n = f64::from(n);
    Some((
        g,
        if matches!(g, "v5e" | "v6e") {
            n
        } else {
            n / 2.0
        },
    ))
}
impl Prices {
    /// Normalize Catalog chip/core/VM-hours to the entire requested TPU slice.
    /// Plain Catalog hours are VM-hours, not the chip-hours on the pricing page:
    /// https://cloud.google.com/tpu/pricing#chips_vs_cores_vs_vms
    pub fn hourly(&self, tpu: &str, spot: bool) -> Option<f64> {
        let (gen, count) = chips(tpu)?;
        let mut matches = Vec::new();
        for sku in &self.skus {
            let Some(desc) = gcp::text(sku, "description") else {
                continue;
            };
            let desc = desc.to_ascii_lowercase();
            let Some(cat) = sku.get("category").and_then(Value::as_dict) else {
                continue;
            };
            let identity = format!("{} {}", desc, gcp::text(cat, "resourceGroup").unwrap_or(""));
            if generation(&identity) != Some(gen) {
                continue;
            }
            let usage = gcp::text(cat, "usageType").unwrap_or("");
            if (spot && !matches!(usage, "Preemptible" | "Spot"))
                || (!spot && usage != "OnDemand")
                || [
                    "commit",
                    "dws",
                    "flex",
                    "calendar",
                    "reservation",
                    "subscription",
                    "capacity optimized",
                ]
                .iter()
                .any(|s| desc.contains(s))
            {
                continue;
            }
            if !sku
                .get("serviceRegions")
                .and_then(Value::as_list)
                .is_some_and(|rs| rs.contains(&Value::str(&self.region)))
            {
                continue;
            }
            // v2/v3 device and pod rates differ, even within a region.
            if matches!(gen, "v2" | "v3") && desc.contains("pod") != (count > 4.0) {
                continue;
            }
            let Some(infos) = sku.get("pricingInfo").and_then(Value::as_list) else {
                continue;
            };
            let info = infos
                .iter()
                .filter_map(Value::as_dict)
                .filter_map(|d| {
                    let t = gcp::text(d, "effectiveTime")?.parse::<Timestamp>().ok()?;
                    (t <= self.at).then_some((t, d))
                })
                .max_by_key(|(t, _)| *t)
                .map(|(_, d)| d);
            let Some(expr) = info
                .and_then(|d| d.get("pricingExpression"))
                .and_then(Value::as_dict)
            else {
                continue;
            };
            let Some(rates) = expr.get("tieredRates").and_then(Value::as_list) else {
                continue;
            };
            if rates.len() != 1 {
                continue;
            } // do not misquote tiered/discounted rates
            let Some(rate) = rates[0].as_dict() else {
                continue;
            };
            let start = if rate.contains_key("startUsageAmount") {
                number(rate, "startUsageAmount")
            } else {
                Some(0.0)
            };
            if start != Some(0.0) {
                continue;
            }
            let Some(money) = rate.get("unitPrice").and_then(Value::as_dict) else {
                continue;
            };
            if gcp::text(money, "currencyCode") != Some("USD") {
                continue;
            }
            let units = match money.get("units") {
                None => 0.0,
                Some(_) => match number(money, "units") {
                    Some(n) => n,
                    None => continue,
                },
            };
            let nanos = match money.get("nanos") {
                None => 0.0,
                Some(_) => match number(money, "nanos") {
                    Some(n) => n,
                    None => continue,
                },
            };
            if !(0.0..1_000_000_000.0).contains(&nanos)
                || units < 0.0
                || units.fract() != 0.0
                || nanos.fract() != 0.0
            {
                continue;
            }
            let quantity = if expr.contains_key("unitQuantity") {
                number(expr, "unitQuantity")
            } else {
                Some(1.0)
            };
            let Some(quantity) = quantity else {
                continue;
            };
            if quantity <= 0.0 {
                continue;
            }
            let price = (units + nanos / 1e9) / quantity;
            let unit = gcp::text(expr, "usageUnit")
                .unwrap_or("")
                .to_ascii_lowercase();
            let unit_desc = gcp::text(expr, "usageUnitDescription")
                .unwrap_or("")
                .to_ascii_lowercase();
            let basis = format!("{unit} {unit_desc}");
            let is_hour = unit_desc.contains("hour")
                || unit
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .any(|s| matches!(s, "h" | "hr" | "hour"));
            let compute_chip = gcp::text(cat, "serviceDisplayName") == Some("Compute Engine")
                && gcp::text(cat, "resourceGroup") == Some("TPU");
            let hourly = if basis.contains("core") && is_hour {
                price * count * 2.0
            } else if (basis.contains("chip") || compute_chip) && is_hour {
                price * count
            } else if matches!(unit.as_str(), "h" | "hr" | "hour") {
                // A size-specific VM SKU overrides the full-host shape.
                let explicit = desc
                    .split_whitespace()
                    .filter_map(|s| {
                        chips(s.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-'))
                    })
                    .find(|(g, _)| *g == gen)
                    .map(|(_, n)| n);
                let per_vm = explicit.unwrap_or(if gen == "v5e" { 8.0 } else { 4.0 });
                price * count / per_vm
            } else {
                continue;
            };
            if hourly.is_finite() && hourly >= 0.0 {
                matches.push(hourly);
            }
        }
        let first = *matches.first()?;
        // Multiple non-equivalent rates are ambiguous, never choose the cheapest.
        matches
            .iter()
            .all(|p| (*p - first).abs() < 1e-8)
            .then_some(first)
    }
    pub fn cache_note(&self) -> String {
        self.cache_age
            .map(|age| {
                let age = age.saturating_add(
                    self.clock
                        .as_ref()
                        .map(|c| {
                            c.now()
                                .as_second()
                                .saturating_sub(self.at.as_second())
                                .max(0)
                        })
                        .unwrap_or(0),
                );
                format!(
                    "cached {}",
                    if age < 60 {
                        format!("{age}s")
                    } else if age < 3600 {
                        format!("{}m", age / 60)
                    } else if age < 86400 {
                        format!("{}h", age / 3600)
                    } else {
                        format!("{}d", age / 86400)
                    }
                )
            })
            .unwrap_or_default()
    }
    pub fn quote(&self, tpu: &str, spot: bool) -> String {
        self.hourly(tpu, spot)
            .map(|p| {
                format!(
                    "${}/hr{}",
                    crate::pyfmt::fixed(p, 3),
                    self.cache_age
                        .map(|_| format!(" ({})", self.cache_note()))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "price unavailable ({})",
                    self.unavailable
                        .as_deref()
                        .unwrap_or("no matching hourly SKU")
                )
            })
    }
    pub fn category_label(&self, types: &[&str]) -> String {
        let rate = |spot| {
            types
                .iter()
                .filter_map(|t| self.hourly(t, spot))
                .min_by(f64::total_cmp)
                .map(|p| format!("${}/hr", crate::pyfmt::fixed(p, 3)))
                .unwrap_or_else(|| "unavailable".into())
        };
        format!(
            "TPU (Google Cloud) · from {} · Spot {}",
            rate(false),
            rate(true)
        )
    }
    pub fn label(&self, tpu: &str) -> String {
        let rate = |spot| {
            self.hourly(tpu, spot)
                .map(|p| format!("${}/hr", crate::pyfmt::fixed(p, 3)))
                .unwrap_or_else(|| "unavailable".into())
        };
        if self.hourly(tpu, false).is_none() && self.hourly(tpu, true).is_none() {
            format!("TPU {tpu} · price unavailable")
        } else {
            format!("TPU {tpu} · {} · Spot {}", rate(false), rate(true))
        }
    }
}
fn unavailable(reason: &str) -> Error {
    Error::msg(reason.to_string())
}
#[derive(Clone)]
struct Entry {
    at: i64,
    sku: Dict,
}
fn entries(ctx: &Ctx, project: &str) -> std::collections::BTreeMap<String, Entry> {
    crate::gcp_http::read_cache(ctx, &format!("tpu-prices-{project}.json"), i64::MAX)
        .and_then(|d| d.get("entries").cloned())
        .and_then(|v| gcp::objects(v).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| {
            let id = gcp::text(&e, "id")
                .filter(|s| gcp::component(s))?
                .to_string();
            let Value::Int(at) = e.get("at")? else {
                return None;
            };
            let age = ctx.clock.now().as_second().checked_sub(*at)?;
            if age < 0 {
                return None;
            }
            Some((
                id,
                Entry {
                    at: *at,
                    sku: e.get("sku")?.as_dict()?.clone(),
                },
            ))
        })
        .collect()
}
fn has_region(sku: &Dict, key: &str, region: &str) -> bool {
    sku.get(key)
        .and_then(Value::as_list)
        .is_some_and(|rs| rs.contains(&Value::str(region)))
}
fn prices(
    ctx: &Ctx,
    region: String,
    entries: &std::collections::BTreeMap<String, Entry>,
    cache: bool,
    unavailable: Option<String>,
) -> Prices {
    let selected = entries
        .values()
        .filter(|e| has_region(&e.sku, "serviceRegions", &region))
        .collect::<Vec<_>>();
    let cache_age = cache.then(|| {
        selected
            .iter()
            .map(|e| ctx.clock.now().as_second().saturating_sub(e.at))
            .max()
            .unwrap_or(0)
    });
    Prices {
        region,
        skus: selected.into_iter().map(|e| e.sku.clone()).collect(),
        at: ctx.clock.now(),
        cache_age,
        clock: Some(ctx.clock.clone()),
        unavailable,
    }
}
pub fn cached(ctx: &Ctx, cfg: &Dict, zone: &str) -> Option<std::sync::Arc<Prices>> {
    if !crate::setup::gcp_ready(cfg) {
        return None;
    }
    let project = gcp::Settings::load(cfg).ok()?.project;
    let region = crate::gcp_geo::region(zone)?.to_string();
    let entries = entries(ctx, &project);
    let prices = prices(ctx, region, &entries, true, None);
    (!prices.skus.is_empty()).then(|| std::sync::Arc::new(prices))
}
fn normalize(meta: &Dict, data: Dict, at: Timestamp) -> Result<Dict> {
    if gcp::text(&data, "currencyCode") != Some("USD") {
        return Err(unavailable("Non-USD public rate."));
    }
    let rate = data
        .get("rate")
        .and_then(Value::as_dict)
        .ok_or_else(|| unavailable("No hourly public rate."))?;
    let unit = rate
        .get("unitInfo")
        .and_then(Value::as_dict)
        .ok_or_else(gcp::bad_reply)?;
    let mut expr = Dict::new();
    expr.insert(
        "usageUnit",
        unit.get("unit").cloned().unwrap_or(Value::Null),
    );
    expr.insert(
        "usageUnitDescription",
        unit.get("unitDescription").cloned().unwrap_or(Value::Null),
    );
    expr.insert(
        "unitQuantity",
        unit.get("unitQuantity")
            .and_then(Value::as_dict)
            .and_then(|d| d.get("value"))
            .cloned()
            .unwrap_or(Value::Int(1)),
    );
    let tiers = rate
        .get("tiers")
        .and_then(Value::as_list)
        .ok_or_else(gcp::bad_reply)?
        .iter()
        .map(|tier| {
            let tier = tier.as_dict().ok_or_else(gcp::bad_reply)?;
            let mut d = Dict::new();
            d.insert(
                "startUsageAmount",
                tier.get("startAmount")
                    .and_then(Value::as_dict)
                    .and_then(|d| d.get("value"))
                    .cloned()
                    .unwrap_or(Value::Int(0)),
            );
            d.insert(
                "unitPrice",
                tier.get("listPrice").cloned().ok_or_else(gcp::bad_reply)?,
            );
            Ok(Value::Dict(d))
        })
        .collect::<Result<Vec<_>>>()?;
    expr.insert("tieredRates", Value::List(tiers));
    let mut info = Dict::new();
    info.insert("effectiveTime", Value::str(at.to_string()));
    info.insert("pricingExpression", Value::Dict(expr));
    let mut sku = Dict::new();
    for key in ["description", "category"] {
        sku.insert(key, meta.get(key).cloned().unwrap_or(Value::Null));
    }
    sku.insert("skuId", meta.get("id").cloned().unwrap_or(Value::Null));
    sku.insert(
        "serviceRegions",
        meta.get("regions")
            .cloned()
            .unwrap_or(Value::List(Vec::new())),
    );
    sku.insert("pricingInfo", Value::List(vec![Value::Dict(info)]));
    Ok(sku)
}
pub fn load(ctx: &Ctx, cfg: &Dict, zone: &str) -> Result<Prices> {
    load_for(ctx, cfg, zone, None)
}
pub fn load_for(ctx: &Ctx, cfg: &Dict, zone: &str, wanted: Option<(&str, bool)>) -> Result<Prices> {
    ctx.require_gcloud()?;
    let project = gcp::Settings::load(cfg)?.project;
    let region = crate::gcp_geo::region(zone)
        .filter(|s| gcp::component(s))
        .ok_or_else(|| unavailable("Choose a TPU zone with gpu up."))?
        .to_string();
    let gen = match wanted {
        Some((tpu, _)) => Some(
            chips(tpu)
                .ok_or_else(|| unavailable("No public price mapping."))?
                .0,
        ),
        None => None,
    };
    let metas = crate::gcp_facts::skus(ctx)
        .into_iter()
        .filter(|m| {
            gcp::text(m, "id").is_some_and(gcp::component)
                && has_region(m, "regions", &region)
                && crate::gcp_facts::capacity(m)
                    .is_some_and(|spot| wanted.is_none_or(|(_, w)| spot == w))
                && gen.is_none_or(|g| gcp::text(m, "generation") == Some(g))
        })
        .collect::<Vec<_>>();
    if metas.is_empty() {
        return Err(unavailable("No public hourly SKU for this region/type."));
    }
    let _flight = ctx
        .gcp_price_lock
        .lock()
        .map_err(|_| unavailable("Price cache unavailable."))?;
    let mut stored = entries(ctx, &project);
    let now = ctx.clock.now();
    let missing = metas
        .iter()
        .filter(|m| {
            !stored
                .get(gcp::text(m, "id").unwrap())
                .is_some_and(|e| (0..600).contains(&now.as_second().saturating_sub(e.at)))
        })
        .collect::<Vec<_>>();
    let was_cached = missing.len() < metas.len();
    let urls = missing
        .iter()
        .map(|m| {
            format!(
                "https://cloudbilling.googleapis.com/v1beta/skus/{}/price?currencyCode=USD",
                gcp::text(m, "id").unwrap()
            )
        })
        .collect::<Vec<_>>();
    let mut failure = None;
    for (meta, data) in missing
        .into_iter()
        .zip(crate::gcp_http::many_results(ctx, &project, &urls)?)
    {
        match data.and_then(|d| normalize(meta, d, now)) {
            Ok(sku) => {
                stored.insert(
                    gcp::text(meta, "id").unwrap().into(),
                    Entry {
                        at: now.as_second(),
                        sku,
                    },
                );
            }
            Err(e) => {
                failure = Some(e.text().to_string());
                stored.remove(gcp::text(meta, "id").unwrap());
            }
        }
    }
    let mut cache = Dict::new();
    cache.insert(
        "entries",
        Value::List(
            stored
                .iter()
                .map(|(id, e)| {
                    let mut d = Dict::new();
                    d.insert("id", Value::str(id));
                    d.insert("at", Value::Int(e.at));
                    d.insert("sku", Value::Dict(e.sku.clone()));
                    Value::Dict(d)
                })
                .collect(),
        ),
    );
    crate::gcp_http::write_cache(ctx, &format!("tpu-prices-{project}.json"), cache);
    let ids = metas
        .iter()
        .filter_map(|m| gcp::text(m, "id"))
        .collect::<std::collections::HashSet<_>>();
    let relevant = stored
        .into_iter()
        .filter(|(id, _)| ids.contains(id.as_str()))
        .collect();
    let prices = prices(ctx, region, &relevant, was_cached, failure.clone());
    if prices.skus.is_empty() {
        return Err(unavailable(
            failure.as_deref().unwrap_or("No public hourly rate."),
        ));
    }
    Ok(prices)
}
pub fn job(ctx: &Ctx, cfg: &Dict) -> Job<std::sync::Arc<Prices>> {
    job_at(ctx, cfg, None)
}
pub(crate) fn job_at(ctx: &Ctx, cfg: &Dict, zone: Option<&str>) -> Job<std::sync::Arc<Prices>> {
    let explicit = zone.map(String::from);
    let worker = ctx.clone();
    let cfg = cfg.clone();
    Job::new(move || {
        if !crate::setup::gcp_ready(&cfg) {
            return Err("needs gpu setup".into());
        }
        let settings = gcp::Settings::load(&cfg).map_err(|e| e.text().to_string())?;
        let zone = explicit
            .clone()
            .or_else(|| crate::gcp_preferences::preview(&worker, &settings))
            .ok_or("No known TPU pricing region.")?;
        // Rare metadata refresh is nonfatal: known SKU IDs can still return
        // authoritative current rates. No API is enabled implicitly.
        let _ = crate::gcp_facts::refresh_skus(&worker, &settings.project);
        load(&worker, &cfg, &zone)
            .map(std::sync::Arc::new)
            .map_err(|e| e.text().to_string())
    })
}

pub(crate) fn cached_default(ctx: &Ctx, cfg: &Dict) -> Option<std::sync::Arc<Prices>> {
    let settings = gcp::Settings::load(cfg).ok()?;
    cached(ctx, cfg, &crate::gcp_preferences::preview(ctx, &settings)?)
}
