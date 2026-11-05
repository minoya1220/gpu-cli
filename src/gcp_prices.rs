//! Public USD list prices, fetched from Cloud Billing Catalog with gcloud OAuth.
//! Tokens are captured, passed to curl on stdin, and never cached or displayed.
use crate::{
    ctx::Ctx,
    error::{Error, Result},
    gcp,
    picker::Job,
    process::{Cmd, Input},
    pyjson::{self, Dict, Value},
};
use jiff::Timestamp;

#[derive(Clone)]
pub struct Prices {
    pub region: String,
    pub skus: Vec<Dict>,
    pub at: Timestamp,
}
fn number(d: &Dict, key: &str) -> Option<f64> {
    crate::pyfmt::py_float(d.get(key)?)
        .ok()
        .filter(|n| n.is_finite())
}
fn generation(s: &str) -> Option<&'static str> {
    let s = s.to_ascii_lowercase();
    let compact: String = s.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if compact.contains("v6e") || compact.contains("trillium") {
        Some("v6e")
    } else if compact.contains("v5lite") || compact.contains("v5e") {
        Some("v5e")
    } else if compact.contains("v5p") {
        Some("v5p")
    } else if compact.contains("v4") {
        Some("v4")
    } else if compact.contains("v3") {
        Some("v3")
    } else if compact.contains("v2") {
        Some("v2")
    } else {
        None
    }
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
                || ["commit", "flex", "calendar", "reservation"]
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
            if number(rate, "startUsageAmount").unwrap_or(0.0) != 0.0 {
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
            let price = units + nanos / 1e9;
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
            let hourly = if basis.contains("chip") && is_hour {
                price * count
            } else if basis.contains("core") && is_hour {
                price * count * 2.0
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
    pub fn quote(&self, tpu: &str, spot: bool) -> String {
        self.hourly(tpu, spot)
            .map(|p| {
                format!(
                    "${}/hr USD list ({})",
                    crate::pyfmt::fixed(p, 3),
                    self.region
                )
            })
            .unwrap_or_else(|| {
                format!(
                    "price unavailable (no unambiguous hourly SKU in {})",
                    self.region
                )
            })
    }
    pub fn category_label(&self, types: &[&str]) -> String {
        let rate = |spot| {
            types
                .iter()
                .filter_map(|t| self.hourly(t, spot))
                .min_by(f64::total_cmp)
                .map(|p| format!("${}", crate::pyfmt::fixed(p, 3)))
                .unwrap_or_else(|| "unavailable".into())
        };
        format!(
            "TPU (Google Cloud)  from {} on-demand / {} Spot USD/hr ({})",
            rate(false),
            rate(true),
            self.region
        )
    }
    pub fn label(&self, tpu: &str) -> String {
        let rate = |spot| {
            self.hourly(tpu, spot)
                .map(|p| format!("${}", crate::pyfmt::fixed(p, 3)))
                .unwrap_or_else(|| "unavailable".into())
        };
        format!(
            "TPU {tpu} (Google Cloud)  on-demand {} / Spot {} USD/hr ({})",
            rate(false),
            rate(true),
            self.region
        )
    }
}
fn unavailable(reason: &str) -> Error {
    Error::msg(format!(
        "price unavailable ({reason}); rerun gpu setup for Cloud Billing API access"
    ))
}
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn page(ctx: &Ctx, token: &str, project: &str, path: &str) -> Result<Dict> {
    let url = format!("https://cloudbilling.googleapis.com/v1/{path}");
    let config = format!(
        "header = \"Authorization: Bearer {token}\"\nheader = \"x-goog-user-project: {project}\"\n"
    );
    let cmd = Cmd::new(&[
        "curl",
        "--disable", // ignore ~/.curlrc (redirects, traces, proxy overrides)
        "--silent",
        "--show-error",
        "--fail-with-body",
        "--noproxy",
        "*",
        "--proto",
        "=https",
        "--max-time",
        "15",
        "--config",
        "-",
        &url,
    ])
    .stdin(Input::Text(config))
    .timeout(Some(20.0));
    let out = ctx
        .run(&cmd)
        .map_err(|_| unavailable("could not run the Catalog HTTPS request"))?;
    if out.code != 0 {
        let reason = if out.stdout.contains("SERVICE_DISABLED") {
            "Cloud Billing API is disabled"
        } else if out.stdout.contains("PERMISSION_DENIED") {
            "Catalog access was denied"
        } else {
            "Catalog HTTPS request failed"
        };
        return Err(unavailable(reason));
    }
    pyjson::loads(&out.stdout)
        .ok()
        .and_then(|v| v.as_dict().cloned())
        .ok_or_else(|| unavailable("unreadable Catalog reply"))
}
fn pages(ctx: &Ctx, token: &str, project: &str, path: &str, key: &str) -> Result<Vec<Dict>> {
    let mut rows = Vec::new();
    let mut next = String::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..30 {
        let query = format!(
            "{path}{}pageSize=5000&pageToken={}",
            if path.contains('?') { "&" } else { "?" },
            encode(&next)
        );
        let page = page(ctx, token, project, &query)?;
        rows.extend(
            gcp::objects(page.get(key).cloned().unwrap_or(Value::List(Vec::new())))
                .map_err(|_| unavailable("incomplete Catalog page"))?,
        );
        next = gcp::text(&page, "nextPageToken").unwrap_or("").into();
        if next.is_empty() {
            return Ok(rows);
        }
        if !seen.insert(next.clone()) {
            return Err(unavailable("repeated Catalog page token"));
        }
    }
    Err(unavailable("Catalog pagination limit reached"))
}
pub fn load(ctx: &Ctx, cfg: &Dict, zone: &str) -> Result<Prices> {
    ctx.require_gcloud()?;
    let project = gcp::Settings::load(cfg)?.project;
    let region = crate::gcp_geo::region(zone)
        .filter(|s| gcp::component(s))
        .ok_or_else(|| unavailable("configure a TPU zone"))?
        .to_string();
    let at = ctx.clock.now();
    let path = ctx
        .env
        .xdg("XDG_CACHE_HOME", ".cache")
        .join("gpu")
        .join(format!("tpu-prices-{project}-{region}.json"));
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(Value::Dict(d)) = pyjson::loads(&raw) {
            let age = number(&d, "at").map(|t| at.as_second() as f64 - t);
            if age.is_some_and(|age| (0.0..600.0).contains(&age)) {
                if let Some(skus) = d.get("skus").cloned().and_then(|v| gcp::objects(v).ok()) {
                    return Ok(Prices { region, skus, at });
                }
            }
        }
    }
    let out = ctx
        .run(
            &Cmd::new(&[
                "gcloud",
                "auth",
                "print-access-token",
                &format!("--project={project}"),
                "--quiet",
            ])
            .timeout(Some(30.0)),
        )
        .map_err(|_| unavailable("could not read gcloud credentials"))?;
    let token = out.stdout.trim();
    if out.code != 0
        || token.is_empty()
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
    {
        return Err(unavailable("gcloud authentication failed"));
    }
    let services = pages(ctx, token, &project, "services", "services")?;
    let service = services
        .iter()
        .find(|d| gcp::text(d, "displayName") == Some("Cloud TPU"))
        .and_then(|d| gcp::text(d, "name"))
        .filter(|s| s.strip_prefix("services/").is_some_and(gcp::component))
        .ok_or_else(|| unavailable("Cloud TPU service not found in Catalog"))?;
    let skus = pages(
        ctx,
        token,
        &project,
        &format!("{service}/skus?currencyCode=USD"),
        "skus",
    )?;
    let mut cache = Dict::new();
    cache.insert("at", Value::Int(at.as_second()));
    cache.insert(
        "skus",
        Value::List(skus.iter().cloned().map(Value::Dict).collect()),
    );
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_ok() {
            if let Ok(file) = tempfile::NamedTempFile::new_in(parent) {
                if std::fs::write(file.path(), pyjson::dumps(&Value::Dict(cache), None)).is_ok() {
                    let _ = file.persist(&path);
                }
            }
        }
    }
    Ok(Prices { region, skus, at })
}
pub fn job(ctx: &Ctx, cfg: &Dict) -> Job<std::sync::Arc<Prices>> {
    let worker = ctx.clone();
    let cfg = cfg.clone();
    Job::new(move || {
        if !crate::setup::gcp_ready(&cfg) {
            return Err("TPU prices need gpu setup".into());
        }
        let zone = gcp::Settings::load(&cfg)
            .map_err(|e| e.text().to_string())?
            .zone
            .ok_or("TPU prices need a configured zone; run gpu setup")?;
        load(&worker, &cfg, &zone)
            .map(std::sync::Arc::new)
            .map_err(|e| e.text().to_string())
    })
}
