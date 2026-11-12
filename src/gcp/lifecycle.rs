//! `gpu down` and `gpu stop` for TPUs, verified by polling the node list.
use super::{
    expiry,
    gcloud::{run, text},
    id::TpuId,
    inventory::{find_node, is_spot, nodes, nodes_timeout},
    ssh,
};
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::pyjson::{Dict, Value};

/// Clean up local records of a TPU gpu recorded but that is already gone.
fn forget_absent(ctx: &Ctx, id: &TpuId, cfg: &Dict) -> Result<()> {
    let queue = expiry::queue_for_node(cfg, id, &Dict::new()).ok();
    let tasks = match &queue {
        Some(queue) => queue.has_tasks(ctx, id)?,
        None => false,
    };
    if !tasks && !ssh::has(ctx, id)? {
        return Err(Error::not_found(&id.cli()));
    }
    if let (Some(queue), true) = (&queue, tasks) {
        queue.cancel(ctx, id)?;
    }
    ssh::remove(ctx, id)?;
    ctx.result("state", Value::str("already_absent"));
    ctx.println(&format!("TPU {} is already absent.", id.cli()))
}

pub fn lifecycle(ctx: &Ctx, id: &TpuId, cfg: &Dict, destroy: bool, yes: bool) -> Result<()> {
    let Some(node) = find_node(&nodes(ctx, &id.project, &id.zone)?, &id.name).cloned() else {
        return if destroy {
            forget_absent(ctx, id, cfg)
        } else {
            Err(Error::not_found(&id.cli()))
        };
    };
    if destroy
        && !yes
        && !crate::prompt::confirm(
            ctx,
            Some(&format!("Destroy TPU {} and its boot disk?", id.cli())),
        )?
    {
        return ctx.println("Cancelled.");
    }
    let wait = ctx.waiting(if destroy {
        "Waiting for Google Cloud to delete the TPU"
    } else {
        "Waiting for Google Cloud to stop the TPU"
    })?;
    if destroy {
        expiry::reconcile(ctx, cfg)?;
    }
    if node.get("queuedResource").is_some_and(crate::pyfmt::truthy) {
        return Err(Error::msg(
            "Queued-resource TPU: manage its owning request, not just the node",
        )
        .next(format!(
            "gcloud compute tpus queued-resources list --project={} --zone={}",
            id.project, id.zone
        )));
    }
    if !destroy {
        if is_spot(&node) {
            return Err(
                Error::msg("Spot/preemptible TPUs cannot be stopped and restarted")
                    .next(format!("gpu down {}", id.cli())),
            );
        }
        if text(&node, "state") == Some("STOPPED") {
            drop(wait);
            ctx.result("state", Value::str("stopped"));
            return ctx.println(&format!(
                "TPU {} is already stopped. Expiry unchanged.",
                id.cli()
            ));
        }
    }
    let action = if destroy { "delete" } else { "stop" };
    run(ctx, &id.args(action), "gpu ls --provider gcp")?;
    let poll = ctx.polling(60.0, 2.0);
    loop {
        if poll.expired() {
            return Err(
                Error::msg("TPU lifecycle completion is unverified").next("gpu ls --provider gcp")
            );
        }
        let listed = match nodes_timeout(ctx, &id.project, &id.zone, (poll.remaining()).min(15.0)) {
            Ok(listed) => listed,
            Err(error) if error.retryable() => {
                poll.pause();
                continue;
            }
            Err(error) => return Err(error),
        };
        let live = find_node(&listed, &id.name);
        if (destroy && live.is_none())
            || (!destroy && live.is_some_and(|d| text(d, "state") == Some("STOPPED")))
        {
            break;
        }
        if poll.expired() {
            return Err(
                Error::msg("TPU lifecycle completion is unverified").next("gpu ls --provider gcp")
            );
        }
        poll.pause();
    }
    if destroy {
        // Do not cancel the safety task until deletion was verified.
        let recorded_queue = node
            .get("metadata")
            .and_then(Value::as_dict)
            .is_some_and(|m| m.get("gpu-expiry-queue").is_some());
        let cleanup_error = || {
            Error::msg("TPU was deleted, but deletion-task cleanup is unverified")
                .next(format!("gpu expire {} cancel", id.cli()))
        };
        match expiry::queue_for_node(cfg, id, &node) {
            Ok(queue) => queue.cancel(ctx, id).map_err(|_| cleanup_error())?,
            Err(_) if recorded_queue => return Err(cleanup_error()),
            // Unmanaged nodes may have no gpu expiry config or recorded queue.
            Err(_) => {}
        }
        ssh::remove(ctx, id)?;
        drop(wait);
        ctx.result("state", Value::str("destroyed"));
        ctx.println(&format!("Destroyed TPU {} and its boot disk.", id.cli()))
    } else {
        drop(wait);
        ctx.result("state", Value::str("stopped"));
        ctx.println(&format!(
            "Stopped {}. Expiry unchanged. Storage still bills.\nNext: gpu down {}",
            id.cli(),
            id.cli()
        ))
    }
}
