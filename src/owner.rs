//! Who launched an instance: `--owner`, else `GPU_OWNER`, stored in the label.
//!
//! The provider label is `REPO` or `REPO@OWNER`. `@` never occurs in a
//! repository name (`repos::is_name`) or an owner, so the split is exact and
//! instances launched before this existed (no `@`) simply have no owner.

use crate::ctx::Ctx;
use crate::error::{Error, Result};
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
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
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
            "Invalid owner from {source}: use 1-{MAX_LEN} letters, digits, '.', '_' or '-'."
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
    match from_cfg(cfg) {
        Some(owner) => format!("{}@{owner}", repo.replace('@', "_")),
        None => repo.to_string(),
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

/// Warn (never refuse) when `owner` already has a running instance.
pub fn warn_duplicate(ctx: &Ctx, cfg: &Dict, owner: &str) -> Result<()> {
    // Best effort: an unreachable inventory must not block the launch.
    let Ok(items) = crate::provider::listed_items(ctx, cfg) else {
        return Ok(());
    };
    let ids: Vec<String> = items
        .iter()
        .filter(|item| of(item).as_deref() == Some(owner))
        .filter(|item| {
            py_str(item.get("actual_status").unwrap_or(&Value::Null))
                .eq_ignore_ascii_case("running")
        })
        .filter_map(|item| item.get("id").map(py_str))
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let message = format!(
        "Owner {owner} already has a running instance: {}. Continuing; gpu ls --owner {owner} lists them.",
        ids.join(", ")
    );
    ctx.result("owner_warning", Value::str(&message));
    ctx.eprintln(&format!("Warning: {message}"))
}
