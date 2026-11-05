//! Generated, non-price facts seed every screen. Live cache entries override
//! known zones and extend types/SKU metadata; paid preflight is always live.
use crate::{
    ctx::Ctx,
    error::{Error, Result},
    gcp,
    gcp_catalog::Zone,
    gcp_http,
    pyjson::{self, Dict, Value},
};
use std::{collections::BTreeMap, sync::OnceLock};
fn bundled() -> &'static Dict {
    static DATA: OnceLock<Dict> = OnceLock::new();
    DATA.get_or_init(|| {
        pyjson::loads(include_str!("data/tpu_facts.json"))
            .expect("generated TPU facts JSON")
            .as_dict()
            .expect("TPU facts object")
            .clone()
    })
}
fn rows(d: &Dict, key: &str) -> Vec<Dict> {
    d.get(key)
        .cloned()
        .and_then(|v| gcp::objects(v).ok())
        .unwrap_or_default()
}
fn parse_zones(data: &Dict) -> Vec<Zone> {
    rows(data, "zones")
        .into_iter()
        .filter_map(|d| {
            let name = gcp::text(&d, "name").filter(|s| gcp::component(s))?.into();
            let types = d
                .get("types")?
                .as_list()?
                .iter()
                .map(|v| match v {
                    Value::Str(s) if gcp::component(s) => Some(s.clone()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            Some(Zone { name, types })
        })
        .collect()
}
pub fn known_zones(ctx: &Ctx, project: &str) -> Vec<Zone> {
    let mut zones = parse_zones(bundled())
        .into_iter()
        .map(|z| (z.name.clone(), z))
        .collect::<BTreeMap<_, _>>();
    if gcp::component(project) {
        if let Some(cache) =
            gcp_http::read_cache(ctx, &format!("tpu-catalog-{project}.json"), i64::MAX)
        {
            for zone in parse_zones(&cache) {
                zones.insert(zone.name.clone(), zone);
            }
        }
    }
    zones.into_values().collect()
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
pub fn region_coordinates(region: &str) -> Option<(f64, f64)> {
    let r = bundled()
        .get("regions")?
        .as_list()?
        .iter()
        .filter_map(Value::as_dict)
        .find(|d| gcp::text(d, "name") == Some(region))?;
    let lat = crate::pyfmt::py_float(r.get("latitude")?).ok()?;
    let lon = crate::pyfmt::py_float(r.get("longitude")?).ok()?;
    (lat.is_finite() && lon.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0)
        .then_some((lat, lon))
}
pub fn skus(ctx: &Ctx) -> Vec<Dict> {
    let mut skus = rows(bundled(), "skus")
        .into_iter()
        .filter_map(|s| Some((gcp::text(&s, "id")?.to_string(), s)))
        .collect::<BTreeMap<_, _>>();
    if let Some(data) = gcp_http::read_cache(ctx, "tpu-sku-facts.json", i64::MAX) {
        for sku in rows(&data, "skus") {
            if let Some(id) = gcp::text(&sku, "id").filter(|s| gcp::component(s)) {
                skus.insert(id.into(), sku);
            }
        }
    }
    skus.into_values().collect()
}
pub fn generation(text: &str) -> Option<&'static str> {
    let s = text
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>();
    if s.contains("v6e") || s.contains("trillium") {
        Some("v6e")
    } else if s.contains("v5enterprise") || s.contains("v5p") {
        Some("v5p")
    } else if s.contains("v5lite") || s.contains("v5e") {
        Some("v5e")
    } else if s.contains("v4") {
        Some("v4")
    } else if s.contains("v3") {
        Some("v3")
    } else if s.contains("v2") {
        Some("v2")
    } else {
        None
    }
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
        generation(&format!(
            "{desc} {}",
            gcp::text(category, "resourceGroup").unwrap_or("")
        ))
        .map(Value::str)
        .unwrap_or(Value::Null),
    );
    Some(d)
}
pub fn refresh_skus(ctx: &Ctx, project: &str) -> Result<()> {
    ctx.require_gcloud()?;
    let _lock = ctx
        .gcp_sku_lock
        .lock()
        .map_err(|_| Error::msg("Catalog refresh unavailable."))?;
    if gcp_http::read_cache(ctx, "tpu-sku-facts.json", 30 * 86400).is_some() {
        return Ok(());
    }
    let generated =
        gcp::text(bundled(), "generated_at").and_then(|s| s.parse::<jiff::Timestamp>().ok());
    if generated
        .is_some_and(|t| (0..30 * 86400).contains(&(ctx.clock.now().as_second() - t.as_second())))
    {
        return Ok(());
    }
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
    let mut cache = Dict::new();
    cache.insert("services", Value::List(services));
    cache.insert("skus", Value::List(skus));
    gcp_http::write_cache(ctx, "tpu-sku-facts.json", cache);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    #[test]
    fn generated_snapshot_has_facts_not_prices_and_cache_overrides_known_zones() {
        let text = include_str!("data/tpu_facts.json");
        for forbidden in [
            "unitPrice",
            "tieredRates",
            "pricingInfo",
            "Authorization",
            "gcp-sandbox",
        ] {
            assert!(!text.contains(forbidden), "{forbidden}");
        }
        assert!(rows(bundled(), "zones").len() > 20);
        assert!(rows(bundled(), "skus").len() > 100);
        let env = TestEnv::new();
        let ctx = env.ctx().build();
        assert!(!known_types(&ctx, "test-project", None).is_empty());
        let mut cache = dict(
            r#"{"zones":[{"name":"us-east5-b","types":[]},{"name":"future-region1-a","types":["v99-1"]}]}"#,
        );
        gcp_http::write_cache(&ctx, "tpu-catalog-test-project.json", cache.clone());
        assert!(known_types(&ctx, "test-project", Some("us-east5-b")).is_empty());
        assert_eq!(
            known_types(&ctx, "test-project", Some("future-region1-a")),
            ["v99-1"]
        );
        cache.insert("zones", Value::Null);
        gcp_http::write_cache(&ctx, "tpu-catalog-test-project.json", cache);
        assert!(!known_types(&ctx, "test-project", None).is_empty());
    }
}
