//! The Vast backend for `gpu run`.
use jiff::Timestamp;

use crate::cli::RunFlags;
use crate::ctx::Ctx;
use crate::error::{Error, ExitCode, Result};
use crate::process::Cmd;
use crate::provider::Provider;
use crate::pyjson::{Dict, Value};
use crate::run::{RunBackend, RunPlan, DOWNLOAD_RESERVE};
use crate::vast::offers::offer_price;
use crate::vast::{self, Vast};

fn ssh(alias: &str, command: &str, timeout: f64) -> Cmd {
    crate::ssh::command(&["ssh", "-T"], true, alias, command)
        .timeout(Some(timeout.max(0.1)))
        .raw_output()
}

/// `gpu run` on a Vast offer: create, wait for SSH, arm the instance's
/// own expiry, then supervise over the managed SSH alias.
pub(crate) struct VastBackend {
    alias: String,
    offer: Option<Dict>,
}

impl VastBackend {
    pub(crate) fn new(offer: Dict) -> Self {
        Self {
            alias: String::new(),
            offer: Some(offer),
        }
    }

    /// A backend whose instance already answers on `alias`.
    #[cfg(test)]
    pub(crate) fn connected(alias: &str) -> Self {
        Self {
            alias: alias.into(),
            offer: None,
        }
    }
}

impl RunBackend for VastBackend {
    fn provider(&self) -> Provider {
        Provider::Vast
    }

    fn plan(
        &self,
        _ctx: &Ctx,
        flags: &RunFlags,
        cfg: &Dict,
        deadline: Timestamp,
        label: String,
    ) -> Result<RunPlan> {
        let offer = self
            .offer
            .clone()
            .ok_or_else(|| Error::msg("No Vast offer selected."))?;
        let rate = offer_price(&offer)?;
        if !rate.is_finite() || rate < 0.0 {
            return Err(Error::msg(
                "Offer has no finite nonnegative hourly price; no instance created.",
            ));
        }
        let request = vast::run_request(&offer, flags.disk, cfg, deadline, label)
            .and_then(|request| vast::argv(&request).map(|_| Value::List(request)));
        Ok(RunPlan {
            gpu: offer.get("gpu_name").cloned().unwrap_or(Value::Null),
            rate,
            request,
        })
    }

    fn launch(
        &mut self,
        ctx: &Ctx,
        request: &Value,
        deadline: Timestamp,
        timeout: f64,
        created: &mut dyn FnMut(&str, Option<&str>) -> Result<()>,
    ) -> Result<String> {
        ctx.set_interrupt_notice(Some(format!(
            "Vast may have created an instance; boot expiry is {deadline}. Check: gpu ls"
        )));
        ctx.result("created", Value::Null);
        let request = request
            .as_list()
            .ok_or_else(|| Error::msg("Invalid Vast run request.").rejected())?;
        let reply = Vast::new(ctx).create(request).inspect_err(|_| {
            if let Some(notice) = ctx.interrupt_notice() {
                let _ = ctx.eprintln(&notice);
            }
        })?;
        vast::created_summary(reply.as_ref()).inspect_err(|_| {
            ctx.result("created", Value::Bool(false));
            ctx.set_interrupt_notice(None);
        })?;
        let id = reply
            .as_ref()
            .and_then(vast::created_id)
            .ok_or_else(|| {
                Error::status(
                    ExitCode::CleanupUnverified,
                    "Create response has no instance ID; cleanup cannot be verified. Check: gpu ls, then gpu down ID --yes",
                )
            })?;
        ctx.result("id", Value::str(&id));
        ctx.result("created", Value::Bool(true));
        ctx.set_interrupt_notice(Some(format!("Instance {id} may still be running; it self-destroys at {deadline}. Check: gpu ls, gpu down {id} --yes")));
        created(&id, None)?;
        ctx.check_cancelled()?;
        let remaining =
            (deadline.as_second() - DOWNLOAD_RESERVE - ctx.clock.now().as_second()) as f64;
        let wait_timeout = timeout.min(remaining.max(0.0));
        let cap = if remaining < timeout {
            let message = format!("SSH wait limited to {}s by --expire (requested --timeout {}s; 10m reserved for results). Use a longer \
                --expire to allow more boot time.", crate::pyfmt::general(wait_timeout), crate::pyfmt::general(timeout));
            ctx.eprintln(&message)?;
            message + " "
        } else {
            String::new()
        };
        let hint = format!(
            "{cap}Next: retry gpu run with a longer --expire/--timeout or another --offer."
        );
        let (alias, _) =
            crate::vast::instance::wait_for_ssh_with_hint(ctx, &id, wait_timeout, &hint)?;
        crate::vast::expiry::manage_expiry(ctx, &alias, &id, "arm", Some(deadline))?;
        self.alias = alias;
        ctx.result("expiry_armed", Value::Bool(true));
        Ok(id)
    }

    fn remote(&self, command: &str, timeout: f64) -> Cmd {
        ssh(&self.alias, command, timeout)
    }

    fn lost(&self, ctx: &Ctx, id: &str) -> bool {
        let live = vast::list_instances(ctx, Some(15.0)).ok();
        live.as_ref().is_some_and(|live| {
            vast::find_instance(live, id).is_none_or(|i| {
                matches!(
                    i.get("actual_status").and_then(Value::as_str),
                    Some("stopped" | "exited" | "destroyed")
                )
            })
        })
    }

    fn restore_expiry(&self, ctx: &Ctx, id: &str, deadline: Timestamp) -> Result<()> {
        crate::vast::expiry::manage_expiry(ctx, &self.alias, id, "arm", Some(deadline)).map(|_| ())
    }

    fn hook(&self, id: &str) -> String {
        format!("#!/bin/bash\nroot=\"${{HOME:-/root}}/.local/state/gpu-expire\"\nGPU_EXPIRE_SOURCE=$(cat \
            \"$root/runner.sh\")\nexport GPU_EXPIRE_SOURCE\nexec bash \"$root/runner.sh\" arm {id} \"$1\"\n")
    }

    fn wait_for_startup(&self) -> bool {
        true
    }

    fn destroy(&self, ctx: &Ctx, id: &str) -> Result<()> {
        crate::vast::instance::destroy_created(ctx, id)
    }
}
