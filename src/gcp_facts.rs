//! Non-price TPU facts fetched during setup and refreshed during launch.
//! Cached facts remain usable offline; paid preflight is always live.
use crate::{
    ctx::Ctx,
    error::{Error, Result},
    gcp,
    gcp_catalog::Zone,
    gcp_http,
    pyjson::{Dict, Value},
};
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
    if gcp::component(project) {
        if let Some(cache) =
            gcp_http::read_cache(ctx, &format!("tpu-catalog-{project}.json"), i64::MAX)
        {
            return parse_zones(&cache);
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
    gcp_http::read_cache(ctx, "tpu-sku-facts.json", i64::MAX)
        .map(|data| rows(&data, "skus"))
        .unwrap_or_default()
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
pub fn sku_generation(meta: &Dict) -> Option<&'static str> {
    let desc = gcp::text(meta, "description")?;
    let category = meta.get("category")?.as_dict()?;
    generation(&format!(
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
        .gcp_sku_refreshed
        .lock()
        .map_err(|_| Error::msg("Catalog refresh unavailable."))?;
    *refreshed = true;
    fetch_skus(ctx, project)
}
/// Refresh at most once per process; concurrent callers wait for that one.
pub fn refresh_skus_once(ctx: &Ctx, project: &str) {
    let Ok(mut refreshed) = ctx.gcp_sku_refreshed.lock() else {
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
    gcp_http::write_cache(ctx, "tpu-sku-facts.json", cache);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    #[test]
    fn every_refresh_fetches_metadata_and_failure_keeps_offline_facts() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let env = TestEnv::new();
        std::fs::create_dir_all(env.config.join("gpu")).unwrap();
        std::fs::write(
            env.config.join("gpu/config.json"),
            r#"{"gcp":{"project":"test-project","setup":{"version":1,"project":"test-project"}}}"#,
        )
        .unwrap();
        let round = Arc::new(AtomicUsize::new(0));
        let counter = round.clone();
        let runner = FakeRunner::new(move |cmd| {
            if cmd.program() == "gcloud" {
                return ok("test-token");
            }
            crate::tests::gcp_transport::curl(cmd, |url| {
                if url.contains("/skus?") {
                    let n = counter.load(Ordering::SeqCst);
                    (200, format!(r#"{{"skus":[{{"skuId":"sku-{n}","description":"TPU v6e chip","category":{{"resourceGroup":"TPU","usageType":"OnDemand"}},"serviceRegions":["us-east5"]}}]}}"#))
                } else {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    if n >= 2 { return (503, "{}".into()); }
                    (200, r#"{"services":[{"name":"services/test-service","displayName":"Cloud TPU"}]}"#.into())
                }
            }).unwrap()
        });
        let ctx = env.ctx().runner(&runner).build();
        for id in ["sku-1", "sku-2"] {
            refresh_skus(&ctx, "test-project").unwrap();
            assert_eq!(gcp::text(&skus(&ctx)[0], "id"), Some(id));
        }
        assert!(refresh_skus(&ctx, "test-project").is_err());
        assert_eq!(gcp::text(&skus(&ctx)[0], "id"), Some("sku-2"));
    }
    #[test]
    fn cached_sku_metadata_is_preserved() {
        let env = TestEnv::new();
        let ctx = env.ctx().build();
        assert!(skus(&ctx).is_empty());
        let mut cached = dict(r#"{"id":"test-sku"}"#);
        cached.insert("generation", Value::str("hand-edited-generation"));
        let id = gcp::text(&cached, "id").unwrap().to_string();
        let mut data = Dict::new();
        data.insert("skus", Value::List(vec![Value::Dict(cached.clone())]));
        gcp_http::write_cache(&ctx, "tpu-sku-facts.json", data);
        assert_eq!(
            skus(&ctx)
                .into_iter()
                .find(|s| gcp::text(s, "id") == Some(&id)),
            Some(cached)
        );
    }
    #[test]
    fn only_cached_zones_are_offered() {
        let env = TestEnv::new();
        let ctx = env.ctx().build();
        assert!(known_types(&ctx, "test-project", None).is_empty());
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
        assert!(known_types(&ctx, "test-project", None).is_empty());
    }
}
