use super::*;
use crate::testing::TestEnv;

fn record() -> Record {
    Record {
        job_id: "20260101T000000Z-123".into(),
        provider: "vast".into(),
        instance: "123".into(),
        owner: Some("alice".into()),
        gpu: Some("RTX 4090".into()),
        command: vec!["python".into(), "train.py".into(), "a b".into()],
        rate: 0.36,
        created_at: 1_000,
        expires_at: 8_200,
        linger: 0,
        outputs: "outputs".into(),
        local: PathBuf::from("/tmp/project/gpu-runs/x"),
        root: "/root/.local/state/gpu-run/x".into(),
        jobdir: "/workspace/gpu-run-x".into(),
        state: BOOTING.into(),
        exit_code: None,
        timed_out: false,
        ended_at: None,
        collected: false,
        destroyed: false,
    }
}

fn live(phase: &str, exit: Option<i32>, timed: bool) -> Seen {
    Seen::Live(
        Box::new(VastBackend::resumed("fake")),
        Status {
            phase: phase.into(),
            exit,
            timed,
            size: 0,
        },
    )
}

#[test]
fn a_record_round_trips_through_its_file() {
    let env = TestEnv::new();
    let ctx = env.ctx().build();
    let mut r = record();
    r.exit_code = Some(3);
    r.ended_at = Some(2_000);
    r.timed_out = true;
    save(&ctx, &mut r.clone()).unwrap();
    let path = record_path(&ctx.env, &r.job_id);
    assert_eq!(read(&path).unwrap(), r);
    assert_eq!(load(&ctx, &r.job_id).unwrap(), r);
    // A fresh process finds it by its instance ID too.
    assert_eq!(load(&ctx, "123").unwrap(), r);
    assert_eq!(records(&ctx).unwrap().0, vec![r]);
}

#[test]
fn ids_that_could_leave_the_state_directory_never_resolve() {
    let env = TestEnv::new();
    let ctx = env.ctx().build();
    for id in ["", "..", "../x", "a/b", "a.json", &"x".repeat(65)] {
        assert_eq!(load(&ctx, id).unwrap_err().exit_code(), 16, "{id:?}");
    }
}

#[test]
fn damaged_records_are_reported_not_trusted() {
    let env = TestEnv::new();
    let ctx = env.ctx().build();
    let dir = jobs_dir(&ctx.env);
    fs::create_dir_all(&dir).unwrap();
    for (name, text) in [
        ("empty.json", ""),
        ("array.json", "[]"),
        ("partial.json", r#"{"job_id":"x"}"#),
    ] {
        fs::write(dir.join(name), text).unwrap();
        assert!(read(&dir.join(name)).is_err(), "{name}");
    }
    let (found, damaged) = records(&ctx).unwrap();
    assert!(found.is_empty());
    assert_eq!(damaged.len(), 3);
}

#[test]
fn remote_status_maps_to_job_states() {
    let mut r = record();
    let now = 2_000;
    assert!(!apply(&mut r, &live("setup", None, false), now));
    assert_eq!(r.state, BOOTING);
    assert!(apply(&mut r, &live("job", None, false), now));
    assert_eq!(r.state, RUNNING);
    assert!(apply(&mut r, &live("job", Some(0), false), now));
    assert_eq!((r.state.as_str(), r.exit_code), (SUCCEEDED, Some(0)));
    let mut r = record();
    apply(&mut r, &live("job", Some(2), false), now);
    assert_eq!((r.state.as_str(), r.exit_code), (FAILED, Some(2)));
    let mut r = record();
    apply(&mut r, &live("job", Some(17), true), now);
    assert_eq!((r.state.as_str(), r.timed_out), (FAILED, true));
}

#[test]
fn a_lost_supervisor_is_lost_unless_the_job_already_finished() {
    let mut r = record();
    apply(&mut r, &live("lost", None, false), 2_000);
    assert_eq!(r.state, LOST);
    let mut r = record();
    apply(&mut r, &live("job", Some(0), false), 2_000);
    apply(&mut r, &live("lost", None, false), 2_100);
    assert_eq!(r.state, SUCCEEDED);
}

#[test]
fn a_gone_instance_is_expired_at_its_deadline_and_lost_before_it() {
    let mut r = record();
    apply(&mut r, &live("job", Some(0), false), 2_000);
    assert!(apply(&mut r, &Seen::Gone, 8_200));
    assert_eq!(r.state, EXPIRED);
    assert_eq!(r.exit_code, Some(0), "the exit code outlives the instance");
    assert!(r.destroyed);
    assert_eq!(r.ended_at, Some(8_200));
    assert_eq!(r.elapsed(99_999), 7_200, "billing stopped at expiry");

    let mut r = record();
    apply(&mut r, &Seen::Gone, 3_000);
    assert_eq!(r.state, LOST);
    assert_eq!(r.ended_at, Some(3_000));

    // Collected outputs are not turned into a loss when the instance goes.
    let mut r = record();
    apply(&mut r, &live("job", Some(0), false), 2_000);
    r.collected = true;
    apply(&mut r, &Seen::Gone, 8_200);
    assert_eq!(r.state, SUCCEEDED);
}

#[test]
fn a_cancelled_job_stays_cancelled_whatever_the_instance_says() {
    let mut r = record();
    r.state = CANCELLED.into();
    apply(&mut r, &live("job", Some(143), false), 2_000);
    assert_eq!(r.state, CANCELLED);
    apply(&mut r, &Seen::Gone, 3_000);
    assert_eq!(r.state, CANCELLED);
}

#[test]
fn an_unreachable_instance_keeps_the_last_known_state() {
    let mut r = record();
    apply(&mut r, &live("job", None, false), 2_000);
    let seen = Seen::Unreachable(Error::msg("ssh timed out"));
    assert!(!apply(&mut r, &seen, 3_000));
    assert_eq!(r.state, RUNNING);
    assert!(!r.destroyed);
}

#[test]
fn elapsed_and_cost_run_until_the_instance_is_destroyed() {
    let mut r = record();
    assert_eq!(r.elapsed(4_600), 3_600);
    assert!((r.cost(4_600) - 0.36).abs() < 1e-9);
    r.ended_at = Some(2_800);
    assert_eq!(r.elapsed(99_999), 1_800);
    assert!((r.cost(99_999) - 0.18).abs() < 1e-9);
    assert_eq!(duration_text(3_723), "1h02m03s");
    assert_eq!(duration_text(75), "1m15s");
}

#[test]
fn a_stale_copy_never_undoes_what_another_command_saved() {
    let env = TestEnv::new();
    let ctx = env.ctx().build();
    let mut stale = record();
    stale.state = RUNNING.into();
    save(&ctx, &mut stale.clone()).unwrap();
    // `gpu job outputs` collects and destroys while `status` holds `stale`.
    let mut collector = stale.clone();
    apply(&mut collector, &live("job", Some(0), false), 2_000);
    collector.collected = true;
    collector.destroyed = true;
    collector.ended_at = Some(2_100);
    save(&ctx, &mut collector.clone()).unwrap();
    // `status` then sees the instance gone and saves its older copy.
    assert!(apply(&mut stale, &Seen::Gone, 2_200));
    assert_eq!(stale.state, LOST);
    save(&ctx, &mut stale).unwrap();
    assert_eq!(stale, collector, "the caller sees the merged record");
    assert_eq!(
        read(&record_path(&ctx.env, &stale.job_id)).unwrap(),
        collector
    );

    let mut cancelled = record();
    cancelled.job_id = "20260101T000000Z-456".into();
    cancelled.state = CANCELLED.into();
    save(&ctx, &mut cancelled.clone()).unwrap();
    let mut older = record();
    older.job_id = cancelled.job_id.clone();
    apply(&mut older, &live("job", None, false), 2_000);
    save(&ctx, &mut older).unwrap();
    assert_eq!(older.state, CANCELLED);
}

#[test]
fn a_stopped_instance_is_lost_but_not_destroyed() {
    for provider in ["stopped", "exited"] {
        let mut r = record();
        assert!(apply(&mut r, &Seen::Down(provider.into()), 2_000));
        assert_eq!(r.state, LOST);
        assert!(!r.destroyed && !r.done());
        assert_eq!(r.ended_at, None, "it still bills");
    }
    let mut r = record();
    assert!(!apply(&mut r, &Seen::Down("loading".into()), 2_000));
    assert_eq!(r.state, BOOTING);
}
