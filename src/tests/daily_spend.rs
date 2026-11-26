//! `max_daily_spend`: every launch is fake, nothing can rent or bill.
use crate::ctx::{Clock, Ctx};
use crate::process::Output;
use crate::testing::*;
use std::sync::Arc;

const OFFER: &str = r#"[{"id":42,"gpu_name":"Tesla T4","num_gpus":1,"dph_total":0.1}]"#;
// 2h at $0.10/hr.
const ESTIMATE: f64 = 0.2;
const UP: &[&str] = &[
    "up",
    "--no-repo",
    "--mode",
    "container",
    "--gpu",
    "Tesla T4",
    "--yes",
    "--expire",
    "2h",
];

fn fixture(cap: &str) -> (TestEnv, Ctx, Arc<FakeRunner>) {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|cmd| match strs(&cmd.argv)[..] {
        ["vastai", "search", "offers", ..] => ok(OFFER),
        ["vastai", "create", "instance", ..] => ok(r#"{"success":true,"new_contract":123}"#),
        ["vastai", "show", "instances", "--raw"] => {
            ok(r#"[{"id":123,"actual_status":"running","ssh_host":"ssh1.vast.ai","ssh_port":22}]"#)
        }
        ["vastai", "ssh-url", ..] => ok("ssh://root@ssh1.vast.ai:22"),
        ["ssh", ..] | ["scp", ..] => ok(r#"{"status":"armed","deadline":4102444800}"#),
        _ => panic!("unexpected command: {:?}", cmd.argv),
    });
    let ctx = t.ctx().runner(&runner).build();
    let path = crate::config::config_path(&ctx.env);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!(r#"{{"max_daily_spend":{cap}}}"#)).unwrap();
    (t, ctx, runner)
}

fn seed(ctx: &Ctx, entries: &[(&str, i64, f64)]) {
    let list: Vec<String> = entries
        .iter()
        .map(|(key, at, usd)| format!(r#"{{"key":"{key}","at":{at},"usd":{usd}}}"#))
        .collect();
    let path = crate::spend::ledger_path(&ctx.env);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!(r#"{{"launches":[{}]}}"#, list.join(","))).unwrap();
}

fn ledger(ctx: &Ctx) -> String {
    std::fs::read_to_string(crate::spend::ledger_path(&ctx.env)).unwrap_or_default()
}

fn created(runner: &FakeRunner) -> bool {
    runner
        .argvs()
        .iter()
        .any(|a| a.get(1).is_some_and(|s| s == "create"))
}

#[test]
fn a_launch_under_the_cap_is_recorded_at_its_estimate() {
    let (_t, ctx, runner) = fixture("0.5");
    let now = ctx.clock.now().as_second();
    seed(&ctx, &[("vast:900", now - 3600, 0.25)]);
    assert_eq!(main(&ctx, UP), 0, "{}", err(&ctx));
    assert!(created(&runner));
    let text = ledger(&ctx);
    assert!(text.contains(r#""key": "vast:900""#), "{text}");
    assert!(text.contains(r#""key": "vast:123""#), "{text}");
    assert!((crate::spend::window_total(&ctx).unwrap() - (0.25 + ESTIMATE)).abs() < 0.01);
}

#[test]
fn a_launch_over_the_cap_is_refused_with_the_amounts() {
    let (_t, ctx, runner) = fixture("0.3");
    let now = ctx.clock.now().as_second();
    seed(&ctx, &[("vast:900", now - 3600, 0.25)]);
    let before = ledger(&ctx);
    assert_eq!(main(&ctx, UP), 15, "{}", err(&ctx));
    let message = err(&ctx);
    for amount in ["$0.25", "$0.20", "$0.30"] {
        assert!(message.contains(amount), "{amount}: {message}");
    }
    assert!(message.contains("max_daily_spend"), "{message}");
    assert!(!created(&runner));
    assert_eq!(ledger(&ctx), before);
}

#[test]
fn a_damaged_ledger_refuses_the_launch() {
    let (_t, ctx, runner) = fixture("50");
    let path = crate::spend::ledger_path(&ctx.env);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{truncated").unwrap();
    assert_eq!(main(&ctx, UP), 15, "{}", err(&ctx));
    assert!(err(&ctx).contains("spend ledger"), "{}", err(&ctx));
    assert!(!created(&runner));
    assert_eq!(ledger(&ctx), "{truncated");
}

#[test]
fn launches_older_than_24_hours_do_not_count() {
    let (_t, ctx, runner) = fixture("0.3");
    let now = ctx.clock.now().as_second();
    seed(&ctx, &[("vast:900", now - 25 * 3600, 50.0)]);
    assert_eq!(main(&ctx, UP), 0, "{}", err(&ctx));
    assert!(created(&runner));
    let text = ledger(&ctx);
    assert!(!text.contains("vast:900"), "pruned: {text}");
    assert!(text.contains("vast:123"), "{text}");
}

#[test]
fn dry_run_reports_the_window_and_records_nothing() {
    let (_t, ctx, runner) = fixture("0.5");
    let now = ctx.clock.now().as_second();
    seed(&ctx, &[("vast:900", now - 3600, 0.25)]);
    let before = ledger(&ctx);
    let mut args = UP.to_vec();
    args.push("--dry-run");
    assert_eq!(main(&ctx, &args), 0, "{}", err(&ctx));
    let stdout = out(&ctx);
    assert!(
        stdout.contains(
            "Daily budget: $0.25 estimated spent in the last 24h of $0.50; $0.25 remaining."
        ),
        "{stdout}"
    );
    assert!(!created(&runner));
    assert_eq!(ledger(&ctx), before);
}

#[test]
fn dry_run_without_a_ledger_writes_none() {
    let (_t, ctx, _runner) = fixture("0.5");
    let mut args = UP.to_vec();
    args.push("--dry-run");
    assert_eq!(main(&ctx, &args), 0, "{}", err(&ctx));
    let stdout = out(&ctx);
    assert!(stdout.contains("$0.00 estimated spent"), "{stdout}");
    assert!(!crate::spend::ledger_path(&ctx.env).exists());
}

#[test]
fn a_dry_run_over_the_cap_is_refused_like_a_launch() {
    let (_t, ctx, _runner) = fixture("0.1");
    let mut args = UP.to_vec();
    args.extend(["--dry-run", "--json"]);
    assert_eq!(main(&ctx, &args), 15, "{}", err(&ctx));
}

#[test]
fn an_unestimable_launch_fails_closed() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    let mut cfg = crate::pyjson::Dict::new();
    cfg.insert("max_daily_spend", crate::pyjson::Value::Int(5));
    for (rate, deadline) in [
        (None, Some(ctx.clock.now())),
        (Some(1.0), None),
        (Some(f64::NAN), Some(ctx.clock.now())),
    ] {
        let error = crate::guardrails::spending(&ctx, &cfg, rate, deadline).unwrap_err();
        assert!(error.text().contains("max_daily_spend"), "{}", error.text());
    }
}

#[test]
fn an_invalid_cap_is_a_usage_error() {
    let (_t, ctx, runner) = fixture("-1");
    assert_eq!(main(&ctx, UP), 2, "{}", err(&ctx));
    assert!(!created(&runner));
}

#[test]
fn without_the_cap_nothing_is_recorded() {
    let t = TestEnv::new();
    let runner = FakeRunner::new(|cmd| match strs(&cmd.argv)[..] {
        ["vastai", "search", "offers", ..] => ok(OFFER),
        ["vastai", "create", "instance", ..] => ok(r#"{"success":true,"new_contract":123}"#),
        ["vastai", "show", "instances", "--raw"] => {
            ok(r#"[{"id":123,"actual_status":"running","ssh_host":"h","ssh_port":22}]"#)
        }
        ["vastai", "ssh-url", ..] => ok("ssh://root@h:22"),
        _ => ok(r#"{"status":"armed","deadline":4102444800}"#),
    });
    let ctx = t.ctx().runner(&runner).build();
    assert_eq!(main(&ctx, UP), 0, "{}", err(&ctx));
    assert!(!crate::spend::ledger_path(&ctx.env).exists());
}

#[test]
fn destroying_a_launch_replaces_its_estimate_with_the_billed_amount() {
    let t = TestEnv::new();
    let clock = FakeClock::now_real();
    let started = clock.now().as_second() - 3600;
    let destroyed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = destroyed.clone();
    let runner = FakeRunner::new(move |cmd| match strs(&cmd.argv)[..] {
        ["vastai", "show", "instances", "--raw"]
            if flag.load(std::sync::atomic::Ordering::SeqCst) =>
        {
            ok("[]")
        }
        ["vastai", "show", "instances", "--raw"] => ok(&format!(
            r#"[{{"id":12,"gpu_name":"RTX 4090","actual_status":"running","dph_total":0.36,"start_date":{started}}}]"#
        )),
        ["vastai", "destroy", ..] => {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            ok("ok")
        }
        _ => Ok(Output::failed(1, "unexpected")),
    });
    let ctx = t.ctx().runner(&runner).clock(&clock).build();
    let now = clock.now().as_second();
    seed(
        &ctx,
        &[("vast:12", now - 3600, 8.0), ("vast:13", now - 60, 1.0)],
    );
    assert_eq!(main(&ctx, &["down", "12", "--yes"]), 0, "{}", err(&ctx));
    // 1h at $0.36/hr replaces the $8 estimate; the other launch is untouched.
    let total = crate::spend::window_total(&ctx).unwrap();
    assert!((total - 1.36).abs() < 0.01, "{total}");
}
