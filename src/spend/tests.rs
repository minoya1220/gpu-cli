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
        let error = reserve(&ctx, budget(5.0, 0.5)).unwrap_err();
        assert_eq!(error.exit_code(), 15, "{}", error.text());
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

fn budget(cap: f64, estimate: f64) -> Option<Budget> {
    Some(Budget { cap, estimate })
}

#[test]
fn reserve_prunes_old_entries_and_claim_files_it_under_the_launch() {
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
    let first = reserve(&ctx, budget(2.0, 0.75)).unwrap();
    let second = reserve(&ctx, budget(2.0, 0.25)).unwrap();
    assert_eq!(window_total(&ctx).unwrap(), 2.0);
    claim_best_effort(&ctx, first, "vast:2");
    release_best_effort(&ctx, second);
    assert_eq!(
        stored(&ctx),
        [("vast:1".to_string(), 1.0), ("vast:2".to_string(), 0.75)]
    );
}

#[test]
fn reserve_refuses_over_the_cap_and_writes_nothing() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    let now = ctx.clock.now().as_second();
    seed(&ctx, &[("vast:1", now - 60, 1.5)]);
    let before = std::fs::read(ledger_path(&ctx.env)).unwrap();
    let error = reserve(&ctx, budget(2.0, 0.75)).unwrap_err();
    assert_eq!(error.exit_code(), 15, "{}", error.text());
    assert!(error.text().contains("$1.50"), "{}", error.text());
    assert_eq!(std::fs::read(ledger_path(&ctx.env)).unwrap(), before);
    assert!(reserve(&ctx, None).unwrap().is_none());
}

#[test]
fn an_unwritable_ledger_refuses_the_reservation() {
    use std::os::unix::fs::PermissionsExt;
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    let now = ctx.clock.now().as_second();
    seed(&ctx, &[("vast:1", now - 60, 0.5)]);
    let dir = ledger_path(&ctx.env).parent().unwrap().to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let result = reserve(&ctx, budget(5.0, 0.5));
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.exit_code(), 15, "{}", error.text());
    assert!(
        error.text().contains("cannot be written"),
        "{}",
        error.text()
    );
    assert_eq!(stored(&ctx), [("vast:1".to_string(), 0.5)]);
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
