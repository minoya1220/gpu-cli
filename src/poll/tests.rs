use crate::testing::{FakeClock, TestEnv};
#[test]
fn sleeps_are_bounded_and_cancellation_is_explicit_for_cleanup() {
    let env = TestEnv::new();
    let clock = FakeClock::now_real();
    let ctx = env.ctx().clock(&clock).build();
    let budget = ctx.polling(5.5, 2.0);
    budget.pause();
    budget.pause();
    budget.pause();
    assert_eq!(clock.elapsed(), 5.5);
    assert!(budget.expired());
    budget.pause();
    assert_eq!(clock.elapsed(), 5.5);
    ctx.begin_run();
    ctx.interrupt();
    assert_eq!(budget.check_cancelled().unwrap_err().exit_code(), 130);
    // Destruction can retain the same budget without checking job cancellation.
    let cleanup = ctx.polling(3.0, 2.0);
    cleanup.pause();
    assert_eq!(clock.elapsed(), 7.5);
    ctx.end_run();
}
