//! Who launched an instance: `--owner`, else `GPU_OWNER`, stored in the label.
//!
//! The listed label is `REPO` or `REPO@OWNER`. `tag` replaces `@` in the repo
//! part and an owner never contains one, so the split is exact and instances
//! launched before this existed (no `@`) simply have no owner. Vast, Runpod
//! and Modal store that label as is; GCP keeps the owner in a TPU label and
//! DigitalOcean in a Droplet tag, and their inventories rebuild it. The owner
//! alphabet is what both of those accept.

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::provider::Provider;
use crate::pyfmt::py_str;
use crate::pyjson::{Dict, Value};

/// Config key carrying the resolved owner of the current launch.
pub const CFG_KEY: &str = "launch_owner";
const MAX_LEN: usize = 32;

fn valid(owner: &str) -> bool {
    !owner.is_empty()
        && owner.len() <= MAX_LEN
        && owner
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
}

/// `--owner`, else `GPU_OWNER`, else none. An empty value counts as unset.
pub fn resolve(ctx: &Ctx, flag: Option<&str>) -> Result<Option<String>> {
    let (value, source) = match flag.filter(|v| !v.is_empty()) {
        Some(value) => (value, "--owner"),
        None => match ctx.env.get("GPU_OWNER").filter(|v| !v.is_empty()) {
            Some(value) => (value, "GPU_OWNER"),
            None => return Ok(None),
        },
    };
    if !valid(value) {
        return Err(Error::usage(format!(
            "Invalid owner from {source}: use 1-{MAX_LEN} lowercase letters, digits, '_' or '-'."
        )));
    }
    Ok(Some(value.to_string()))
}

/// The owner resolved for this launch, if any.
pub fn from_cfg(cfg: &Dict) -> Option<String> {
    cfg.get(CFG_KEY).map(py_str).filter(|o| !o.is_empty())
}

/// The provider label for `repo`, tagged with the launch's owner.
pub fn tag(repo: &str, cfg: &Dict) -> String {
    labeled(repo, from_cfg(cfg).as_deref())
}

/// The label for `repo` launched by `owner`.
pub fn labeled(repo: &str, owner: Option<&str>) -> String {
    let repo = repo.replace('@', "_");
    match owner {
        Some(owner) => format!("{repo}@{owner}"),
        None => repo,
    }
}

/// Split a stored label into its repository part and owner.
pub fn split(label: &str) -> (&str, Option<&str>) {
    match label.rsplit_once('@') {
        Some((repo, owner)) if valid(owner) => (repo, Some(owner)),
        _ => (label, None),
    }
}

/// The owner recorded on a listed instance.
pub fn of(item: &Dict) -> Option<String> {
    match item.get("label") {
        Some(Value::Str(label)) => split(label).1.map(String::from),
        _ => None,
    }
}

/// Statuses of an instance that is gone or no longer running work.
const ENDED: [&str; 12] = [
    "exited",
    "stopped",
    "stopping",
    "destroyed",
    "deleted",
    "deleting",
    "terminated",
    "terminating",
    "preempted",
    "suspended",
    "off",
    "outputs-only",
];

/// Warn (never refuse) when the launch's owner already has a not-yet-ended
/// instance on `provider`.
pub fn warn_duplicate(ctx: &Ctx, cfg: &Dict, provider: Provider) -> Result<()> {
    let Some(owner) = from_cfg(cfg) else {
        return Ok(());
    };
    // Best effort: an unreachable inventory must not block the launch.
    let Ok(items) = crate::provider::listed_items(ctx, cfg, provider) else {
        return Ok(());
    };
    let ids: Vec<String> = items
        .iter()
        .filter(|item| of(item).as_deref() == Some(owner.as_str()))
        .filter(|item| {
            let status = py_str(item.get("actual_status").unwrap_or(&Value::Null));
            !ENDED.iter().any(|ended| status.eq_ignore_ascii_case(ended))
        })
        .filter_map(|item| item.get("id").map(py_str))
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let message = format!(
        "Owner {owner} already has a {} instance that has not ended: {}. Continuing; gpu ls --owner {owner} lists them.",
        provider.label(),
        ids.join(", ")
    );
    ctx.result("owner_warning", Value::str(&message));
    ctx.eprintln(&format!("Warning: {message}"))
}
