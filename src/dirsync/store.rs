//! Link records and last-synced baselines in the CLI's local state directory.
//!
//! One link ties a local directory to the same directory on one instance. Its
//! record (`<key>.json`) is small; the baseline of every file as of the last
//! sync (`<key>.files`) is a tab-separated table, because it can hold
//! hundreds of thousands of paths.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::ctx::Env;
use crate::error::{os_error, Error, Result};
use crate::pyjson::{self, Dict, Value};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Link {
    /// The instance ID as `gpu ls` prints it.
    pub id: String,
    /// Canonical local directory.
    pub local: PathBuf,
    /// The twin directory on the instance.
    pub remote: String,
    /// Gitignored paths (relative to the directory) that pulls include anyway.
    pub include: Vec<String>,
    pub linked_at: String,
}

impl Link {
    pub fn provider(&self) -> crate::provider::Provider {
        let provider = self.id.split(':').next().unwrap_or("");
        crate::provider::Provider::from_key(provider).unwrap_or(crate::provider::Provider::Vast)
    }

    fn key(&self) -> String {
        let id: String = self
            .id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .take(60)
            .collect();
        // FNV-1a over the directory keeps one instance's links to different
        // directories apart without putting a path in a file name.
        let hash = self
            .local
            .to_string_lossy()
            .bytes()
            .fold(0xcbf29ce484222325u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
        format!("{id}-{:08x}", hash & 0xffff_ffff)
    }

    fn value(&self) -> Value {
        let mut dict = Dict::new();
        dict.insert("id", Value::str(&self.id));
        dict.insert("local", Value::str(self.local.to_string_lossy()));
        dict.insert("remote", Value::str(&self.remote));
        dict.insert(
            "include",
            Value::List(self.include.iter().map(Value::str).collect()),
        );
        dict.insert("linked_at", Value::str(&self.linked_at));
        Value::Dict(dict)
    }

    fn parse(value: &Value) -> Option<Link> {
        let text = |key| value.get(key).and_then(Value::as_str).map(String::from);
        Some(Link {
            id: text("id")?,
            local: PathBuf::from(text("local")?),
            remote: text("remote")?,
            include: value
                .get("include")
                .and_then(Value::as_list)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            linked_at: text("linked_at").unwrap_or_default(),
        })
    }
}

/// What one file looked like when the two sides last agreed.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    /// Size and modification time on the instance (opaque: only ever compared
    /// with a later listing from the same instance).
    pub remote_size: u64,
    pub remote_mtime: String,
    /// Size and modification time (ns) of the local file as last written or
    /// uploaded; `None` when the local file was deliberately left different.
    pub local: Option<(u64, i128)>,
    /// SHA-256 of the agreed content, once known.
    pub hash: Option<String>,
}

pub(crate) type Base = BTreeMap<String, Entry>;

pub(crate) fn dir(env: &Env) -> PathBuf {
    env.xdg("XDG_STATE_HOME", ".local/state")
        .join("gpu")
        .join("sync")
}

fn file(env: &Env, link: &Link, extension: &str) -> PathBuf {
    dir(env).join(format!("{}.{extension}", link.key()))
}

/// Every recorded link, sorted by instance and directory. Unreadable records
/// are skipped: a damaged file must not stop `gpu down`.
pub(crate) fn links(env: &Env) -> Vec<Link> {
    let Ok(entries) = fs::read_dir(dir(env)) else {
        return Vec::new();
    };
    let mut links: Vec<Link> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .filter_map(|text| pyjson::loads(&text).ok())
        .filter_map(|value| Link::parse(&value))
        .collect();
    links.sort_by(|a, b| (&a.id, &a.local).cmp(&(&b.id, &b.local)));
    links
}

pub(crate) fn save_link(env: &Env, link: &Link) -> Result<()> {
    let text = pyjson::dumps(&link.value(), Some(2)) + "\n";
    crate::atomic_file::update(&file(env, link, "json"), |_| Ok(text.clone().into_bytes()))
}

pub(crate) fn remove_link(env: &Env, link: &Link) {
    // The autosync lock stays: a syncer still winding down holds it, and a
    // new one must not start beside it.
    for extension in ["json", "files", "status"] {
        let _ = fs::remove_file(file(env, link, extension));
    }
    let _ = fs::remove_file(file(env, link, "lock"));
}

pub(crate) fn load_base(env: &Env, link: &Link) -> Base {
    let Ok(text) = fs::read_to_string(file(env, link, "files")) else {
        return Base::new();
    };
    text.lines().filter_map(parse_row).collect()
}

fn parse_row(line: &str) -> Option<(String, Entry)> {
    let mut fields = line.split('\t');
    let path = fields.next()?.to_string();
    let remote_size = fields.next()?.parse().ok()?;
    let remote_mtime = fields.next()?.to_string();
    let local = match (fields.next()?, fields.next()?) {
        ("-", _) => None,
        (size, mtime) => Some((size.parse().ok()?, mtime.parse().ok()?)),
    };
    let hash = match fields.next()? {
        "-" => None,
        hash => Some(hash.to_string()),
    };
    Some((
        path,
        Entry {
            remote_size,
            remote_mtime,
            local,
            hash,
        },
    ))
}

pub(crate) fn save_base(env: &Env, link: &Link, base: &Base) -> Result<()> {
    let mut text = String::new();
    for (path, entry) in base {
        let (size, mtime) = match entry.local {
            Some((size, mtime)) => (size.to_string(), mtime.to_string()),
            None => ("-".into(), "-".into()),
        };
        text.push_str(&format!(
            "{path}\t{}\t{}\t{size}\t{mtime}\t{}\n",
            entry.remote_size,
            entry.remote_mtime,
            entry.hash.as_deref().unwrap_or("-")
        ));
    }
    crate::atomic_file::update(&file(env, link, "files"), |_| Ok(text.clone().into_bytes()))
}

/// Held while one process syncs a link; released on drop or exit.
pub(crate) struct Lock(#[allow(dead_code)] fs::File);

/// The link's lock, or `None` when another sync holds it.
pub(crate) fn lock(env: &Env, link: &Link) -> Result<Option<Lock>> {
    let path = file(env, link, "lock");
    let parent = path.parent().expect("state directory");
    fs::create_dir_all(parent).map_err(|e| os_error(&e, parent))?;
    let handle = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| os_error(&e, &path))?;
    match rustix::fs::flock(
        &handle,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    ) {
        Ok(()) => Ok(Some(Lock(handle))),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(error) => Err(Error::msg(format!(
            "Cannot lock {}: {}",
            path.display(),
            std::io::Error::from(error)
        ))),
    }
}

/// The link's lock, waiting up to `seconds` for another sync to finish.
pub(crate) fn lock_wait(ctx: &crate::ctx::Ctx, link: &Link, seconds: f64) -> Result<Option<Lock>> {
    let end = ctx.clock.monotonic() + seconds;
    loop {
        if let Some(held) = lock(&ctx.env, link)? {
            return Ok(Some(held));
        }
        if ctx.clock.monotonic() >= end {
            return Ok(None);
        }
        ctx.clock.sleep(0.25);
    }
}

/// Where a link's background syncer writes its log.
pub(crate) fn log_path(env: &Env, link: &Link) -> PathBuf {
    file(env, link, "log")
}

/// Held for as long as a link's background syncer lives (a crash releases
/// it too), so at most one runs per link.
pub(crate) struct AutosyncLock(#[allow(dead_code)] fs::File);

fn open_autosync(env: &Env, link: &Link) -> Result<fs::File> {
    let path = file(env, link, "autosync");
    let parent = path.parent().expect("state directory");
    fs::create_dir_all(parent).map_err(|e| os_error(&e, parent))?;
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| os_error(&e, &path))
}

/// Claim the link for this process (recording its ID), or `None` when a
/// background syncer already runs.
pub(crate) fn autosync_claim(env: &Env, link: &Link) -> Result<Option<AutosyncLock>> {
    use std::io::Write;

    let mut handle = open_autosync(env, link)?;
    match rustix::fs::flock(
        &handle,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    ) {
        Ok(()) => {
            handle
                .set_len(0)
                .and_then(|()| writeln!(handle, "{}", std::process::id()))
                .map_err(|e| os_error(&e, &file(env, link, "autosync")))?;
            Ok(Some(AutosyncLock(handle)))
        }
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(error) => Err(Error::msg(format!(
            "Cannot lock the autosync record: {}",
            std::io::Error::from(error)
        ))),
    }
}

/// The process ID of the link's running background syncer, if there is one.
pub(crate) fn autosync_pid(env: &Env, link: &Link) -> Option<u32> {
    let path = file(env, link, "autosync");
    if !path.exists() {
        return None;
    }
    let handle = open_autosync(env, link).ok()?;
    match rustix::fs::flock(
        &handle,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    ) {
        Err(rustix::io::Errno::WOULDBLOCK) => {
            let text = fs::read_to_string(&path).ok()?;
            Some(text.trim().parse().unwrap_or(0))
        }
        _ => None,
    }
}

/// What the background syncer last reported about itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Status {
    pub started_at: String,
    /// "syncing", "reconnecting", or why it stopped.
    pub state: String,
    pub last_pull_at: String,
    pub last_pull_files: u64,
    /// The auto-destroy deadline the syncer pulls before, when it knows one.
    pub deadline: String,
    /// The latest errors, oldest first: (time, redacted text).
    pub errors: Vec<(String, String)>,
}

/// Errors kept in the status record.
pub(crate) const STATUS_ERRORS: usize = 3;

pub(crate) fn save_status(env: &Env, link: &Link, status: &Status) -> Result<()> {
    let mut dict = Dict::new();
    dict.insert("started_at", Value::str(&status.started_at));
    dict.insert("state", Value::str(&status.state));
    dict.insert("last_pull_at", Value::str(&status.last_pull_at));
    dict.insert("last_pull_files", Value::Int(status.last_pull_files as i64));
    dict.insert("deadline", Value::str(&status.deadline));
    dict.insert(
        "errors",
        Value::List(
            status
                .errors
                .iter()
                .map(|(at, text)| {
                    let mut item = Dict::new();
                    item.insert("at", Value::str(at));
                    item.insert("text", Value::str(text));
                    Value::Dict(item)
                })
                .collect(),
        ),
    );
    let text = pyjson::dumps(&Value::Dict(dict), Some(2)) + "\n";
    crate::atomic_file::update(&file(env, link, "status"), |_| {
        Ok(text.clone().into_bytes())
    })
}

pub(crate) fn load_status(env: &Env, link: &Link) -> Option<Status> {
    let text = fs::read_to_string(file(env, link, "status")).ok()?;
    let value = pyjson::loads(&text).ok()?;
    let text_of = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Some(Status {
        started_at: text_of("started_at"),
        state: text_of("state"),
        last_pull_at: text_of("last_pull_at"),
        last_pull_files: match value.get("last_pull_files") {
            Some(Value::Int(n)) => (*n).max(0) as u64,
            _ => 0,
        },
        deadline: text_of("deadline"),
        errors: value
            .get("errors")
            .and_then(Value::as_list)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let field = |key| item.get(key).and_then(Value::as_str).map(String::from);
                        Some((field("at")?, field("text")?))
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// A path that is safe to use relative to a directory and to put in the
/// baseline table: normal components only, no control characters.
pub(crate) fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.chars().any(char::is_control)
        && Path::new(path)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}
