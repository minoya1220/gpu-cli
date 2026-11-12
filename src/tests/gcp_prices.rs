use crate::{
    ctx::Ctx,
    gcp_prices::{self, Prices},
    picker::{testing::*, Job, Next, Outcome},
    process::Input,
    pyjson::{Dict, Value},
    testing::*,
};
use std::sync::{Arc, Mutex};
const CFG: &str = r#"{"gcp":{"setup":{"version":1,"project":"test-project"},"project":"test-project","zone":"us-east5-b"}}"#;
pub(crate) fn prices() -> Prices {
    Prices {
        region: "us-east5".into(),
        cache_age: None,
        clock: None,
        unavailable: None,
        at: "2026-11-03T00:00:00Z".parse().unwrap(),
        skus: dicts(
            r#"[
      {"description":"TPU v6e chip","category":{"usageType":"OnDemand"},"serviceRegions":["us-east5"],"pricingInfo":[{"effectiveTime":"2026-01-01T00:00:00Z","pricingExpression":{"usageUnit":"chip.h","tieredRates":[{"startUsageAmount":0,"unitPrice":{"currencyCode":"USD","units":"2","nanos":750000000}}]}}]},
      {"description":"TPU v6e chip Spot","category":{"usageType":"Preemptible"},"serviceRegions":["us-east5"],"pricingInfo":[{"effectiveTime":"2026-01-01T00:00:00Z","pricingExpression":{"usageUnit":"chip.h","tieredRates":[{"unitPrice":{"currencyCode":"USD","nanos":750000000}}]}}]},
      {"description":"TPU v5 Lite Pod","category":{"usageType":"OnDemand"},"serviceRegions":["us-east5"],"pricingInfo":[{"effectiveTime":"2026-01-01T00:00:00Z","pricingExpression":{"usageUnit":"h","usageUnitDescription":"hour","tieredRates":[{"unitPrice":{"currencyCode":"USD","units":"8"}}]}}]},
      {"description":"TPU v2 Device","category":{"usageType":"OnDemand"},"serviceRegions":["us-east5"],"pricingInfo":[{"effectiveTime":"2026-01-01T00:00:00Z","pricingExpression":{"usageUnit":"h","tieredRates":[{"unitPrice":{"currencyCode":"USD","units":"4"}}]}}]},
      {"description":"TPU v4 Pod","category":{"usageType":"OnDemand"},"serviceRegions":["us-east5"],"pricingInfo":[{"effectiveTime":"2026-01-01T00:00:00Z","pricingExpression":{"usageUnit":"h","tieredRates":[{"unitPrice":{"currencyCode":"USD","units":"10"}}]}}]}
    ]"#,
        ),
    }
}
pub(crate) fn cache(env: &TestEnv) {
    let p = prices();
    let ctx = env.ctx().clock(&FakeClock::at(p.at)).build();
    tpu_facts(&ctx);
    let entries = crate::gcp_facts::skus(&ctx)
        .into_iter()
        .filter(|m| {
            crate::gcp::text(m, "generation") == Some("v6e")
                && m.get("regions")
                    .and_then(Value::as_list)
                    .is_some_and(|v| v.contains(&Value::str("us-east5")))
                && crate::gcp_facts::capacity(m).is_some()
        })
        .map(|m| {
            let mut d = Dict::new();
            d.insert("id", m.get("id").unwrap().clone());
            d.insert("at", Value::Int(p.at.as_second()));
            d.insert(
                "sku",
                Value::Dict(p.skus[usize::from(crate::gcp_facts::capacity(&m).unwrap())].clone()),
            );
            Value::Dict(d)
        })
        .collect();
    let mut data = Dict::new();
    data.insert("entries", Value::List(entries));
    crate::gcp_http::write_cache(&ctx, "tpu-prices-test-project.json", data);
}
#[test]
fn normalizes_units_slice_sizes_and_capacity_without_guessing_missing_rates() {
    let p = prices();
    assert_eq!(p.hourly("v6e-4", false), Some(11.0));
    assert_eq!(p.hourly("v6e-4", true), Some(3.0));
    assert_eq!(p.hourly("v5e-1", false), Some(1.0));
    assert_eq!(p.hourly("v5litepod-8", false), Some(8.0));
    assert_eq!(p.hourly("v2-8", false), Some(4.0));
    assert_eq!(p.hourly("v4-32", false), Some(40.0));
    assert_eq!(p.hourly("v5e-1", true), None);
    assert_eq!(p.hourly("v99-1", false), None);
    assert!(p.label("v6e-1").contains("$2.750/hr  Spot $0.750/hr"));
    assert_eq!(
        p.category_label(&["v99-1"]),
        "TPU (Google Cloud)  -  Spot -"
    );
    assert_eq!(
        p.rate_columns(&["v5e-1"], true),
        ["from $1.000/hr", "Spot -"]
    );
    let mut ambiguous = p.clone();
    let mut duplicate = p.skus[0].clone();
    duplicate.insert("pricingInfo", json(r#"[{"effectiveTime":"2026-01-01T00:00:00Z","pricingExpression":{"usageUnit":"chip.h","tieredRates":[{"unitPrice":{"currencyCode":"USD","units":"999"}}]}}]"#));
    ambiguous.skus.push(duplicate);
    assert_eq!(ambiguous.hourly("v6e-1", false), None);
    let mut foreign = p;
    foreign.region = "other-region".into();
    assert_eq!(foreign.hourly("v6e-1", false), None);
}
#[test]
fn facts_and_prices_exclude_the_same_normalized_capacity_keywords() {
    let mut price = prices();
    price.skus.truncate(1);
    assert_eq!(price.hourly("v6e-1", false), Some(2.75));
    assert_eq!(crate::gcp_facts::capacity(&price.skus[0]), Some(false));
    for keyword in [
        "capacity optimized",
        "capacityoptimized",
        "Capacity-Optimized",
        "commit",
        "dws",
        "flex",
        "calendar",
        "reservation",
        "subscription",
    ] {
        price.skus[0].insert("description", Value::str(format!("TPU v6e chip {keyword}")));
        assert_eq!(
            crate::gcp_facts::capacity(&price.skus[0]),
            None,
            "{keyword}"
        );
        assert_eq!(price.hourly("v6e-1", false), None, "{keyword}");
    }
}

#[test]
fn future_tiered_monthly_or_malformed_money_is_never_quoted_as_an_hourly_rate() {
    for (at, unit, money, rates) in [
        (
            "2050-01-01T00:00:00Z",
            "chip.h",
            r#"{"currencyCode":"USD","units":"2"}"#,
            false,
        ),
        (
            "2020-01-01T00:00:00Z",
            "chip.month",
            r#"{"currencyCode":"USD","units":"2"}"#,
            false,
        ),
        (
            "2020-01-01T00:00:00Z",
            "chip.h",
            r#"{"currencyCode":"USD","units":"NaN"}"#,
            false,
        ),
        (
            "2020-01-01T00:00:00Z",
            "chip.h",
            r#"{"currencyCode":"EUR","units":"2"}"#,
            false,
        ),
        (
            "2020-01-01T00:00:00Z",
            "chip.h",
            r#"{"currencyCode":"USD","units":"2"}"#,
            true,
        ),
    ] {
        let mut p = prices();
        let mut expression = Dict::new();
        expression.insert("usageUnit", Value::str(unit));
        let mut rate = Dict::new();
        rate.insert("unitPrice", json(money));
        expression.insert(
            "tieredRates",
            Value::List(if rates {
                vec![Value::Dict(rate.clone()), Value::Dict(rate)]
            } else {
                vec![Value::Dict(rate)]
            }),
        );
        let mut info = Dict::new();
        info.insert("effectiveTime", Value::str(at));
        info.insert("pricingExpression", Value::Dict(expression));
        p.skus[0].insert("pricingInfo", Value::List(vec![Value::Dict(info)]));
        p.skus.truncate(1);
        assert_eq!(p.hourly("v6e-1", false), None, "{at} {unit} {money}");
    }
}

fn fixture() -> (TestEnv, Ctx, Arc<FakeRunner>) {
    let env = TestEnv::new();
    std::fs::create_dir(env.config.join("gpu")).unwrap();
    std::fs::write(env.config.join("gpu/config.json"), CFG).unwrap();
    let runner = FakeRunner::new(|cmd| {
        if cmd.program() == "gcloud" {
            assert_eq!(strs(&cmd.argv)[1..3], ["auth", "print-access-token"]);
            return ok("sensitive-test-token\n");
        }
        assert_eq!(cmd.program(), "curl");
        assert!(!cmd.argv.iter().any(|s| s.contains("sensitive-test-token")));
        let Input::Text(config) = &cmd.stdin else {
            panic!("no stdin auth")
        };
        assert!(config.contains("Authorization: Bearer sensitive-test-token"));
        assert!(config.contains("x-goog-user-project: test-project"));
        super::gcp_transport::curl(cmd,|url| {
            if url.contains("acceleratorTypes") {return (200,r#"{"acceleratorTypes":[{"type":"v6e-1"}]}"#.into());}
            if url.contains("/locations?") {return (200,r#"{"locations":[{"locationId":"us-east5-b"}]}"#.into());}
            let id=url.split("/skus/").nth(1).unwrap().split('/').next().unwrap();
            let env=TestEnv::new();let probe=env.ctx().build();tpu_facts(&probe);let meta=crate::gcp_facts::skus(&probe).into_iter().find(|m|crate::gcp::text(m,"id")==Some(id)).unwrap();
            let spot=crate::gcp_facts::capacity(&meta)==Some(true);
            (200,format!(r#"{{"currencyCode":"USD","rate":{{"unitInfo":{{"unit":"h","unitDescription":"hour","unitQuantity":{{"value":"1"}}}},"tiers":[{{"startAmount":{{"value":"0"}},"listPrice":{{"currencyCode":"USD","units":"{}","nanos":750000000}}}}]}}}}"#,if spot {0} else {2}))
        }).unwrap()
    });
    let ctx = env
        .ctx()
        .runner(&runner)
        .clock(&FakeClock::at(prices().at))
        .build();
    tpu_facts(&ctx);
    (env, ctx, runner)
}
#[test]
fn known_skus_batch_prices_and_cache_public_data_not_tokens() {
    let (env, ctx, runner) = fixture();
    let cfg = dict(CFG);
    let p = gcp_prices::load(&ctx, &cfg, "us-east5-b").unwrap();
    assert_eq!(p.hourly("v6e-1", false), Some(2.75));
    assert_eq!(runner.calls().len(), 2);
    assert_eq!(
        gcp_prices::load(&ctx, &cfg, "us-east5-b")
            .unwrap()
            .hourly("v6e-1", true),
        Some(0.75)
    );
    assert_eq!(runner.calls().len(), 2);
    let raw = read(&env.cache.join("gpu/tpu-prices-test-project.json"));
    assert!(!raw.contains("sensitive-test-token"));
    assert!(!raw.contains("Authorization"));
    assert!(!out(&ctx).contains("sensitive-test-token"));
}
#[test]
fn gate_applies_even_to_cached_prices_and_errors_do_not_leak_credentials() {
    let env = TestEnv::new();
    cache(&env);
    let ctx = env.ctx().build();
    assert!(gcp_prices::load(&ctx, &dict(CFG), "us-east5-b").is_err());
    let (env, _ctx, _runner) = fixture();
    let runner = FakeRunner::new(|_| {
        Ok(crate::process::Output::failed(
            1,
            "token=private credential",
        ))
    });
    let ctx = env.ctx().runner(&runner).build();
    let error = gcp_prices::load(&ctx, &dict(CFG), "us-east5-b")
        .err()
        .unwrap()
        .text()
        .to_string();
    assert!(!error.contains("private"));
}
struct EmptySearch;
impl crate::wizard::Search for EmptySearch {
    fn offers(&self, _: &str, _: Option<&str>) -> crate::error::Result<Vec<Dict>> {
        Ok(Vec::new())
    }
    fn stats(&self, _: &str) -> crate::error::Result<crate::offers::Stats> {
        Ok(Default::default())
    }
}

#[test]
fn tpu_prices_load_automatically_and_no_setup_note_is_visible() {
    for configured in [false, true] {
        let env = TestEnv::new();
        let ctx = env.ctx().build();
        let job = if configured {
            Job::new(|| Ok(Arc::new(prices())))
        } else {
            gcp_prices::job(&ctx, &Dict::new())
        };
        let wizard = crate::wizard::LaunchWizard::new(
            &env.env(),
            crate::wizard::WizardOptions {
                repos: Job::new(|| Ok(Vec::new())),
                search: Some(Arc::new(EmptySearch)),
                models: vec!["RTX 4090".into()],
                recent_gpus: Vec::new(),
                choose_offer: false,
                tpu_setup_error: (!configured).then(|| "needs gpu setup".into()),
                tpu_transition: None,
                tpu_prices: Some(job),
                tpu_cached_prices: None,
                tpu_types: if configured {
                    vec!["v6e-1".into()]
                } else {
                    Vec::new()
                },
                tpu_catalog: None,
                owner: "example".into(),
            },
        );
        let Next::Screen(screen) = wizard.next(crate::wizard::Picks::new(
            Some(crate::repos::Repo::NoRepo),
            Some("container"),
            None,
        )) else {
            panic!("no screen")
        };
        let seen = Arc::new(Mutex::new(false));
        let watched = seen.clone();
        let result = drive(screen, move |keys| {
            keys.type_text("tpu");
            assert!(!keys.footer().contains("F2"));
            if configured {
                keys.wait(|s| s.labels.iter().any(|l| l.contains("Spot $0.750/hr")));
            } else {
                keys.wait(|s| s.labels.iter().any(|l| l.contains("needs gpu setup")));
            }
            *watched.lock().unwrap() = true;
            keys.press(CTRL_C);
        });
        assert_eq!(result, Outcome::Cancelled(130));
        assert!(*seen.lock().unwrap());
    }
}

#[test]
fn shared_token_catalog_cache_and_fresh_preflight_do_not_duplicate_auth() {
    let (_env, ctx, runner) = fixture();
    let cfg = dict(CFG);
    gcp_prices::load(&ctx, &cfg, "us-east5-b").unwrap();
    crate::gcp_catalog::zones(&ctx, "test-project", None).unwrap();
    let before = runner.calls().len();
    crate::gcp_catalog::cached_types(&ctx, "test-project", "us-east5-b").unwrap();
    assert_eq!(runner.calls().len(), before);
    crate::gcp_catalog::verify(&ctx, "test-project", "us-east5-b", "v6e-1").unwrap();
    assert_eq!(
        runner.calls().len(),
        before + 1,
        "preflight must make a fresh read"
    );
    assert_eq!(
        runner
            .calls()
            .iter()
            .filter(|c| c.program() == "gcloud")
            .count(),
        1
    );
}
#[test]
fn compute_engine_hours_are_chip_hours_and_regions_share_sku_cache() {
    let (_env, ctx, runner) = fixture();
    let cfg = dict(CFG);
    let p = gcp_prices::load_for(&ctx, &cfg, "us-east5-b", Some(("v6e-1", false))).unwrap();
    assert_eq!(p.hourly("v6e-4", false), Some(11.0));
    let count = runner.calls().len();
    let cached = gcp_prices::load_for(&ctx, &cfg, "us-east5-a", Some(("v6e-1", false))).unwrap();
    assert_eq!(runner.calls().len(), count);
    assert_eq!(cached.cache_age, Some(0));
}
#[test]
fn live_prices_use_cached_skus_during_refresh_and_reprice_only_on_change() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for changed in [false, true] {
        let env = TestEnv::new();
        std::fs::create_dir(env.config.join("gpu")).unwrap();
        std::fs::write(env.config.join("gpu/config.json"), CFG).unwrap();
        cache(&env);
        let release = super::picker::Gate::default();
        let held = release.clone();
        let listings = Arc::new(AtomicUsize::new(0));
        let counted = listings.clone();
        let runner = FakeRunner::new(move |cmd| {
            if cmd.program() == "gcloud" {
                return ok("test-token\n");
            }
            super::gcp_transport::curl(cmd, |url| {
                if url.contains("/skus?") {
                    let demand = if changed { "test-new-demand" } else { "test-demand" };
                    return (200, format!(r#"{{"skus":[{{"skuId":"{demand}","description":"TPU v6e chip","category":{{"usageType":"OnDemand","resourceGroup":"TPU"}},"serviceRegions":["us-east5"]}},{{"skuId":"test-spot","description":"TPU v6e chip Spot","category":{{"usageType":"Preemptible","resourceGroup":"TPU"}},"serviceRegions":["us-east5"]}}]}}"#));
                }
                if url.contains("/price?") {
                    let units = if url.contains("/skus/test-new-demand/") { 9 } else { 1 };
                    return (200, format!(r#"{{"currencyCode":"USD","rate":{{"unitInfo":{{"unit":"h","unitDescription":"hour","unitQuantity":{{"value":"1"}}}},"tiers":[{{"startAmount":{{"value":"0"}},"listPrice":{{"currencyCode":"USD","units":"{units}"}}}}]}}}}"#));
                }
                assert!(url.contains("/v1/services?"), "unexpected {url}");
                counted.fetch_add(1, Ordering::SeqCst);
                assert!(held.wait());
                (200, r#"{"services":[{"name":"services/test-service","displayName":"Cloud TPU"}]}"#.into())
            })
            .unwrap()
        });
        let ctx = env
            .ctx()
            .runner(&runner)
            .clock(&FakeClock::at(prices().at))
            .build();
        let cfg = dict(CFG);
        let job = gcp_prices::job(&ctx, &cfg);
        let again = gcp_prices::job_at(&ctx, &cfg, Some("us-east5-a"));
        let (rows, observer) = (job.clone(), job.observer());
        let screen = crate::picker::Screen::new(
            "Prices",
            move || {
                let rate = rows.result().and_then(|p| p.hourly("v6e-1", false));
                vec![crate::picker::Opt::new(String::new(), format!("{rate:?}"))]
            },
            Next::Done,
        )
        .prices(job)
        .prices(again.clone());
        let result = drive(screen, move |keys| {
            keys.wait(|s| s.labels == ["Some(2.75)"]);
            assert_eq!(observer.state(), crate::picker::JobState::Running);
            release.set();
            let expected = if changed { "Some(2.25)" } else { "Some(2.75)" };
            keys.wait(|s| s.labels == [expected]);
            keys.wait_for("refresh settles", || {
                observer.state() == crate::picker::JobState::Done
            });
            keys.press(CTRL_C);
        });
        assert_eq!(result, Outcome::Cancelled(130));
        assert_eq!(listings.load(Ordering::SeqCst), 1, "one refresh per launch");
    }
}
