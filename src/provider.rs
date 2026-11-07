//! Provider routing. Numeric IDs remain Vast IDs. Full cloud IDs carry their
//! scope; short TPU names require a fresh, unambiguous configured-project lookup.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
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

type Inventory = (Vec<Dict>, Vec<(String, Error)>);

fn inventory(ctx: &Ctx, cfg: &Dict, only: Option<&str>) -> Result<Inventory> {
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
            Err(error) => failed.push(("Vast.ai".to_string(), error)),
        }
    }
    if only == Some("gcp") || (only.is_none() && crate::setup::gcp_ready(cfg)) {
        ctx.require_gcloud()?;
        match crate::gcp::Settings::load(cfg).and_then(|settings| crate::gcp::list(ctx, &settings))
        {
            Ok(gcp) => items.extend(gcp),
            Err(err) if only.is_some() => return Err(err),
            Err(error) => failed.push(("Google Cloud".to_string(), error)),
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
    if let Some(id) = id {
        if !short_candidate(id) {
            ctx.result("id", Value::str(id));
            return Ok(Some(id.into()));
        }
        ctx.require_gcloud()?;
        let settings = crate::gcp::Settings::load(cfg)?;
        let nodes = crate::gcp::all_nodes(ctx, &settings.project)?;
        let matched = nodes
            .iter()
            .filter(|node| crate::gcp::vm_node(node))
            .map(|node| crate::gcp::node_id(node, &settings.project))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|tpu| tpu.matches_short(id))
            .collect::<Vec<_>>();
        return match matched.as_slice() {
            [tpu] => {
                ctx.result("id", Value::str(tpu.cli()));
                Ok(Some(tpu.cli()))
            }
            [] => Err(Error::msg(format!(
                "TPU {id} was not found in {}. Next: gpu ls --provider gcp",
                settings.project
            ))),
            _ => Err(Error::msg(format!(
                "TPU ID {id} is ambiguous. Use a full ID. Next: gpu ls --provider gcp --long"
            ))),
        };
    }
    if !ctx.interactive() {
        return Err(crate::output::usage(format!(
            "Missing instance ID. Next: gpu {action} ID"
        )));
    }
    if !crate::setup::gcp_ready(cfg) {
        return crate::commands::selected_id(ctx, None, action);
    }
    let (items, failed) = inventory(ctx, cfg, None)?;
    if !failed.is_empty() {
        ctx.eprintln(&format!(
            "Could not list {}; showing only reachable providers. Next: gpu ls",
            failed
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(" and ")
        ))?;
        if items.is_empty() {
            return Err(Error::msg("Instance listing is incomplete. Next: gpu ls"));
        }
    }
    crate::instances::select_instance(ctx, &items, action)
}

pub(crate) fn short_candidate(id: &str) -> bool {
    crate::gcp::component(id) && id.as_bytes()[0].is_ascii_alphabetic()
}

pub fn ls(ctx: &Ctx, cfg: &Dict, json: bool, long: bool, only: Option<&str>) -> Result<()> {
    if only != Some("gcp") && (only == Some("vast") || !crate::setup::gcp_ready(cfg)) {
        return crate::commands::ls(ctx, json);
    }
    let (items, failed) = inventory(ctx, cfg, only)?;
    if json {
        let value = Value::List(items.iter().cloned().map(Value::Dict).collect());
        if ctx.json() {
            ctx.result("instances", crate::redact::redact(&value));
        } else {
            ctx.println(&crate::pyjson::dumps(
                &crate::redact::redact(&value),
                Some(1),
            ))?;
        }
    }
    if !json && (!items.is_empty() || failed.is_empty()) {
        crate::instances::render_table(ctx, &items, long)?;
    }
    if !failed.is_empty() {
        let provider = if failed[0].0 == "Vast.ai" {
            "vast"
        } else {
            "gcp"
        };
        let message =
            format!(
            "Instance listing is incomplete: {} unavailable. {} Next: gpu ls --provider {provider}",
            failed.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(" and "),
            failed[0].1.text(),
        );
        return Err(match &failed[0].1 {
            Error::Status(code, _) => Error::status(*code, message),
            _ => Error::msg(message),
        });
    }
    Ok(())
}
