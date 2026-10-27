//! Repository choices shared by flags, picker, and startup generation.

use std::io::Write;
use std::path::PathBuf;
use std::sync::LazyLock;

use regex::Regex;

use crate::ctx::Env;
use crate::error::{os_error, Error, Result};
use crate::pyfmt::is_printable;
use crate::pyjson::{self, Value};

pub const ERROR: &str =
    "Use a bare repo name, owner/name, https://host/path/repo.git, or git@host:path/repo.git (no credentials or query).";

/// A repository pick. `NoRepo` is written as `""` wherever Python writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Repo {
    NoRepo,
    Named(String),
}

impl Repo {
    /// The flag value: an empty string means no repository, as in Python.
    pub fn from_arg(value: &str) -> Repo {
        if value.is_empty() {
            Repo::NoRepo
        } else {
            Repo::Named(value.to_string())
        }
    }

    pub fn as_arg(&self) -> &str {
        match self {
            Repo::NoRepo => "",
            Repo::Named(name) => name,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Repo::NoRepo => "No repository",
            Repo::Named(name) => name,
        }
    }
}

static PART: &str = r"[A-Za-z0-9._-]+";
static NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._-]+$").unwrap());
static PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^{PART}(?:/{PART})*$")).unwrap());
static NETLOC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9.-]+(?::[0-9]+)?$").unwrap());
static SCP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^git@[A-Za-z0-9.-]+:({PART}(?:/{PART})*)/?$")).unwrap());
static SHORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^{PART}(?:/{PART})?$")).unwrap());
static OWNER_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^{PART}/{PART}$")).unwrap());
static UNSAFE_OWNER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^A-Za-z0-9_-]").unwrap());

/// The pieces of `urllib.parse.urlsplit` this module relies on.
struct Split<'a> {
    netloc: &'a str,
    path: &'a str,
    query: &'a str,
    fragment: &'a str,
}

fn urlsplit(rest: &str) -> Result<Split<'_>> {
    // `rest` follows "https://": the netloc ends at the first '/', '?' or '#'.
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let netloc = &rest[..end];
    if netloc.contains('[') != netloc.contains(']') {
        return Err(Error::msg("Invalid IPv6 URL"));
    }
    let remainder = &rest[end..];
    let (remainder, fragment) = remainder.split_once('#').unwrap_or((remainder, ""));
    let (path, query) = remainder.split_once('?').unwrap_or((remainder, ""));
    Ok(Split {
        netloc,
        path,
        query,
        fragment,
    })
}

fn hostname(netloc: &str) -> String {
    let host = netloc.rsplit_once('@').map_or(netloc, |(_, host)| host);
    let host = match host.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    host.to_lowercase()
}

/// A validated clone URL and safe workspace basename; never touches the network.
pub fn resolve_repo(repo: &str, owner: &str) -> Result<(String, String)> {
    if repo.is_empty() {
        return Ok((String::new(), "no-repo".into()));
    }
    let reject = || Error::msg(ERROR);
    if repo.chars().any(|c| c.is_whitespace() || !is_printable(c)) {
        return Err(reject());
    }
    let (path, clone_url) = if let Some(rest) = repo.strip_prefix("https://") {
        let url = urlsplit(rest)?;
        let userinfo = url.netloc.contains('@');
        if hostname(url.netloc).is_empty()
            || userinfo
            || !url.query.is_empty()
            || !url.fragment.is_empty()
            || !NETLOC.is_match(url.netloc)
            || !PATH.is_match(url.path.trim_matches('/'))
        {
            return Err(reject());
        }
        (url.path.trim_end_matches('/').to_string(), repo.to_string())
    } else if repo.starts_with("git@") {
        let captures = SCP.captures(repo).ok_or_else(reject)?;
        (captures[1].to_string(), repo.to_string())
    } else {
        if !SHORT.is_match(repo) {
            return Err(reject());
        }
        let path = if repo.contains('/') {
            repo.to_string()
        } else {
            format!("{owner}/{repo}")
        };
        if !OWNER_NAME.is_match(&path) {
            return Err(reject());
        }
        let url = format!("https://github.com/{path}");
        (path, url)
    };
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err(reject());
    }
    let name = path.rsplit('/').next().unwrap_or("");
    let name = name.strip_suffix(".git").unwrap_or(name);
    if name == "." || name == ".." || !NAME.is_match(name) {
        return Err(reject());
    }
    Ok((clone_url, name.to_string()))
}

/// Account-scoped repository names, never credentials.
pub fn cache_path(env: &Env, owner: &str) -> PathBuf {
    let safe_owner = UNSAFE_OWNER.replace_all(owner, "_");
    env.xdg("XDG_CACHE_HOME", ".cache")
        .join("gpu")
        .join(format!("repos-{safe_owner}.json"))
}

fn names(value: &Value) -> Vec<String> {
    let Some(items) = value.as_list() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(Value::as_str)
        .filter(|name| NAME.is_match(name) && *name != "." && *name != "..")
        .map(String::from)
        .collect()
}

pub fn load_cached_repos(env: &Env, owner: &str) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(cache_path(env, owner)).ok()?;
    pyjson::loads(&text).ok().map(|value| names(&value))
}

pub fn save_cached_repos(env: &Env, owner: &str, repos: &[String]) -> Result<()> {
    let path = cache_path(env, owner);
    let parent = path.parent().unwrap_or(&path).to_path_buf();
    std::fs::create_dir_all(&parent).map_err(|e| os_error(&e, &parent))?;
    let list = Value::List(repos.iter().map(|r| Value::str(r.as_str())).collect());
    let text = pyjson::dumps(
        &Value::List(names(&list).into_iter().map(Value::Str).collect()),
        None,
    );
    let mut temp = tempfile::Builder::new()
        .prefix("tmp")
        .tempfile_in(&parent)
        .map_err(|e| os_error(&e, &parent))?;
    temp.write_all(text.as_bytes())?;
    temp.persist(&path).map_err(|e| os_error(&e.error, &path))?;
    Ok(())
}
