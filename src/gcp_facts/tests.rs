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
    gcp_http::write_cache(&ctx, SKU_CACHE, data);
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
