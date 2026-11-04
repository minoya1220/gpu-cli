//! Provider routing. Numeric legacy IDs remain Vast IDs; cloud IDs are fully
//! qualified, so changing config cannot redirect a destructive command.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::picker::{pick, Next, Opt, Screen};
use crate::pyjson::{Dict, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum InstanceId {
    Vast(String),
    Gcp(crate::gcp::TpuId),
}

impl InstanceId {
    pub fn parse(id: &str) -> Result<Self> {
        if id.starts_with("gcp:") {
            return crate::gcp::TpuId::parse(id).map(Self::Gcp);
        }
        // Preserve Vast's legacy ID handling, but never send another provider's
        // qualified identifier to Vast.
        if id.contains(':') {
            return Err(Error::msg("Unknown provider in instance ID. Next: gpu ls"));
        }
        Ok(Self::Vast(id.into()))
    }
}

pub fn choose(ctx: &Ctx, cfg: &Dict, explicit: Option<&str>) -> Result<String> {
    if let Some(provider) = explicit {
        return Ok(provider.into());
    }
    if ctx.interactive() && cfg.get("gcp").is_some() {
        return pick(
            ctx,
            Screen::new(
                "Provider",
                || {
                    vec![
                        Opt::new("vast".to_string(), "Vast.ai — GPU"),
                        Opt::new("gcp".to_string(), "Google Cloud — TPU"),
                    ]
                },
                Next::Done,
            ),
        );
    }
    Ok("vast".into())
}

fn inventory(ctx: &Ctx, cfg: &Dict, only: Option<&str>) -> Result<(Vec<Dict>, Vec<String>)> {
    let mut items = Vec::new();
    let mut failed = Vec::new();
    if only != Some("gcp") {
        match crate::vast::list_instances(ctx, None) {
            Ok(mut vast) => {
                for item in &mut vast {
                    item.insert("provider", Value::str("vast"));
                }
                items.extend(vast);
            }
            Err(err) if only.is_some() => return Err(err),
            Err(_) => failed.push("Vast.ai".to_string()),
        }
    }
    if only == Some("gcp") || (only.is_none() && cfg.get("gcp").is_some()) {
        match crate::gcp::Settings::load(cfg).and_then(|settings| crate::gcp::list(ctx, &settings))
        {
            Ok(gcp) => items.extend(gcp),
            Err(err) if only.is_some() => return Err(err),
            Err(_) => failed.push("Google Cloud".to_string()),
        }
    }
    Ok((items, failed))
}

pub fn selected_id(
    ctx: &Ctx,
    cfg: &Dict,
    id: Option<&str>,
    action: &str,
) -> Result<Option<String>> {
    if id.is_some() || cfg.get("gcp").is_none() {
        return crate::commands::selected_id(ctx, id, action);
    }
    let (items, failed) = inventory(ctx, cfg, None)?;
    if !failed.is_empty() {
        ctx.eprintln(&format!(
            "Could not list {}; showing only reachable providers. Next: gpu ls",
            failed.join(" and ")
        ))?;
        if items.is_empty() {
            return Err(Error::msg("Instance listing is incomplete. Next: gpu ls"));
        }
    }
    crate::instances::select_instance(ctx, &items, action)
}

pub fn ls(ctx: &Ctx, cfg: &Dict, json: bool, only: Option<&str>) -> Result<()> {
    if only != Some("gcp") && (only == Some("vast") || cfg.get("gcp").is_none()) {
        return crate::commands::ls(ctx, json);
    }
    let (items, failed) = inventory(ctx, cfg, only)?;
    if json {
        let value = Value::List(items.iter().cloned().map(Value::Dict).collect());
        ctx.println(&crate::pyjson::dumps(
            &crate::redact::redact(&value),
            Some(1),
        ))?;
    }
    for (provider, label) in [("vast", "Vast.ai GPUs"), ("gcp", "Google Cloud TPUs")] {
        if json {
            break;
        }
        if failed.iter().any(|p| label.starts_with(p)) {
            ctx.println(&format!("{label}: unavailable (not an empty list)"))?;
            continue;
        }
        if only.is_some_and(|p| p != provider) {
            continue;
        }
        ctx.println(label)?;
        let group = items
            .iter()
            .filter(|i| i.get("provider") == Some(&Value::str(provider)))
            .cloned()
            .map(Value::Dict)
            .collect();
        crate::commands::render_instances_with_accelerator(
            ctx,
            &Value::List(group),
            if provider == "gcp" { "TPU" } else { "GPU" },
        )?;
    }
    if !failed.is_empty() {
        let provider = if failed[0] == "Vast.ai" {
            "vast"
        } else {
            "gcp"
        };
        return Err(Error::msg(format!(
            "Instance listing is incomplete: {} unavailable. Next: gpu ls --provider {provider}",
            failed.join(" and ")
        )));
    }
    Ok(())
}
