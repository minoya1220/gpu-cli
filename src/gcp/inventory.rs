//! TPU node listing for `gpu ls`, lookups and lifecycle polling.
use jiff::Timestamp;

use super::{
    expiry,
    gcloud::{bad_reply, json_timeout, objects, text},
    geo, http,
    id::{component, TpuId},
    prices,
    settings::Settings,
    types,
};
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::process::Cmd;
use crate::pyjson::{self, Dict, Value};

pub(crate) fn find_node<'a>(nodes: &'a [Dict], name: &str) -> Option<&'a Dict> {
    nodes
        .iter()
        .find(|d| text(d, "name").is_some_and(|n| n.rsplit('/').next() == Some(name)))
}
pub(super) fn is_spot(node: &Dict) -> bool {
    node.get("schedulingConfig")
        .and_then(Value::as_dict)
        .is_some_and(|schedule| {
            ["spot", "preemptible"]
                .iter()
                .any(|key| schedule.get(key) == Some(&Value::Bool(true)))
        })
}

/// Only an authenticated node GET returning a definite 404 proves absence.
/// List failures, malformed responses, auth failures and process errors never do.
pub fn absent(ctx: &Ctx, id: &TpuId) -> Result<bool> {
    ctx.require_gcloud()?;
    let mut args = id.args("describe");
    args.push("--format=json".into());
    match ctx.run(&Cmd::new(&args).timeout(Some(60.0))) {
        Ok(out) if out.code == 0 => {
            let value = pyjson::loads(&out.stdout).map_err(|_| bad_reply())?;
            if value.as_dict().and_then(|d| text(d, "name")) != Some(&id.resource()) {
                return Err(bad_reply());
            }
            Ok(false)
        }
        Ok(out) => {
            let detail = out.stderr.to_ascii_uppercase();
            let denied = detail.contains("PERMISSION_DENIED")
                || detail.contains("UNAUTHENTICATED")
                || detail.contains("FORBIDDEN");
            if !denied
                && (detail.contains("NOT_FOUND:")
                    || detail.contains("(NOT_FOUND)")
                    || detail.contains("HTTPERROR 404")
                    || (out.stderr.contains(&id.resource()) && detail.contains("WAS NOT FOUND")))
            {
                return Ok(true);
            }
            Err(Error::msg("TPU absence unverified. Expiry tasks kept")
                .next("gpu ls --provider gcp"))
        }
        _ => Err(
            Error::msg("Could not verify TPU absence. Deletion tasks kept")
                .next("gpu ls --provider gcp"),
        ),
    }
}

pub fn nodes(ctx: &Ctx, project: &str, zone: &str) -> Result<Vec<Dict>> {
    nodes_timeout(ctx, project, zone, 60.0)
}
pub(super) fn nodes_timeout(
    ctx: &Ctx,
    project: &str,
    zone: &str,
    timeout: f64,
) -> Result<Vec<Dict>> {
    let found = objects(json_timeout(
        ctx,
        vec![
            "gcloud".into(),
            "compute".into(),
            "tpus".into(),
            "tpu-vm".into(),
            "list".into(),
            format!("--project={project}"),
            format!("--zone={zone}"),
            "--quiet".into(),
        ],
        "gpu ls --provider gcp",
        timeout,
    )?)?;
    let prefix = format!("projects/{project}/locations/{zone}/nodes/");
    for node in &found {
        if !text(node, "name")
            .and_then(|name| name.strip_prefix(&prefix))
            .is_some_and(component)
        {
            return Err(bad_reply());
        }
    }
    Ok(found)
}
pub(crate) fn node_id(node: &Dict, project: &str) -> Result<TpuId> {
    TpuId::from_resource(text(node, "name").ok_or_else(bad_reply)?)
        .filter(|id| id.project == project)
        .ok_or_else(bad_reply)
}
/// One authenticated cross-location list, never one process per TPU zone.
pub fn all_nodes(ctx: &Ctx, project: &str) -> Result<Vec<Dict>> {
    ctx.require_gcloud()?;
    if !component(project) {
        return Err(bad_reply());
    }
    let mut nodes = Vec::new();
    let mut next = String::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..30 {
        let url = format!(
            "https://tpu.googleapis.com/v2/projects/{project}/locations/-/nodes?pageSize=1000&pageToken={}",
            http::encode(&next)
        );
        let data = http::get(ctx, project, &url)?;
        if let Some(unreachable) = data.get("unreachable") {
            if !unreachable.as_list().is_some_and(|xs| xs.is_empty()) {
                return Err(
                    Error::msg("Some TPU zones are unreachable. Expiry tasks kept")
                        .next("gpu ls --provider gcp"),
                );
            }
        }
        nodes.extend(objects(
            data.get("nodes")
                .cloned()
                .unwrap_or(Value::List(Vec::new())),
        )?);
        if data
            .iter()
            .any(|(k, _)| !["nodes", "unreachable", "nextPageToken"].contains(&k.as_str()))
        {
            return Err(bad_reply());
        }
        next = match data.get("nextPageToken") {
            None => String::new(),
            Some(Value::Str(s)) => s.clone(),
            _ => return Err(bad_reply()),
        };
        if next.is_empty() {
            for node in &nodes {
                node_id(node, project)?;
            }
            return Ok(nodes);
        }
        if !seen.insert(next.clone()) {
            return Err(bad_reply());
        }
    }
    Err(bad_reply())
}
pub(crate) fn vm_node(node: &Dict) -> bool {
    !text(node, "apiVersion").is_some_and(|v| v.starts_with("V1") || v == "API_VERSION_UNSPECIFIED")
}

pub fn list(ctx: &Ctx, settings: &Settings) -> Result<Vec<Dict>> {
    let queue = expiry::Queue::load(settings)?;
    // Independent reads share one OAuth token and run concurrently. An
    // incomplete node snapshot cannot authorize orphan deletion.
    let (nodes, tasks) = std::thread::scope(|scope| {
        let pending = scope.spawn(|| queue.pending(ctx));
        (
            all_nodes(ctx, &settings.project),
            pending.join().unwrap_or_else(|_| Err(bad_reply())),
        )
    });
    let nodes = nodes?;
    queue.reconcile_inventory(ctx, &nodes, tasks?)?;
    // One local cache read for all regions. Listing must not start price or
    // catalog requests, regardless of the number of instances.
    let prices = prices::cached_project(ctx, &settings.project);
    let now = ctx.clock.now();
    nodes
        .iter()
        .filter(|node| vm_node(node))
        .map(|node| {
            let id = node_id(node, &settings.project)?;
            let mut item = Dict::new();
            item.insert("id", Value::str(id.cli()));
            item.insert("provider", Value::str("gcp"));
            item.insert(
                "gpu_name",
                Value::str(types::public(
                    text(node, "acceleratorType").unwrap_or("TPU").into(),
                )),
            );
            item.insert(
                "actual_status",
                Value::str(text(node, "state").unwrap_or("UNKNOWN")),
            );
            let owner = node
                .get("labels")
                .and_then(|labels| labels.get(crate::gcp::launch::OWNER_LABEL))
                .and_then(Value::as_str);
            item.insert("label", Value::str(crate::owner::labeled("no-repo", owner)));
            if let Some(created) =
                text(node, "createTime").and_then(|s| s.parse::<Timestamp>().ok())
            {
                item.insert("create_time", Value::str(created.to_string()));
                if let Some(seconds) = now
                    .as_second()
                    .checked_sub(created.as_second())
                    .filter(|_| created <= now)
                {
                    item.insert("duration", Value::Int(seconds));
                }
            }
            let spot = is_spot(node);
            if let Some(hourly) = geo::region(&id.zone)
                .and_then(|region| prices.get(region))
                .and_then(|price| price.hourly(text(node, "acceleratorType").unwrap_or(""), spot))
            {
                item.insert("dph_total", Value::Float(hourly));
            }
            Ok(item)
        })
        .collect()
}
