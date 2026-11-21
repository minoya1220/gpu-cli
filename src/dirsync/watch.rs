//! The pull loop behind `gpu sync --watch` and background autosync: one
//! implementation, two policies. A terminal's `--watch` gives up after a few
//! failures in a row; autosync never does, backs off, looks the instance up
//! again after each failure (a laptop that slept may come back to a new SSH
//! address) and stops by itself when the instance or the link is gone.
use std::collections::BTreeSet;

use super::autosync::Background;
use super::engine::PullOptions;
use super::store::{self, Link};
use super::transport::{Listing, Remote};
use super::{pull_once, say_outcome, BACKSTOP_EVERY, WATCH_RETRIES, WATCH_WAIT};
use crate::ctx::Ctx;
use crate::error::{Error, Result};

/// Seconds `--watch` waits between attempts after a failure.
const WATCH_PAUSE: f64 = 10.0;
/// Autosync's first pause; it doubles up to `MAX_PAUSE`.
const FIRST_PAUSE: f64 = 10.0;
const MAX_PAUSE: f64 = 300.0;

/// Why a loop that did not fail ended.
#[derive(Debug, PartialEq)]
pub(super) enum Stop {
    /// A test's bound was reached.
    Bound,
    LinkRemoved,
    InstanceGone,
}

#[derive(Default)]
struct State {
    sig: String,
    pending: Option<Listing>,
    alias: Option<String>,
    failures: u32,
    idle: u32,
    waited: u32,
    /// Oversized files already warned about in this run.
    warned: BTreeSet<String>,
}

pub(super) struct Loop<'a> {
    pub ctx: &'a Ctx,
    pub link: &'a Link,
    pub options: &'a PullOptions,
    /// The SSH alias to start with; `None` looks the instance up first.
    pub alias: Option<String>,
    /// Failures in a row after which the loop gives up; `None`: never.
    pub retries: Option<u32>,
    /// Present for autosync: logging, status, the pre-expiry pull and the
    /// checks that end the loop by itself.
    pub background: Option<Background<'a>>,
    /// Stop after this many idle waits (tests).
    pub waits: Option<u32>,
    /// Stop after this many passes (tests).
    pub passes: Option<u32>,
}

pub(super) fn pause_after(failures: u32, background: bool) -> f64 {
    if !background {
        return WATCH_PAUSE;
    }
    (FIRST_PAUSE * 2f64.powi(failures.saturating_sub(1).min(8) as i32)).min(MAX_PAUSE)
}

impl Loop<'_> {
    pub fn run(&mut self, first: Option<Listing>) -> Result<Stop> {
        let mut st = State {
            pending: first,
            alias: self.alias.take(),
            ..State::default()
        };
        let mut passes = 0u32;
        loop {
            passes += 1;
            if self.background.is_some() && !store::links(&self.ctx.env).contains(self.link) {
                return Ok(Stop::LinkRemoved);
            }
            match self.step(&mut st) {
                Ok(()) => {
                    if st.failures > 0 {
                        if let Some(background) = self.background.as_mut() {
                            background.recovered(self.ctx, self.link, st.failures);
                        }
                    }
                    st.failures = 0;
                }
                Err(error) => {
                    st.failures += 1;
                    if self.retries.is_some_and(|max| st.failures >= max) {
                        return Err(error);
                    }
                    let delay = pause_after(st.failures, self.background.is_some());
                    if self.background.is_some() {
                        // Find the instance's address again next time.
                        st.alias = None;
                        if st.failures >= 2 && self.gone() {
                            return Ok(Stop::InstanceGone);
                        }
                    }
                    self.failed(&error, st.failures, delay)?;
                    self.ctx.clock.sleep(delay);
                }
            }
            if self.waits.is_some_and(|limit| st.waited >= limit)
                || self.passes.is_some_and(|limit| passes >= limit)
            {
                return Ok(Stop::Bound);
            }
        }
    }

    /// One look at the instance and, when it changed, one pull.
    fn step(&mut self, st: &mut State) -> Result<()> {
        let ctx = self.ctx;
        let link = self.link;
        if st.alias.is_none() {
            st.alias = Some(crate::provider::parse(&link.id)?.wait_ssh(ctx, 60.0)?);
            st.sig.clear();
            st.pending = None;
            if let Some(background) = self.background.as_mut() {
                background.connected(ctx, link);
            }
        }
        let remote = Remote::new(ctx, st.alias.clone().expect("connected above"));
        // Once more shortly before an auto-destroy deadline, whatever the
        // instance last reported.
        let forced = match self.background.as_mut() {
            Some(background) => background.deadline_due(ctx, link, st.alias.as_deref()),
            None => false,
        };
        if forced {
            st.sig.clear();
            st.pending = None;
        }
        let listing = match st.pending.take() {
            Some(listing) => Some(listing),
            None if st.sig.is_empty() => Some(remote.manifest(&link.remote)?),
            None => {
                let found = remote.wait(&link.remote, WATCH_WAIT, &st.sig)?;
                st.waited += 1;
                if found.is_none() {
                    st.idle += 1;
                }
                match found {
                    Some(listing) => Some(listing),
                    None if st.idle >= BACKSTOP_EVERY => {
                        st.idle = 0;
                        Some(remote.manifest(&link.remote)?)
                    }
                    None => None,
                }
            }
        };
        if let Some(listing) = listing {
            st.sig = listing.sig.clone();
            let mut outcome = pull_once(ctx, &remote, link, self.options, Some(listing))?;
            outcome
                .large
                .retain(|(path, _)| st.warned.insert(path.clone()));
            match self.background.as_mut() {
                Some(background) => background.pulled(ctx, link, &outcome, forced)?,
                None => say_outcome(ctx, link, &outcome)?,
            }
        }
        Ok(())
    }

    fn failed(&mut self, error: &Error, failures: u32, delay: f64) -> Result<()> {
        let text = crate::redact::redact_text(error.summary());
        match self.background.as_mut() {
            Some(background) => background.failed(self.ctx, self.link, &text, failures, delay),
            None => self
                .ctx
                .eprintln(&format!("Sync paused: {text}. Retrying in {delay:.0}s ...")),
        }
    }

    /// Whether the provider says the instance no longer exists (the link is
    /// then forgotten, as `gpu sync` does).
    fn gone(&mut self) -> bool {
        let Some(background) = self.background.as_ref() else {
            return false;
        };
        super::prune(self.ctx, background.cfg());
        let gone = !store::links(&self.ctx.env).contains(self.link);
        if gone {
            self.background.as_mut().expect("checked above").stopped(
                self.ctx,
                self.link,
                "the instance no longer exists",
            );
        }
        gone
    }
}

/// `gpu sync --watch`: this terminal's loop.
pub(super) fn watch(
    ctx: &Ctx,
    remote: &Remote,
    link: &Link,
    options: &PullOptions,
    first: Option<Listing>,
    cycles: Option<u32>,
) -> Result<()> {
    ctx.println(&format!(
        "Watching {}:{} -> {}. Ctrl-C to stop.",
        link.id,
        link.remote,
        link.local.display()
    ))?;
    Loop {
        ctx,
        link,
        options,
        alias: Some(remote.alias.clone()),
        retries: Some(WATCH_RETRIES),
        background: None,
        waits: cycles,
        passes: None,
    }
    .run(first)
    .map(drop)
}
