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
    let prices = prices();
    let mut d = Dict::new();
    d.insert("at", Value::Int(prices.at.as_second()));
    d.insert(
        "skus",
        Value::List(prices.skus.into_iter().map(Value::Dict).collect()),
    );
    std::fs::create_dir_all(env.cache.join("gpu")).unwrap();
    std::fs::write(
        env.cache.join("gpu/tpu-prices-test-project-us-east5.json"),
        crate::pyjson::dumps(&Value::Dict(d), None),
    )
    .unwrap();
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
    assert!(p.label("v6e-1").contains("on-demand $2.750 / Spot $0.750"));
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
        let url = cmd.argv.last().unwrap();
        assert!(url.starts_with("https://cloudbilling.googleapis.com/v1/services"));
        if url.contains("/skus?") {
            let p = prices();
            let mut page = Dict::new();
            page.insert(
                "skus",
                Value::List(p.skus.into_iter().map(Value::Dict).collect()),
            );
            return ok(&crate::pyjson::dumps(&Value::Dict(page), None));
        }
        if url.contains("pageToken=next%2F%2B") {
            return ok(r#"{"services":[{"name":"services/TEST-TPU","displayName":"Cloud TPU"}]}"#);
        }
        ok(r#"{"services":[],"nextPageToken":"next/+"}"#)
    });
    let ctx = env
        .ctx()
        .runner(&runner)
        .clock(&FakeClock::at(prices().at))
        .build();
    (env, ctx, runner)
}
#[test]
fn authenticated_catalog_paginates_and_caches_public_data_not_tokens() {
    let (env, ctx, runner) = fixture();
    let cfg = dict(CFG);
    let p = gcp_prices::load(&ctx, &cfg, "us-east5-b").unwrap();
    assert_eq!(p.hourly("v6e-1", false), Some(2.75));
    assert_eq!(runner.calls().len(), 4);
    assert_eq!(
        gcp_prices::load(&ctx, &cfg, "us-east5-b")
            .unwrap()
            .hourly("v6e-1", true),
        Some(0.75)
    );
    assert_eq!(runner.calls().len(), 4);
    let raw = read(&env.cache.join("gpu/tpu-prices-test-project-us-east5.json"));
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
fn f2_on_tpu_rows_has_its_own_job_and_no_setup_note_is_visible() {
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
                tpu_setup_error: None,
                tpu_prices: Some(job),
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
            keys.press(F2); // a completed Vast job must not hide TPU's F2 hint
            keys.wait(|s| !s.footer.contains("F2") && !s.footer.contains("Loading prices"));
            keys.type_text("tpu");
            assert!(keys.footer().contains("F2 prices"));
            keys.press(F2);
            if configured {
                keys.wait(|s| s.labels.iter().any(|l| l.contains("Spot $0.750")));
            } else {
                keys.wait(|s| s.footer.contains("TPU prices need gpu setup"));
            }
            *watched.lock().unwrap() = true;
            keys.press(CTRL_C);
        });
        assert_eq!(result, Outcome::Cancelled(130));
        assert!(*seen.lock().unwrap());
    }
}
