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
    let text = std::fs::read_to_string(ledger_path(&ctx.env)).unwrap_or_default();
    parse(&text).into_iter().map(|e| (e.key, e.usd)).collect()
}

#[test]
fn window_total_sums_only_the_last_24_hours() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    let now = ctx.clock.now().as_second();
    assert_eq!(window_total(&ctx), 0.0);
    seed(
        &ctx,
        &[
            ("vast:1", now - 3600, 1.25),
            ("vast:2", now - WINDOW_SECONDS + 60, 0.5),
            ("vast:3", now - WINDOW_SECONDS - 1, 9.0),
        ],
    );
    assert_eq!(window_total(&ctx), 1.75);
}

#[test]
fn a_corrupt_ledger_counts_as_empty_and_is_rewritten() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    let path = ledger_path(&ctx.env);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "not json").unwrap();
    assert_eq!(window_total(&ctx), 0.0);
    record(&ctx, "vast:1", 0.5).unwrap();
    assert_eq!(stored(&ctx), [("vast:1".to_string(), 0.5)]);
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
    assert_eq!(window_total(&ctx), 0.25);
}

#[test]
fn settle_without_a_ledger_creates_nothing() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    settle(&ctx, "vast:1", 1.0).unwrap();
    assert!(!ledger_path(&ctx.env).exists());
}
