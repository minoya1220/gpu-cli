//! One monotonic budget and sleep policy for bounded provider polling.
#[cfg(test)]
mod tests;
use crate::{ctx::Ctx, error::Result};
pub(crate) struct Poll<'a> {
    ctx: &'a Ctx,
    deadline: f64,
    interval: f64,
}
impl<'a> Poll<'a> {
    pub fn new(ctx: &'a Ctx, timeout: f64, interval: f64) -> Self {
        Self {
            ctx,
            deadline: ctx.clock.monotonic() + timeout,
            interval,
        }
    }
    pub fn remaining(&self) -> f64 {
        self.deadline - self.ctx.clock.monotonic()
    }
    pub fn expired(&self) -> bool {
        self.remaining() <= 0.0
    }
    /// Call only for interruptible work; cleanup budgets deliberately do not.
    pub fn check_cancelled(&self) -> Result<()> {
        self.ctx.check_cancelled()
    }
    pub fn pause(&self) {
        self.pause_for(self.interval);
    }
    pub fn pause_for(&self, seconds: f64) {
        self.ctx.clock.sleep(self.remaining().clamp(0.0, seconds));
    }
}
