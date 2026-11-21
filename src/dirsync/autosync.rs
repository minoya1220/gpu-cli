//! Background autosync: the `--watch` pull loop run by a detached
//! `gpu autosync-run` process, one per link. Its log, status record and
//! lock live beside the link in the sync state directory.
//!
//! The lock (`store::autosync_claim`) is what says a syncer runs: it dies
//! with the process, so a crash never leaves a link looking synced. Nothing
//! secret is logged: errors pass through `redact_text`, and the log holds
//! only file names, counts and times.
use std::path::PathBuf;

use jiff::Timestamp;

use super::engine::Outcome;
use super::store::{self, Link, Status};
use super::watch::{Loop, Stop};
use crate::cli::AutosyncRunFlags;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::process::Cmd;
use crate::pyjson::{Dict, Value};

/// Seconds between looks at the instance's auto-destroy deadline.
const DEADLINE_CHECK: f64 = 300.0;
/// How long before an auto-destroy deadline autosync pulls once more.
const DEADLINE_LEAD: i64 = 180;
/// A log larger than this is set aside when autosync starts.
const LOG_LIMIT: u64 = 1 << 20;
/// Seconds `stop` waits for the syncer to finish its current pull.
const STOP_WAIT: f64 = 40.0;
/// Seconds `start` waits to see the syncer take its lock.
const START_WAIT: f64 = 5.0;

/// What the loop reports to, for a background run.
pub(super) struct Background<'a> {
    cfg: &'a Dict,
    status: Status,
    deadline: Option<Timestamp>,
    checked: Option<f64>,
    pulled_for: Option<i64>,
    /// The failure last written to the log, so a long outage is one line.
    logged: String,
}

fn stamp(ctx: &Ctx) -> String {
    ctx.clock.now().strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

impl<'a> Background<'a> {
    pub fn new(ctx: &Ctx, cfg: &'a Dict) -> Background<'a> {
        Background {
            cfg,
            status: Status {
                started_at: stamp(ctx),
                state: "connecting".into(),
                ..Status::default()
            },
            deadline: None,
            checked: None,
            pulled_for: None,
            logged: String::new(),
        }
    }

    pub fn cfg(&self) -> &'a Dict {
        self.cfg
    }

    /// One line of the log. A log that cannot be written never stops syncing.
    pub fn say(&self, ctx: &Ctx, text: &str) {
        let _ = ctx.println(&format!("{} {text}", stamp(ctx)));
    }

    fn save(&self, ctx: &Ctx, link: &Link) {
        // A removed link's record is gone for good; do not bring it back.
        if store::links(&ctx.env).contains(link) {
            let _ = store::save_status(&ctx.env, link, &self.status);
        }
    }

    pub fn connected(&mut self, ctx: &Ctx, link: &Link) {
        self.status.state = "syncing".into();
        self.save(ctx, link);
    }

    pub fn recovered(&mut self, ctx: &Ctx, link: &Link, failures: u32) {
        self.say(
            ctx,
            &format!(
                "Connection restored after {} failed.",
                crate::table::plural(failures as usize, "attempt")
            ),
        );
        self.logged.clear();
        self.status.state = "syncing".into();
        self.save(ctx, link);
    }

    pub fn failed(
        &mut self,
        ctx: &Ctx,
        link: &Link,
        text: &str,
        failures: u32,
        delay: f64,
    ) -> Result<()> {
        let now = stamp(ctx);
        match self.status.errors.last_mut() {
            Some((at, last)) if last == text => *at = now,
            _ => {
                self.status.errors.push((now, text.to_string()));
                let extra = self
                    .status
                    .errors
                    .len()
                    .saturating_sub(store::STATUS_ERRORS);
                self.status.errors.drain(..extra);
            }
        }
        self.status.state = "reconnecting".into();
        self.save(ctx, link);
        if failures == 1 || self.logged != text {
            self.say(
                ctx,
                &format!("Sync paused: {text}. Retrying in {delay:.0}s ..."),
            );
            self.logged = text.to_string();
        }
        Ok(())
    }

    pub fn stopped(&mut self, ctx: &Ctx, link: &Link, reason: &str) {
        self.say(ctx, &format!("Autosync stopped: {reason}."));
        self.status.state = format!("stopped: {reason}");
        self.save(ctx, link);
    }

    /// A successful pull; `forced` marks the one before the auto-destroy deadline.
    pub fn pulled(
        &mut self,
        ctx: &Ctx,
        link: &Link,
        outcome: &Outcome,
        forced: bool,
    ) -> Result<()> {
        self.status.last_pull_at = stamp(ctx);
        self.status.last_pull_files = outcome.pulled.len() as u64;
        if forced {
            self.pulled_for = self.deadline.map(|d| d.as_second());
            let when = self
                .deadline
                .map(|d| crate::expiry::local_time(ctx, d))
                .unwrap_or_default();
            self.say(
                ctx,
                &format!("Pulled before the auto-destroy deadline ({when})."),
            );
            let left = outcome.large.len() + outcome.missing.len();
            if let Some(problem) =
                super::unpulled_problem(left, &outcome.ignored, &outcome.unpullable)
            {
                self.say(
                    ctx,
                    &format!(
                        "Warning: the latest changes on {} are not all in {}: {problem}. They are lost when the instance is destroyed.",
                        link.id,
                        link.local.display()
                    ),
                );
            }
        }
        for line in super::outcome_lines(link, outcome) {
            self.say(ctx, &line);
        }
        self.save(ctx, link);
        Ok(())
    }

    /// Whether the once-more pull before the auto-destroy deadline is due.
    /// Looks the deadline up now and then, as it can be armed or changed
    /// after autosync started.
    pub fn deadline_due(&mut self, ctx: &Ctx, link: &Link, alias: Option<&str>) -> bool {
        let now = ctx.clock.monotonic();
        if let Some(alias) = alias {
            if self.checked.is_none_or(|at| now - at >= DEADLINE_CHECK) {
                self.checked = Some(now);
                // A failed lookup keeps what was known.
                if let Ok(Ok(found)) = crate::provider::parse(&link.id)
                    .map(|instance| instance.expiry_deadline(ctx, self.cfg, alias))
                {
                    if found != self.deadline {
                        self.deadline = found;
                        self.status.deadline = found
                            .map(|d| crate::expiry::local_time(ctx, d))
                            .unwrap_or_default();
                        self.save(ctx, link);
                    }
                }
            }
        }
        let Some(deadline) = self.deadline else {
            return false;
        };
        self.pulled_for != Some(deadline.as_second())
            && ctx.clock.now().as_second() >= deadline.as_second() - DEADLINE_LEAD
    }
}

/// What `gpu sync` shows about a link's background syncer.
pub(super) struct Info {
    pub pid: Option<u32>,
    pub status: Option<Status>,
    pub log: PathBuf,
}

pub(super) fn info(ctx: &Ctx, link: &Link) -> Info {
    Info {
        pid: store::autosync_pid(&ctx.env, link),
        status: store::load_status(&ctx.env, link),
        log: store::log_path(&ctx.env, link),
    }
}

impl Info {
    /// The machine-readable form for `gpu sync --json`.
    pub fn value(&self) -> Value {
        let mut item = Dict::new();
        item.insert("running", Value::Bool(self.running()));
        item.insert(
            "pid",
            self.pid
                .map_or(Value::Null, |pid| Value::Int(i64::from(pid))),
        );
        let text =
            |f: fn(&Status) -> &str| Value::str(self.status.as_ref().map(f).unwrap_or_default());
        item.insert("state", text(|s| &s.state));
        item.insert("last_pull_at", text(|s| &s.last_pull_at));
        item.insert("deadline", text(|s| &s.deadline));
        item.insert(
            "errors",
            Value::List(
                self.errors()
                    .iter()
                    .map(|(at, error)| {
                        let mut entry = Dict::new();
                        entry.insert("at", Value::str(at));
                        entry.insert("text", Value::str(error));
                        Value::Dict(entry)
                    })
                    .collect(),
            ),
        );
        item.insert("log", Value::str(self.log.to_string_lossy()));
        Value::Dict(item)
    }

    pub fn running(&self) -> bool {
        self.pid.is_some()
    }

    /// The table cell.
    pub fn label(&self) -> String {
        match (&self.pid, &self.status) {
            (Some(_), Some(status)) if status.state == "reconnecting" => {
                "running, reconnecting".into()
            }
            (Some(_), _) => "running".into(),
            (None, _) => "off".into(),
        }
    }

    /// The latest errors it reported.
    pub fn errors(&self) -> &[(String, String)] {
        match &self.status {
            Some(status) => &status.errors,
            None => &[],
        }
    }
}

/// Start the link's background syncer unless one runs. It makes its first
/// pull as soon as it connects.
pub(super) fn start(ctx: &Ctx, link: &Link, max_file: &str) -> Result<()> {
    let log = store::log_path(&ctx.env, link);
    if let Some(pid) = store::autosync_pid(&ctx.env, link) {
        return ctx.println(&format!(
            "Autosync already runs for {} (process {pid}). Log: {}",
            link.local.display(),
            log.display()
        ));
    }
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > LOG_LIMIT) {
        let _ = std::fs::rename(&log, log.with_extension("log.old"));
    }
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).map_err(|e| crate::error::os_error(&e, parent))?;
    }
    let exe = std::env::current_exe()
        .map_err(|e| Error::msg(format!("Cannot find the gpu program to run autosync: {e}")))?;
    let argv = [
        exe.to_string_lossy().into_owned(),
        "autosync-run".into(),
        link.id.clone(),
        "--dir".into(),
        link.local.to_string_lossy().into_owned(),
        "--max-file".into(),
        max_file.into(),
    ];
    ctx.spawn_detached(&Cmd::new(&argv), &log)
        .map_err(|error| {
            Error::msg(format!(
                "Could not start autosync: {}",
                error.describe("gpu")
            ))
        })?;
    // Wait to see it take its lock, so a failure to start is not silent.
    let end = ctx.clock.monotonic() + START_WAIT;
    let mut pid = None;
    while pid.is_none() && ctx.clock.monotonic() < end {
        pid = store::autosync_pid(&ctx.env, link);
        if pid.is_none() {
            ctx.clock.sleep(0.1);
        }
    }
    match pid {
        Some(pid) => ctx.println(&format!(
            "Autosync started for {} (process {pid}); it pulls GPU changes into {} until the instance or link is gone. \
             Log: {}. Stop it: gpu sync {} --no-autosync",
            link.id,
            link.local.display(),
            log.display(),
            link.id
        )),
        None => ctx.eprintln(&format!(
            "Autosync was started for {} but is not confirmed running yet. Check: gpu sync --list; log: {}",
            link.id,
            log.display()
        )),
    }
}

/// Stop the link's background syncer; whether one was running. It finishes
/// the pull it is in the middle of first.
pub(super) fn stop(ctx: &Ctx, link: &Link) -> Result<bool> {
    let Some(pid) = store::autosync_pid(&ctx.env, link) else {
        return Ok(false);
    };
    if pid == 0 {
        return Err(Error::msg(format!(
            "Autosync runs for {} but its process is unknown; stop it by hand (see {}).",
            link.id,
            store::log_path(&ctx.env, link).display()
        )));
    }
    let signal = |name: &str| {
        let _ = ctx.run(&Cmd::new(&["kill", name, &pid.to_string()]).timeout(Some(10.0)));
    };
    let gone = |wait: f64| {
        let end = ctx.clock.monotonic() + wait;
        loop {
            if store::autosync_pid(&ctx.env, link).is_none() {
                return true;
            }
            if ctx.clock.monotonic() >= end {
                return false;
            }
            ctx.clock.sleep(0.2);
        }
    };
    signal("-TERM");
    if !gone(STOP_WAIT) {
        signal("-KILL");
        if !gone(5.0) {
            return Err(Error::msg(format!(
                "Autosync (process {pid}) did not stop for {}.",
                link.id
            )));
        }
    }
    if let Some(mut status) = store::load_status(&ctx.env, link) {
        status.state = "stopped: stopped by you".into();
        status.errors.clear();
        if store::links(&ctx.env).contains(link) {
            let _ = store::save_status(&ctx.env, link, &status);
        }
    }
    Ok(true)
}

/// `gpu autosync-run`: the detached syncer itself.
pub(crate) fn run_command(ctx: &Ctx, cfg: &Dict, flags: &AutosyncRunFlags) -> Result<()> {
    let dir = super::resolve_dir(Some(&flags.dir))?;
    let options = super::pull_options(&flags.max_file, false)?;
    let Some(link) = store::links(&ctx.env)
        .into_iter()
        .find(|l| l.id == flags.id && l.local == dir)
    else {
        return Err(Error::usage(format!(
            "{} is not linked to {}",
            flags.id,
            dir.display()
        )));
    };
    // A check of whether one runs holds the lock for an instant: try again.
    let mut claim = None;
    for attempt in 0..3 {
        claim = store::autosync_claim(&ctx.env, &link)?;
        if claim.is_some() {
            break;
        }
        if attempt < 2 {
            ctx.clock.sleep(0.5);
        }
    }
    let Some(_claim) = claim else {
        return ctx.println("Autosync already runs for this link.");
    };
    let background = Background::new(ctx, cfg);
    background.say(
        ctx,
        &format!(
            "Autosync started: {}:{} -> {}",
            link.id,
            link.remote,
            link.local.display()
        ),
    );
    background.save(ctx, &link);
    let mut syncing = Loop {
        ctx,
        link: &link,
        options: &options,
        alias: None,
        retries: None,
        background: Some(background),
        waits: None,
        passes: None,
    };
    let stop = syncing.run(None);
    let mut background = syncing.background.take().expect("a background run");
    match stop {
        Ok(Stop::LinkRemoved) => background.stopped(ctx, &link, "the link was removed"),
        Ok(Stop::Bound) => background.stopped(ctx, &link, "asked to stop"),
        // `Loop` reported it when it found the instance gone.
        Ok(Stop::InstanceGone) => {}
        Err(error) => {
            background.stopped(
                ctx,
                &link,
                &format!("failed: {}", crate::redact::redact_text(error.summary())),
            );
            return Err(error);
        }
    }
    Ok(())
}
