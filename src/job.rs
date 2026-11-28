//! Asynchronous jobs: `gpu job start|status|logs|outputs|cancel|ls`.
//!
//! A job is a `gpu run` split into separate calls. `start` runs the same
//! launch, upload and job-start steps as `gpu run` (in `run.rs`) and returns
//! while the job runs; the other commands find it again by ID from the job
//! record `start` saved under the state directory, so any later process can
//! resume it. The instance keeps its own expiry, armed to the `--expire`
//! deadline, so an uncollected job is destroyed on time; the supervisor keeps
//! the exit code, log and outputs on the instance until then (`DETACHED=1` in
//! `remote/run_remote.sh`). `outputs` then runs the same recovery and download
//! as `gpu run` and destroys the instance unless the job set `--linger`.
use std::fs;
use std::path::{Path, PathBuf};

use jiff::Timestamp;

use crate::ctx::{Ctx, Env};
use crate::error::{os_error, Error, ExitCode, Result};
use crate::provider::Provider;
use crate::pyfmt::fixed;
use crate::pyjson::{self, Dict, Value};
use crate::run::{
    checked, control, log_chunk, parse_status, recover, Owned, Phase, RunBackend, Session, Status,
    CHUNK,
};
use crate::vast;
use crate::vast::run::VastBackend;

#[cfg(test)]
mod tests;

/// How long `gpu job outputs` waits for the instance to answer SSH.
const CONNECT_TIMEOUT: f64 = 45.0;
/// `status`, `logs` and `ls` answer quickly: a running instance that does not
/// answer SSH this fast is reported with its last known state.
const REFRESH_CONNECT_TIMEOUT: f64 = 5.0;
/// `gpu job cancel` must not hang on an instance that cannot be reached.
const CANCEL_CONNECT_TIMEOUT: f64 = 20.0;

pub(crate) fn vast_only() -> Error {
    Error::usage(
        "gpu job supports Vast only. Modal sandboxes, their helper process and output volume belong to one \
         attached process, so use gpu run for Modal.",
    )
}

pub(crate) const BOOTING: &str = "booting";
pub(crate) const RUNNING: &str = "running";
pub(crate) const SUCCEEDED: &str = "succeeded";
pub(crate) const FAILED: &str = "failed";
pub(crate) const CANCELLED: &str = "cancelled";
pub(crate) const EXPIRED: &str = "expired";
pub(crate) const LOST: &str = "lost";

/// What `gpu job start` saved: everything a later process needs to find, read,
/// collect and destroy the job without the flags it started with.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Record {
    pub(crate) job_id: String,
    pub(crate) provider: String,
    pub(crate) instance: String,
    pub(crate) owner: Option<String>,
    pub(crate) gpu: Option<String>,
    pub(crate) command: Vec<String>,
    pub(crate) rate: f64,
    pub(crate) created_at: i64,
    pub(crate) expires_at: i64,
    pub(crate) linger: i64,
    pub(crate) outputs: String,
    pub(crate) local: PathBuf,
    pub(crate) root: String,
    pub(crate) jobdir: String,
    /// Last observed state; see the `BOOTING`..`LOST` constants.
    pub(crate) state: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    /// When the instance stopped billing, once known.
    pub(crate) ended_at: Option<i64>,
    pub(crate) collected: bool,
    pub(crate) destroyed: bool,
}

/// The facts `gpu run` already has when its job has started.
pub(crate) struct Start<'a> {
    pub(crate) run_id: &'a str,
    pub(crate) instance: &'a str,
    pub(crate) owner: Option<String>,
    pub(crate) gpu: Option<String>,
    pub(crate) command: &'a [String],
    pub(crate) rate: f64,
    pub(crate) created_at: i64,
    pub(crate) deadline: i64,
    pub(crate) linger: i64,
    pub(crate) outputs: &'a str,
    pub(crate) local: &'a Path,
    pub(crate) paths: &'a (String, String),
}

pub(crate) fn jobs_dir(env: &Env) -> PathBuf {
    env.xdg("XDG_STATE_HOME", ".local/state")
        .join("gpu")
        .join("jobs")
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn record_path(env: &Env, id: &str) -> PathBuf {
    jobs_dir(env).join(format!("{id}.json"))
}

impl Record {
    fn to_value(&self) -> Value {
        let text = |value: &Option<String>| value.as_deref().map_or(Value::Null, Value::str);
        let mut d = Dict::new();
        d.insert("version", Value::Int(1));
        d.insert("job_id", Value::str(&self.job_id));
        d.insert("provider", Value::str(&self.provider));
        d.insert("instance", Value::str(&self.instance));
        d.insert("owner", text(&self.owner));
        d.insert("gpu", text(&self.gpu));
        d.insert(
            "command",
            Value::List(self.command.iter().map(Value::str).collect()),
        );
        d.insert("price_per_hour", Value::Float(self.rate));
        d.insert("created_at", Value::Int(self.created_at));
        d.insert("expires_at", Value::Int(self.expires_at));
        d.insert("linger_seconds", Value::Int(self.linger));
        d.insert("outputs", Value::str(&self.outputs));
        d.insert("local_dir", Value::str(self.local.to_string_lossy()));
        d.insert("remote_root", Value::str(&self.root));
        d.insert("remote_jobdir", Value::str(&self.jobdir));
        d.insert("state", Value::str(&self.state));
        d.insert(
            "exit_code",
            self.exit_code.map_or(Value::Null, |c| Value::Int(c.into())),
        );
        d.insert("timed_out", Value::Bool(self.timed_out));
        d.insert("ended_at", self.ended_at.map_or(Value::Null, Value::Int));
        d.insert("outputs_collected", Value::Bool(self.collected));
        d.insert("destroyed", Value::Bool(self.destroyed));
        Value::Dict(d)
    }

    fn from_value(value: &Value) -> Option<Record> {
        let d = value.as_dict()?;
        let text = |key: &str| d.get(key).and_then(Value::as_str).map(String::from);
        let int = |key: &str| match d.get(key) {
            Some(Value::Int(v)) => Some(*v),
            _ => None,
        };
        let flag = |key: &str| matches!(d.get(key), Some(Value::Bool(true)));
        let rate = match d.get("price_per_hour")? {
            Value::Float(v) => *v,
            Value::Int(v) => *v as f64,
            _ => return None,
        };
        let Some(Value::List(command)) = d.get("command") else {
            return None;
        };
        let record = Record {
            job_id: text("job_id")?,
            provider: text("provider")?,
            instance: text("instance")?,
            owner: text("owner"),
            gpu: text("gpu"),
            command: command
                .iter()
                .map(|c| c.as_str().map(String::from))
                .collect::<Option<_>>()?,
            rate,
            created_at: int("created_at")?,
            expires_at: int("expires_at")?,
            linger: int("linger_seconds")?,
            outputs: text("outputs")?,
            local: PathBuf::from(text("local_dir")?),
            root: text("remote_root")?,
            jobdir: text("remote_jobdir")?,
            state: text("state")?,
            exit_code: int("exit_code").and_then(|c| i32::try_from(c).ok()),
            timed_out: flag("timed_out"),
            ended_at: int("ended_at"),
            collected: flag("outputs_collected"),
            destroyed: flag("destroyed"),
        };
        (valid_id(&record.job_id) && rate.is_finite() && rate >= 0.0).then_some(record)
    }

    /// Keep what another command already recorded: a concurrent command
    /// working from an older copy never clears collected, destroyed or
    /// cancelled, nor forgets the exit code.
    fn merge(&mut self, stored: &Record) {
        if stored.collected && !self.collected {
            self.collected = true;
            self.state = stored.state.clone();
        }
        if stored.state == CANCELLED {
            self.state = CANCELLED.into();
        }
        self.destroyed |= stored.destroyed;
        if self.exit_code.is_none() && stored.exit_code.is_some() {
            self.exit_code = stored.exit_code;
            self.timed_out = stored.timed_out;
        }
        self.ended_at = match (self.ended_at, stored.ended_at) {
            (Some(mine), Some(theirs)) => Some(mine.min(theirs)),
            (mine, theirs) => mine.or(theirs),
        };
    }

    fn finished(&self) -> bool {
        self.exit_code.is_some()
    }

    /// No instance is left to bill or to ask.
    fn done(&self) -> bool {
        self.destroyed
    }

    fn deadline(&self) -> Result<Timestamp> {
        Timestamp::from_second(self.expires_at).map_err(|_| Error::msg("Invalid job expiry."))
    }

    /// Seconds the instance has billed: until it was destroyed, else until now.
    fn elapsed(&self, now: i64) -> i64 {
        (self.ended_at.unwrap_or(now) - self.created_at).max(0)
    }

    /// Compute estimate at the rented hourly price; billing stops at destroy.
    fn cost(&self, now: i64) -> f64 {
        self.rate * self.elapsed(now) as f64 / 3600.0
    }

    fn entry(&self, now: i64) -> Dict {
        let mut d = Dict::new();
        d.insert("job_id", Value::str(&self.job_id));
        d.insert("state", Value::str(&self.state));
        d.insert(
            "exit_code",
            self.exit_code.map_or(Value::Null, |c| Value::Int(c.into())),
        );
        d.insert("timed_out", Value::Bool(self.timed_out));
        d.insert("elapsed_seconds", Value::Int(self.elapsed(now)));
        d.insert("cost_usd", Value::Float(self.cost(now)));
        d.insert("price_per_hour", Value::Float(self.rate));
        d.insert("provider", Value::str(&self.provider));
        d.insert("id", Value::str(&self.instance));
        d.insert(
            "owner",
            self.owner.as_deref().map_or(Value::Null, Value::str),
        );
        d.insert("gpu", self.gpu.as_deref().map_or(Value::Null, Value::str));
        d.insert(
            "expires_at",
            Value::str(
                Timestamp::from_second(self.expires_at)
                    .map(|t| t.to_string())
                    .unwrap_or_default(),
            ),
        );
        d.insert("outputs_collected", Value::Bool(self.collected));
        d.insert("destroyed", Value::Bool(self.destroyed));
        d.insert(
            "outputs_dir",
            if self.collected {
                Value::str(self.local.join(&self.outputs).to_string_lossy())
            } else {
                Value::Null
            },
        );
        d.insert("results_dir", Value::str(self.local.to_string_lossy()));
        d.insert(
            "command",
            Value::List(self.command.iter().map(Value::str).collect()),
        );
        d
    }
}

/// Save a record, merged under the file lock with the one on disk.
fn save(ctx: &Ctx, record: &mut Record) -> Result<()> {
    let mut merged = record.clone();
    crate::atomic_file::update(&record_path(&ctx.env, &record.job_id), |stored| {
        if let Some(stored) = pyjson::loads(&String::from_utf8_lossy(stored))
            .ok()
            .and_then(|value| Record::from_value(&value))
        {
            merged.merge(&stored);
        }
        Ok((pyjson::dumps(&merged.to_value(), Some(2)) + "\n").into_bytes())
    })?;
    *record = merged;
    Ok(())
}

fn read(path: &Path) -> Result<Record> {
    let bytes = fs::read(path).map_err(|e| os_error(&e, path))?;
    pyjson::loads(&String::from_utf8_lossy(&bytes))
        .ok()
        .and_then(|value| Record::from_value(&value))
        .ok_or_else(|| {
            Error::msg(format!(
                "Job record {} is damaged. Remove it, and destroy the instance with gpu down if it is still running.",
                path.display()
            ))
        })
}

/// All saved jobs, newest first. A damaged record is reported, never hidden.
fn records(ctx: &Ctx) -> Result<(Vec<Record>, Vec<String>)> {
    let dir = jobs_dir(&ctx.env);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
        Err(e) => return Err(os_error(&e, &dir)),
    };
    let (mut found, mut damaged) = (Vec::new(), Vec::new());
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        match read(&path) {
            Ok(record) => found.push(record),
            Err(_) => damaged.push(path.display().to_string()),
        }
    }
    found.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.job_id.cmp(&b.job_id))
    });
    damaged.sort();
    Ok((found, damaged))
}

/// A job by its ID, or by the ID of its instance.
fn load(ctx: &Ctx, id: &str) -> Result<Record> {
    let missing = || {
        Error::status(
            ExitCode::NotFound,
            format!("Job {id} not found on this machine."),
        )
        .next("gpu job ls")
    };
    if !valid_id(id) {
        return Err(missing());
    }
    let path = record_path(&ctx.env, id);
    if path.exists() {
        return read(&path);
    }
    let (found, _) = records(ctx)?;
    let mut matches = found.into_iter().filter(|r| r.instance == id);
    match (matches.next(), matches.next()) {
        (Some(only), None) => Ok(only),
        (Some(_), Some(_)) => Err(Error::usage(format!(
            "Several jobs ran on instance {id}; pass the job ID from gpu job ls."
        ))),
        _ => Err(missing()),
    }
}

/// Save the record of a job that has started, and tell the caller its ID.
pub(crate) fn save_started(ctx: &Ctx, start: Start) -> Result<()> {
    let mut record = Record {
        job_id: start.run_id.into(),
        provider: Provider::Vast.key().into(),
        instance: start.instance.into(),
        owner: start.owner,
        gpu: start.gpu,
        command: start.command.to_vec(),
        rate: start.rate,
        created_at: start.created_at,
        expires_at: start.deadline,
        linger: start.linger,
        outputs: start.outputs.into(),
        local: start.local.into(),
        root: start.paths.0.clone(),
        jobdir: start.paths.1.clone(),
        state: BOOTING.into(),
        exit_code: None,
        timed_out: false,
        ended_at: None,
        collected: false,
        destroyed: false,
    };
    if !valid_id(&record.job_id) {
        return Err(Error::msg(format!("Unusable job ID {}.", record.job_id)));
    }
    save(ctx, &mut record)?;
    let now = ctx.clock.now().as_second();
    for (key, value) in record.entry(now).iter() {
        ctx.result(key, value.clone());
    }
    ctx.result("job_id", Value::str(&record.job_id));
    ctx.result("phase", Value::str("started"));
    let _ = ctx.eprintln(&format!("Results: {}", record.local.display()));
    ctx.println(&format!("Job ID: {}", record.job_id))?;
    ctx.println(&format!(
        "Running on Vast instance {} until it finishes or at most {}.",
        record.instance,
        timestamp(record.expires_at)
    ))?;
    ctx.println(&format!(
        "Next: gpu job status {id}; gpu job logs {id} --follow; gpu job outputs {id} (downloads, then destroys the instance); gpu job cancel {id}",
        id = record.job_id
    ))?;
    Ok(())
}

fn timestamp(seconds: i64) -> String {
    Timestamp::from_second(seconds)
        .map(|t| t.to_string())
        .unwrap_or_default()
}

fn duration_text(seconds: i64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// What asking the instance about a job found.
enum Seen {
    Live(Box<VastBackend>, Status),
    /// The instance no longer exists.
    Gone,
    /// The instance exists but is not running; its provider status.
    Down(String),
    /// Neither answered nor proven gone; the record keeps its last state.
    Unreachable(Error),
}

fn connect(ctx: &Ctx, record: &Record, timeout: f64) -> Result<VastBackend> {
    let (alias, _) = crate::vast::instance::wait_for_ssh(ctx, &record.instance, timeout)?;
    Ok(VastBackend::resumed(&alias))
}

/// The provider's instance list, asked once per command.
fn listed(ctx: &Ctx) -> Result<Vec<Dict>> {
    vast::list_instances(ctx, Some(15.0))
}

/// Ask for the job's state: the instance list first, then SSH only to a
/// running instance. The log is not fetched.
fn observe(ctx: &Ctx, record: &Record, live: &Result<Vec<Dict>>, timeout: f64) -> Seen {
    let live = match live {
        Ok(live) => live,
        Err(error) => return Seen::Unreachable(error.clone()),
    };
    let Some(instance) = vast::find_instance(live, &record.instance) else {
        return Seen::Gone;
    };
    match instance.get("actual_status").and_then(Value::as_str) {
        Some("running") => {}
        Some("destroyed") => return Seen::Gone,
        other => return Seen::Down(crate::redact::redact_text(other.unwrap_or("unknown"))),
    }
    let backend = match connect(ctx, record, timeout) {
        Ok(backend) => backend,
        Err(error) => return Seen::Unreachable(error),
    };
    match query_status(ctx, &backend, record) {
        Ok(status) => Seen::Live(Box::new(backend), status),
        Err(error) => Seen::Unreachable(error),
    }
}

/// The supervisor's status line; phase `lost` when its process is gone.
fn query_status(ctx: &Ctx, backend: &VastBackend, record: &Record) -> Result<Status> {
    let out = checked(
        ctx,
        backend,
        &backend.remote(&control(&record.root, &record.job_id, "status"), 15.0),
        "Job status unavailable.",
    )?;
    if out.stdout.trim() == "lost" {
        return Ok(Status {
            phase: "lost".into(),
            exit: None,
            timed: false,
            size: 0,
        });
    }
    parse_status(&out.stdout).ok_or_else(|| Error::msg("Job status response was not understood."))
}

/// A provider status the instance will not leave by itself.
fn stopped(provider: &str) -> bool {
    matches!(provider, "stopped" | "exited")
}

/// Fold an observation into the record. True when the record changed.
fn apply(record: &mut Record, seen: &Seen, now: i64) -> bool {
    let before = record.clone();
    match seen {
        Seen::Live(_, status) => {
            if status.phase == "lost" {
                if !record.finished() {
                    record.state = LOST.into();
                }
            } else if let Some(exit) = status.exit {
                record.exit_code = Some(exit);
                record.timed_out = status.timed;
                if record.state != CANCELLED {
                    record.state = if exit == 0 { SUCCEEDED } else { FAILED }.into();
                }
            } else if record.state != CANCELLED {
                record.state = if status.phase == "setup" {
                    BOOTING
                } else {
                    RUNNING
                }
                .into();
            }
        }
        Seen::Down(provider) => {
            if stopped(provider) && !record.finished() && record.state != CANCELLED {
                record.state = LOST.into();
            }
        }
        Seen::Gone => {
            record.destroyed = true;
            record.ended_at.get_or_insert(now.min(record.expires_at));
            if !record.collected && record.state != CANCELLED {
                // Outputs that were never collected are gone with the instance.
                record.state = if now >= record.expires_at - 60 {
                    EXPIRED
                } else {
                    LOST
                }
                .into();
            }
        }
        Seen::Unreachable(_) => {}
    }
    *record != before
}

/// Refresh a record from the instance and save it when it changed.
fn refresh(ctx: &Ctx, record: &mut Record, timeout: f64) -> Result<Seen> {
    if record.done() {
        return Ok(Seen::Gone);
    }
    refresh_from(ctx, record, &listed(ctx), timeout)
}

fn refresh_from(
    ctx: &Ctx,
    record: &mut Record,
    live: &Result<Vec<Dict>>,
    timeout: f64,
) -> Result<Seen> {
    let seen = observe(ctx, record, live, timeout);
    if apply(record, &seen, ctx.clock.now().as_second()) {
        save(ctx, record)?;
    }
    Ok(seen)
}

fn down_text(record: &Record, provider: &str) -> String {
    format!(
        "Vast instance {} is {provider}, not running",
        record.instance
    )
}

fn report(ctx: &Ctx, record: &Record) {
    for (key, value) in record.entry(ctx.clock.now().as_second()).iter() {
        ctx.result(key, value.clone());
    }
}

fn exit_text(record: &Record) -> String {
    record.exit_code.map_or("-".into(), |c| c.to_string())
}

fn summary(record: &Record, now: i64) -> String {
    let mut text = format!("Job {}: {}", record.job_id, record.state);
    if let Some(code) = record.exit_code {
        text.push_str(&format!(
            " (exit {code}{})",
            if record.timed_out {
                ", hit its time limit"
            } else {
                ""
            }
        ));
    }
    text.push_str(&format!(
        "\n  elapsed {}, about ${} so far at ${}/hr",
        duration_text(record.elapsed(now)),
        fixed(record.cost(now), 3),
        fixed(record.rate, 3)
    ));
    text.push_str(&format!(
        "\n  Vast instance {}{}; expires {}",
        record.instance,
        record
            .owner
            .as_deref()
            .map_or(String::new(), |o| format!(", owner {o}")),
        timestamp(record.expires_at)
    ));
    if record.destroyed {
        text.push_str("\n  instance destroyed");
    }
    if record.collected {
        text.push_str(&format!(
            "\n  outputs: {}",
            record.local.join(&record.outputs).display()
        ));
    }
    text
}

pub fn status(ctx: &Ctx, id: &str) -> Result<()> {
    let mut record = load(ctx, id)?;
    let seen = refresh(ctx, &mut record, REFRESH_CONNECT_TIMEOUT)?;
    report(ctx, &record);
    if let Seen::Live(_, status) = &seen {
        ctx.result("log_bytes", Value::Int(status.size as i64));
    }
    if let Seen::Down(provider) = &seen {
        ctx.result("provider_status", Value::str(provider));
        ctx.eprintln(&format!("{}.", down_text(&record, provider)))?;
    }
    if let Seen::Unreachable(error) = &seen {
        ctx.result("stale", Value::Bool(true));
        ctx.result("warning", Value::str(ctx.safe_diagnostic(error.text())));
        ctx.eprintln(&format!(
            "Could not reach the instance ({}); showing the last known state.",
            ctx.safe_diagnostic(error.text())
        ))?;
    }
    if !ctx.json() {
        let mut text = summary(&record, ctx.clock.now().as_second());
        match &seen {
            Seen::Live(_, status) if status.phase == "lost" && !record.collected => {
                text.push_str(&format!(
                    "\n  the job supervisor is gone, so the job will not finish; collect what it left: gpu job outputs {}",
                    record.job_id
                ));
            }
            Seen::Down(provider) if stopped(provider) => {
                text.push_str(&format!(
                    "\n  the instance is not running but still bills; destroy it: gpu job cancel {}",
                    record.job_id
                ));
            }
            _ => {}
        }
        ctx.println(&text)?;
    }
    Ok(())
}

/// The log from `offset`, fetched in chunks, `emit` called for each.
fn fetch_log(
    ctx: &Ctx,
    backend: &VastBackend,
    record: &Record,
    mut offset: u64,
    size: u64,
    emit: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<u64> {
    let chunk = tempfile::NamedTempFile::new()?;
    while offset < size {
        let amount = (size - offset).min(CHUNK);
        checked(
            ctx,
            backend,
            &log_chunk(backend, &record.root, offset, amount, chunk.path()),
            "Log connection interrupted.",
        )?;
        let bytes = fs::read(chunk.path())?;
        if bytes.len() as u64 != amount {
            return Err(Error::msg(
                "Remote log truncated; refusing to lose or duplicate bytes.",
            ));
        }
        emit(&bytes)?;
        offset += amount;
    }
    Ok(offset)
}

pub fn logs(ctx: &Ctx, id: &str, follow: bool, offset: u64) -> Result<()> {
    let mut record = load(ctx, id)?;
    let mut collected = Vec::new();
    // Under --json stdout carries only the envelope; --follow echoes to stderr.
    let mut emit = |bytes: &[u8]| -> Result<()> {
        if ctx.json() {
            if follow {
                ctx.console.write_err_bytes(bytes)?;
            } else {
                collected.extend_from_slice(bytes);
            }
            Ok(())
        } else {
            ctx.console.write_out_bytes(bytes)
        }
    };
    let (next, size) = match refresh(ctx, &mut record, REFRESH_CONNECT_TIMEOUT)? {
        Seen::Live(backend, mut status) => {
            let mut next = offset;
            loop {
                next = fetch_log(ctx, &backend, &record, next, status.size, &mut emit)?;
                if !follow
                    || status.phase == "lost"
                    || (status.exit.is_some() && next >= status.size)
                    || ctx.clock.now().as_second() >= record.expires_at
                {
                    break;
                }
                ctx.check_cancelled()?;
                ctx.clock.sleep(1.0);
                // Transient SSH failures keep a followed log alive; an
                // instance that is really gone ends it.
                let mut retry = 1.0f64;
                status = loop {
                    match query_status(ctx, &backend, &record) {
                        Ok(fresh) => break fresh,
                        Err(error) => {
                            if backend.lost(ctx, &record.instance) {
                                return Err(Error::status(
                                    ExitCode::InstanceLost,
                                    "Instance stopped or vanished while following the log.",
                                ));
                            }
                            ctx.check_cancelled()?;
                            if retry == 1.0 {
                                ctx.eprintln(&format!(
                                    "Connection lost ({}); reconnecting.",
                                    ctx.safe_diagnostic(error.text())
                                ))?;
                            }
                            ctx.clock.sleep(retry);
                            retry = (retry * 2.0).min(15.0);
                        }
                    }
                };
            }
            let size = status.size;
            if apply(
                &mut record,
                &Seen::Live(backend, status),
                ctx.clock.now().as_second(),
            ) {
                save(ctx, &mut record)?;
            }
            (next, size)
        }
        // Collected jobs keep their log beside the outputs.
        _ if record.collected => {
            let path = record.local.join("log.txt");
            let bytes = fs::read(&path).map_err(|e| os_error(&e, &path))?;
            let skip = (offset as usize).min(bytes.len());
            emit(&bytes[skip..])?;
            (bytes.len() as u64, bytes.len() as u64)
        }
        Seen::Unreachable(error) => {
            return Err(Error::msg(format!(
                "Could not reach the instance for the log: {}",
                ctx.safe_diagnostic(error.text())
            ))
            .next(format!("gpu job status {}", record.job_id)));
        }
        Seen::Down(provider) => {
            return Err(Error::msg(format!(
                "{}, so the log of job {} cannot be read.",
                down_text(&record, &provider),
                record.job_id
            ))
            .next(format!("gpu job status {}", record.job_id)));
        }
        Seen::Gone => {
            return Err(Error::status(
                ExitCode::InstanceLost,
                format!(
                    "Job {} has no log left: its instance is gone and no outputs were collected.",
                    record.job_id
                ),
            ));
        }
    };
    report(ctx, &record);
    ctx.result("offset", Value::Int(offset as i64));
    ctx.result("next_offset", Value::Int(next as i64));
    ctx.result("log_bytes", Value::Int(size as i64));
    if ctx.json() && !follow {
        ctx.result("log", Value::str(String::from_utf8_lossy(&collected)));
    }
    Ok(())
}

pub fn outputs(ctx: &Ctx, id: &str) -> Result<()> {
    let mut record = load(ctx, id)?;
    if record.collected || record.state == CANCELLED {
        if record.state == CANCELLED {
            return Err(Error::msg(format!(
                "Job {} was cancelled; it has no outputs.",
                record.job_id
            )));
        }
        // Collecting twice is harmless: say where the outputs are.
        report(ctx, &record);
        ctx.result(
            "log_path",
            Value::str(record.local.join("log.txt").to_string_lossy()),
        );
        if !ctx.json() {
            ctx.println(&summary(&record, ctx.clock.now().as_second()))?;
        }
        return Ok(());
    }
    let seen = refresh(ctx, &mut record, CONNECT_TIMEOUT)?;
    let backend = match seen {
        Seen::Live(backend, status) => {
            if status.phase == "lost" {
                ctx.eprintln(&format!(
                    "The supervisor of job {} is gone, so the job will not finish; downloading whatever is in its outputs folder.",
                    record.job_id
                ))?;
            } else if status.exit.is_none() {
                return Err(Error::msg(format!(
                    "Job {} is still {}; outputs can be collected once it finishes.",
                    record.job_id, record.state
                ))
                .next(format!("gpu job logs {} --follow", record.job_id)));
            }
            backend
        }
        Seen::Down(provider) => {
            report(ctx, &record);
            return Err(Error::msg(format!(
                "{}, so the outputs of job {} cannot be downloaded now.",
                down_text(&record, &provider),
                record.job_id
            ))
            .next(format!("gpu job cancel {}", record.job_id)));
        }
        Seen::Gone => {
            report(ctx, &record);
            return Err(Error::status(
                ExitCode::InstanceLost,
                format!(
                    "Job {} is {}: its instance is gone and the outputs were not collected{}.",
                    record.job_id,
                    record.state,
                    record
                        .exit_code
                        .map_or(String::new(), |c| format!(" (the job exited with {c})"))
                ),
            ));
        }
        Seen::Unreachable(error) => return Err(error),
    };
    let deadline = record.deadline()?;
    fs::create_dir_all(&record.local).map_err(|e| os_error(&e, &record.local))?;
    let log = record.local.join("log.txt");
    if !log.exists() {
        fs::write(&log, b"").map_err(|e| os_error(&e, &log))?;
    }
    let paths = (record.root.clone(), record.jobdir.clone());
    let session = Session {
        created_id: Some(record.instance.clone()),
        ledger: None,
        run_id: record.job_id.clone(),
        local: record.local.clone(),
        paths: Some(paths.clone()),
        launched: true,
        rate: record.rate,
        created_at: record.created_at,
    };
    let phase = Phase::start(ctx, "download");
    let owned = Owned {
        phase: &phase,
        outputs: &record.outputs,
        deadline,
        backend: backend.as_ref(),
        session: &session,
        id: &record.instance,
    };
    let mut outcome = Ok(());
    recover(&owned, &paths, &mut outcome);
    if let Err(error) = outcome {
        // The instance is the only copy of the outputs: keep it, so the
        // download can be retried until it expires.
        return Err(Error::msg(format!(
            "{} The instance is kept so the download can be retried until {}: gpu job outputs {}",
            error.text(),
            timestamp(record.expires_at),
            record.job_id
        ))
        .next(format!("gpu job outputs {}", record.job_id)));
    }
    record.collected = true;
    let now = ctx.clock.now().as_second();
    let closing: Result<()>;
    phase.set("cleanup");
    if record.linger == 0 {
        closing = backend.destroy(ctx, &record.instance);
        if closing.is_ok() {
            record.destroyed = true;
            record.ended_at = Some(now);
            ctx.result("destroyed", Value::Bool(true));
        }
    } else {
        // `--linger`: keep the instance for that long, never past its ceiling.
        let until = (now + record.linger).min(record.expires_at);
        closing = backend
            .restore_expiry(
                ctx,
                &record.instance,
                Timestamp::from_second(until).map_err(|_| Error::msg("Invalid linger."))?,
            )
            .map_err(|e| {
                Error::msg(format!(
                    "Outputs were downloaded but the instance's retention could not be set: {}",
                    e.text()
                ))
            });
        ctx.result("destroyed", Value::Bool(false));
        ctx.result("retained_until", Value::str(timestamp(until)));
    }
    save(ctx, &mut record)?;
    report(ctx, &record);
    ctx.result(
        "log_path",
        Value::str(record.local.join("log.txt").to_string_lossy()),
    );
    ctx.result(
        "log_tail",
        crate::run_files::tail(&record.local.join("log.txt"))
            .map(Value::Str)
            .unwrap_or(Value::Null),
    );
    ctx.result(
        "phase",
        Value::str(if closing.is_ok() { "done" } else { "cleanup" }),
    );
    let object = crate::output::envelope(ctx, "job outputs", &closing);
    let written = fs::write(
        record.local.join("result.json"),
        pyjson::dumps(&object, None),
    );
    let _ = ctx.eprintln(&format!("Results: {}", record.local.display()));
    closing?;
    written?;
    if !ctx.json() {
        ctx.println(&summary(&record, now))?;
    }
    Ok(())
}

pub fn cancel(ctx: &Ctx, id: &str) -> Result<()> {
    let mut record = load(ctx, id)?;
    if !record.done() {
        // Best effort: ask the supervisor to stop the job before the
        // instance goes, so its last log is not cut mid-write on the host.
        if let Ok(backend) = connect(ctx, &record, CANCEL_CONNECT_TIMEOUT) {
            let _ = checked(
                ctx,
                &backend,
                &backend.remote(&control(&record.root, &record.job_id, "stop"), 10.0),
                "Stop unverified.",
            );
        }
        if !record.finished() {
            record.state = CANCELLED.into();
        }
        let destroyed = VastBackend::resumed("").destroy(ctx, &record.instance);
        if destroyed.is_ok() {
            record.destroyed = true;
            record.ended_at = Some(ctx.clock.now().as_second());
            ctx.result("destroyed", Value::Bool(true));
        }
        // Remember the intent even when destroying failed, so a retry is safe.
        save(ctx, &mut record)?;
        report(ctx, &record);
        destroyed?;
    } else {
        report(ctx, &record);
        ctx.result("destroyed", Value::Bool(true));
    }
    if !ctx.json() {
        ctx.println(&summary(&record, ctx.clock.now().as_second()))?;
    }
    Ok(())
}

pub fn ls(ctx: &Ctx, owner: Option<&str>) -> Result<()> {
    let (mut found, damaged) = records(ctx)?;
    if let Some(owner) = owner {
        found.retain(|r| r.owner.as_deref() == Some(owner));
    }
    let mut unreachable = 0;
    let mut providers = Vec::with_capacity(found.len());
    let live = found
        .iter()
        .any(|r| !r.done() && !r.collected)
        .then(|| listed(ctx));
    for record in &mut found {
        let mut provider = None;
        if let (Some(live), false, false) = (&live, record.done(), record.collected) {
            match refresh_from(ctx, record, live, REFRESH_CONNECT_TIMEOUT)? {
                Seen::Unreachable(_) => unreachable += 1,
                Seen::Down(status) => provider = Some(status),
                _ => {}
            }
        }
        providers.push(provider);
    }
    let now = ctx.clock.now().as_second();
    ctx.result(
        "jobs",
        Value::List(
            found
                .iter()
                .zip(&providers)
                .map(|(r, provider)| {
                    let mut entry = r.entry(now);
                    if let Some(provider) = provider {
                        entry.insert("provider_status", Value::str(provider));
                    }
                    Value::Dict(entry)
                })
                .collect(),
        ),
    );
    ctx.result("unreachable", Value::Int(unreachable));
    ctx.result(
        "damaged_records",
        Value::List(damaged.iter().map(Value::str).collect()),
    );
    if !ctx.json() {
        if found.is_empty() {
            ctx.println("No jobs.")?;
        } else {
            let mut rows = vec![[
                "ID", "State", "Exit", "Elapsed", "Cost", "Instance", "Owner",
            ]
            .map(String::from)];
            for r in &found {
                rows.push([
                    r.job_id.clone(),
                    r.state.clone(),
                    exit_text(r),
                    duration_text(r.elapsed(now)),
                    format!("${}", fixed(r.cost(now), 3)),
                    r.instance.clone(),
                    r.owner.clone().unwrap_or_default(),
                ]);
            }
            use crate::table::Column::{Left, Plain, Right};
            for line in
                crate::table::aligned_rows(&rows, [Left, Left, Right, Right, Right, Left, Plain])
            {
                ctx.println(&line)?;
            }
        }
        for path in &damaged {
            ctx.eprintln(&format!("Skipped damaged job record {path}."))?;
        }
    }
    Ok(())
}
