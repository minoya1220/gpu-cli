use super::*;
use crate::testing::*;

fn seed(ctx: &Ctx, entries: &[(&str, i64, f64)]) {
    let entries: Vec<Entry> = entries
        .iter()
        .map(|(key, at, usd)| Entry {
            key: key.to_string(),
            at: *at,
            usd: *usd,
        })
        .collect();
    let path = ledger_path(&ctx.env);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, dump(&entries)).unwrap();
}

fn stored(ctx: &Ctx) -> Vec<(String, f64)> {
    let bytes = std::fs::read(ledger_path(&ctx.env)).unwrap_or_default();
    parse(&ctx.env, &bytes)
        .unwrap()
        .into_iter()
        .map(|e| (e.key, e.usd))
        .collect()
}

#[test]
fn window_total_sums_only_the_last_24_hours() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    let now = ctx.clock.now().as_second();
    assert_eq!(window_total(&ctx).unwrap(), 0.0);
    seed(
        &ctx,
        &[
            ("vast:1", now - 3600, 1.25),
            ("vast:2", now - WINDOW_SECONDS + 60, 0.5),
            ("vast:3", now - WINDOW_SECONDS - 1, 9.0),
        ],
    );
    assert_eq!(window_total(&ctx).unwrap(), 1.75);
}

#[test]
fn a_damaged_ledger_is_refused_and_left_alone() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    let path = ledger_path(&ctx.env);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let damaged: [&[u8]; 4] = [
        b"not json",
        b"\xff\xfe",
        br#"{"launches":{}}"#,
        br#"{"launches":[{"key":"vast:1","at":1,"usd":"lots"}]}"#,
    ];
    for bytes in damaged {
        std::fs::write(&path, bytes).unwrap();
        let error = window_total(&ctx).unwrap_err();
        assert_eq!(error.exit_code(), 15, "{}", error.text());
        assert!(error.text().contains("max_daily_spend"), "{}", error.text());
        assert!(record(&ctx, "vast:2", 0.5).is_err());
        assert!(settle(&ctx, "vast:1", 0.5).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[test]
fn an_unreadable_ledger_is_refused() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    std::fs::create_dir_all(ledger_path(&ctx.env)).unwrap();
    let error = window_total(&ctx).unwrap_err();
    assert_eq!(error.exit_code(), 15, "{}", error.text());
    assert!(error.text().contains("cannot be read"), "{}", error.text());
}

#[test]
fn record_prunes_old_entries_and_replaces_the_same_launch() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    let now = ctx.clock.now().as_second();
    seed(
        &ctx,
        &[
            ("vast:old", now - WINDOW_SECONDS - 10, 4.0),
            ("vast:1", now - 60, 1.0),
        ],
    );
    record(&ctx, "vast:2", 0.75).unwrap();
    record(&ctx, "vast:2", 0.5).unwrap();
    assert_eq!(
        stored(&ctx),
        [("vast:1".to_string(), 1.0), ("vast:2".to_string(), 0.5)]
    );
}

#[test]
fn settle_replaces_the_estimate_with_the_billed_amount() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    let now = ctx.clock.now().as_second();
    seed(
        &ctx,
        &[
            ("vast:1", now - 60, 8.0),
            ("vast:old", now - WINDOW_SECONDS - 10, 4.0),
        ],
    );
    settle(&ctx, "vast:1", 0.25).unwrap();
    settle(&ctx, "vast:unknown", 3.0).unwrap();
    assert_eq!(stored(&ctx), [("vast:1".to_string(), 0.25)]);
    assert_eq!(window_total(&ctx).unwrap(), 0.25);
}

#[test]
fn settle_without_a_ledger_creates_nothing() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    settle(&ctx, "vast:1", 1.0).unwrap();
    assert!(!ledger_path(&ctx.env).exists());
}
