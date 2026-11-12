//! Instance labels, uptime, and instance selection (no lifecycle calls here).

use std::sync::LazyLock;

use regex::Regex;

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::picker::{pick, Next, Opt, Screen};
use crate::pyfmt::{fixed, py_float, py_str, truthy};
use crate::pyjson::{Dict, Value};

static SAFE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._-]+$").unwrap());
static CLONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"git clone [^\n]+ /workspace/([A-Za-z0-9._-]+)(?:\s|\z)").unwrap()
});

/// Use metadata stored on the instance by `gpu up`, never local history.
pub fn repo_name(instance: &Dict) -> Option<String> {
    if let Some(Value::Str(label)) = instance.get("label") {
        if label != "no-repo" && SAFE_NAME.is_match(label) && label != "." && label != ".." {
            return Some(label.clone());
        }
    }
    // Older instances may have a clone command but no label.
    let script = [instance.get("onstart"), instance.get("onstart_cmd")]
        .into_iter()
        .flatten()
        .find(|value| truthy(value));
    if let Some(Value::Str(script)) = script {
        if let Some(captures) = CLONE.captures(script) {
            let name = &captures[1];
            if name != "." && name != ".." {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Format Vast duration seconds, truncating partial units; invalid values are unknown.
pub fn format_uptime(value: Option<&Value>) -> String {
    let seconds = match value {
        None | Some(Value::Bool(_)) => return "-".into(),
        Some(value) => match py_float(value) {
            Ok(seconds) => seconds,
            Err(_) => return "-".into(),
        },
    };
    if !seconds.is_finite() || !(0.0..18_446_744_073_709_551_616.0).contains(&seconds) {
        return "-".into();
    }
    let seconds = seconds as u64;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let (hours, minutes) = (minutes / 60, minutes % 60);
    if hours < 24 {
        return format!("{hours}h {minutes:02}m");
    }
    let (days, hours) = (hours / 24, hours % 24);
    format!("{days}d {hours:02}h")
}

/// Only show a short TPU identifier if the live inventory proves that both its
/// compact and literal-name interpretation point at exactly this node.
pub(crate) fn display_ids(items: &[Dict], long: bool) -> Result<Vec<String>> {
    let ids = items
        .iter()
        .map(|item| {
            let full = item.get("id").map(py_str).unwrap_or_else(|| "-".into());
            let tpu = full
                .starts_with("gcp:")
                .then(|| crate::gcp::TpuId::parse(&full))
                .transpose()?;
            Ok((full, tpu))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for (_, tpu) in &ids {
        if let Some(tpu) = tpu {
            for name in [tpu.short_name(), tpu.name.clone()]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
            {
                *counts.entry(name).or_default() += 1;
            }
        }
    }
    Ok(ids
        .into_iter()
        .map(|(full, tpu)| {
            let Some(tpu) = tpu.filter(|_| !long) else {
                return full;
            };
            [tpu.short_name(), tpu.name]
                .into_iter()
                .find(|name| crate::provider::short_candidate(name) && counts.get(name) == Some(&1))
                .unwrap_or(full)
        })
        .collect())
}

fn table_price(item: &Dict) -> String {
    item.get("dph_total")
        .or_else(|| item.get("dph"))
        .filter(|v| !matches!(v, Value::Bool(_)))
        .and_then(|v| py_float(v).ok())
        .filter(|p| p.is_finite() && *p >= 0.0)
        .map(|p| format!("${}/hr", fixed(p, 2)))
        .unwrap_or_else(|| "-".into())
}

/// Human output only. JSON keeps full identifiers, original precision/case and
/// provider-specific fields. One column measurement covers every provider.
pub(crate) fn render_table(ctx: &Ctx, items: &[Dict], long: bool) -> Result<()> {
    if items.is_empty() {
        return ctx.println("No instances. Launch one: gpu up");
    }
    let ids = display_ids(items, long)?;
    let mut cells = vec![["ID", "Provider", "Type", "Status", "Price", "Uptime"].map(String::from)];
    for (item, id) in items.iter().zip(ids) {
        let field = |key| {
            item.get(key)
                .filter(|v| truthy(v))
                .map(py_str)
                .unwrap_or_else(|| "-".into())
        };
        let provider = match item.get("provider").and_then(Value::as_str) {
            Some("gcp") => "gcp",
            Some("modal") => "modal",
            _ => "vast",
        };
        let kind = field("gpu_name");
        cells.push(
            [
                id,
                provider.into(),
                if provider == "gcp" {
                    crate::gcp_types::public(kind)
                } else {
                    kind
                },
                field("actual_status").to_ascii_lowercase(),
                table_price(item),
                format_uptime(item.get("duration")),
            ]
            .map(|cell| {
                crate::redact::redact_text(&cell)
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect()
            }),
        );
    }
    use crate::table::Column::{Left, Plain};
    for line in crate::table::aligned_rows(&cells, [Left, Left, Left, Left, Left, Plain]) {
        ctx.println(line.trim_end())?;
    }
    Ok(())
}

pub fn instance_label(item: &Dict) -> Result<String> {
    let dash = Value::str("-");
    let price = item
        .get("dph_total")
        .or_else(|| item.get("dph"))
        .unwrap_or(&dash);
    let price = match py_float(price) {
        Ok(price) => format!("${}/hr", fixed(price, 3)),
        Err(_) => "-".into(),
    };
    let or_dash = |key: &str| match item.get(key) {
        Some(value) if truthy(value) => py_str(value),
        _ => "-".into(),
    };
    Ok(format!(
        "{}  {}  {}  {}  {}  {}",
        py_str(item.get("id").ok_or_else(|| Error::key("id"))?),
        or_dash("gpu_name"),
        or_dash("actual_status"),
        price,
        format_uptime(item.get("duration")),
        repo_name(item).unwrap_or_else(|| "-".into()),
    ))
}

/// The instance picker; `Ok(None)` when there is nothing to select.
pub fn select_instance(ctx: &Ctx, instances: &[Dict], action: &str) -> Result<Option<String>> {
    if instances.is_empty() {
        ctx.println("No instances. Launch one: gpu up")?;
        return Ok(None);
    }
    let mut options = Vec::new();
    for item in instances {
        let id = py_str(item.get("id").ok_or_else(|| Error::key("id"))?);
        let label = instance_label(item)?;
        let label = match item.get("provider") {
            Some(Value::Str(p)) if p == "vast" => format!("Vast.ai  {label}"),
            Some(Value::Str(p)) if p == "gcp" => format!("Google Cloud  {label}"),
            _ => label,
        };
        options.push(Opt::new(id, label));
    }
    // Even a single instance must be explicitly selected.
    let screen = Screen::new(
        format!("Instance to {action}"),
        move || options.clone(),
        Next::Done,
    )
    .empty("No instances");
    pick(ctx, screen).map(Some)
}
