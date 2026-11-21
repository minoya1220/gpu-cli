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
    for extension in ["json", "files"] {
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

/// A path that is safe to use relative to a directory and to put in the
/// baseline table: normal components only, no control characters.
pub(crate) fn safe_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.chars().any(char::is_control)
        && Path::new(path)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}
