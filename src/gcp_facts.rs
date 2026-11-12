//! Non-price TPU facts fetched during setup and refreshed during launch.
//! Cached facts remain usable offline; paid preflight is always live.
use crate::cache;
use crate::{
    ctx::Ctx,
    error::{Error, Result},
    gcp,
    gcp_catalog::Zone,
    gcp_http,
    pyjson::{Dict, Value},
};
pub(crate) const SKU_CACHE: &str = "tpu-sku-facts.json";

fn rows(d: &Dict, key: &str) -> Vec<Dict> {
    d.get(key)
        .cloned()
        .and_then(|v| gcp::objects(v).ok())
        .unwrap_or_default()
}
pub fn known_zones(ctx: &Ctx, project: &str) -> Vec<Zone> {
    if gcp::component(project) {
        if let Some(cache) = cache::read_json(ctx, &crate::gcp_catalog::file(project), i64::MAX) {
            return rows(&cache, "zones")
                .iter()
                .filter_map(crate::gcp_catalog::parse_zone)
                .collect();
        }
    }
    Vec::new()
}
pub fn known_types(ctx: &Ctx, project: &str, zone: Option<&str>) -> Vec<String> {
    let mut types = known_zones(ctx, project)
        .into_iter()
        .filter(|z| zone.is_none_or(|name| z.name == name))
        .flat_map(|z| z.types)
        .collect::<Vec<_>>();
    types.sort();
    types.dedup();
    types
}
pub fn skus(ctx: &Ctx) -> Vec<Dict> {
    cache::read_json(ctx, SKU_CACHE, i64::MAX)
        .map(|data| rows(&data, "skus"))
        .unwrap_or_default()
}
pub fn sku_generation(meta: &Dict) -> Option<&'static str> {
    let desc = gcp::text(meta, "description")?;
    let category = meta.get("category")?.as_dict()?;
    crate::gcp_types::classify(&format!(
        "{desc} {}",
        gcp::text(category, "resourceGroup").unwrap_or("")
    ))
}
pub fn capacity(meta: &Dict) -> Option<bool> {
    let desc = gcp::text(meta, "description")?.to_ascii_lowercase();
    let compact = desc
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>();
    if [
        "commit",
        "dws",
        "flex",
        "calendar",
        "reservation",
        "subscription",
        "capacityoptimized",
    ]
    .iter()
    .any(|s| compact.contains(s))
    {
        return None;
    }
    let cat = meta.get("category")?.as_dict()?;
    match gcp::text(cat, "usageType")? {
        "OnDemand" => Some(false),
        "Preemptible" | "Spot" => Some(true),
        _ => None,
    }
}
fn sku_fact(service: &str, sku: &Dict) -> Option<Dict> {
    let desc = gcp::text(sku, "description")?;
    let category = sku.get("category")?.as_dict()?;
    if !format!(
        "{desc} {}",
        gcp::text(category, "resourceGroup").unwrap_or("")
    )
    .to_ascii_lowercase()
    .contains("tpu")
    {
        return None;
    }
    let mut d = Dict::new();
    d.insert("id", sku.get("skuId")?.clone());
    d.insert("service", Value::str(service));
    d.insert("description", Value::str(desc));
    d.insert("category", Value::Dict(category.clone()));
    d.insert(
        "regions",
        sku.get("serviceRegions")
            .cloned()
            .unwrap_or(Value::List(Vec::new())),
    );
    d.insert(
        "generation",
        sku_generation(sku).map(Value::str).unwrap_or(Value::Null),
    );
    Some(d)
}
pub fn refresh_skus(ctx: &Ctx, project: &str) -> Result<()> {
    let mut refreshed = ctx
        .gcp
        .sku_refreshed
        .lock()
        .map_err(|_| Error::msg("Catalog refresh unavailable."))?;
    *refreshed = true;
    fetch_skus(ctx, project)
}
/// Refresh at most once per process; concurrent callers wait for that one.
pub fn refresh_skus_once(ctx: &Ctx, project: &str) {
    let Ok(mut refreshed) = ctx.gcp.sku_refreshed.lock() else {
        return;
    };
    if !*refreshed {
        *refreshed = true;
        let _ = fetch_skus(ctx, project);
    }
}
fn fetch_skus(ctx: &Ctx, project: &str) -> Result<()> {
    ctx.require_gcloud()?;
    let listed = gcp_http::pages(
        ctx,
        project,
        "https://cloudbilling.googleapis.com/v1/services",
        "services",
        5000,
    )?;
    let selected = listed
        .into_iter()
        .filter(|s| {
            matches!(
                gcp::text(s, "displayName"),
                Some("Cloud TPU" | "Compute Engine")
            )
        })
        .collect::<Vec<_>>();
    let mut services = Vec::new();
    let mut skus = Vec::new();
    for service in selected {
        let name = gcp::text(&service, "name")
            .filter(|s| s.strip_prefix("services/").is_some_and(gcp::component))
            .ok_or_else(gcp::bad_reply)?;
        let fields =
            gcp_http::encode("skus(skuId,description,category,serviceRegions),nextPageToken");
        for sku in gcp_http::pages(
            ctx,
            project,
            &format!("https://cloudbilling.googleapis.com/v1/{name}/skus?fields={fields}"),
            "skus",
            5000,
        )? {
            if let Some(s) = sku_fact(name, &sku) {
                skus.push(Value::Dict(s));
            }
        }
        let mut d = Dict::new();
        d.insert("name", Value::str(name));
        d.insert(
            "display_name",
            service.get("displayName").cloned().unwrap_or(Value::Null),
        );
        services.push(Value::Dict(d));
    }
    if skus.is_empty() {
        return Err(gcp::bad_reply());
    }
    let mut cache = Dict::new();
    cache.insert("services", Value::List(services));
    cache.insert("skus", Value::List(skus));
    cache::write_json(ctx, SKU_CACHE, cache);
    Ok(())
}
#[cfg(test)]
mod tests;
