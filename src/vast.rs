//! The authenticated `vastai` CLI: offer search, instance listing, and the
//! exact `create instance` request.

use jiff::Timestamp;

use crate::config::{self, CONTAINER_ENV};
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::offers::{offer_number, price_field};
use crate::process::{run, run_output};
use crate::pyfmt::{float_repr, py_str};
use crate::pyjson::{self, Dict, Value};
use crate::redact::redact_text;
use crate::repos::{resolve_repo, Repo};
use crate::startup::startup_script;

/// The choices a create request depends on.
pub struct RequestArgs<'a> {
    pub repo: &'a Repo,
    pub mode: &'a str,
    pub interruptible: bool,
    pub disk: Option<i64>,
    /// `--expire`: the onstart script starts the boot-time expiry guard.
    pub deadline: Option<Timestamp>,
}

#[derive(Clone)]
pub struct Vast {
    pub ctx: Ctx,
}

impl Vast {
    pub fn new(ctx: &Ctx) -> Vast {
        Vast { ctx: ctx.clone() }
    }

    pub fn json(&self, args: &[&str]) -> Result<Value> {
        let mut argv = vec!["vastai"];
        argv.extend_from_slice(args);
        argv.push("--raw");
        parse_reply(&run(&self.ctx, &argv, None)?)
    }

    pub fn offers(&self, query: &str, bid: bool) -> Result<Value> {
        let order = if bid { "min_bid" } else { "dph" };
        let mut args = vec![
            "search",
            "offers",
            query,
            "--no-default",
            "--limit",
            "1000",
            "--order",
            order,
        ];
        if bid {
            args.push("--interruptible");
        }
        self.json(&args)
    }

    pub fn request(&self, offer: &Dict, args: &RequestArgs, cfg: &Dict) -> Result<Vec<Value>> {
        request(offer, args, cfg)
    }

    /// Send a create request; the reply, or `None` when it is unreadable.
    /// vastai 1.8.2 reports a refused request (HTTP 410 under
    /// `--cancel-unavail`, low credit, bad arguments) by printing
    /// `{"error": true, "status_code": N, "msg": ...}` to stderr and exiting
    /// 0 with no stdout; that becomes a `{"success": false, "msg": ...}` reply.
    pub fn create(&self, request: &[Value]) -> Result<Option<Dict>> {
        let output = run_output(&self.ctx, &argv(request)?, Some(120.0)).map_err(|error| {
            if matches!(error, Error::Timeout(_)) {
                Error::Timeout("Vast create timed out; completion unverified. An instance may be billing. Check: gpu ls before retrying.".into())
            } else { error }
        })?;
        let stdout = output.stdout.trim();
        if !stdout.is_empty() {
            return Ok(match pyjson::loads(stdout) {
                Ok(Value::Dict(reply)) => Some(reply),
                _ => None,
            });
        }
        Ok(output.stderr.lines().rev().find_map(|line| {
            let Ok(Value::Dict(error)) = pyjson::loads(line) else {
                return None;
            };
            (error.get("error") == Some(&Value::Bool(true))).then(|| {
                let msg = error.get("msg").map(py_str).unwrap_or_default();
                Dict::from_iter([
                    ("success".into(), Value::Bool(false)),
                    ("msg".into(), Value::Str(redact_text(&msg))),
                ])
            })
        }))
    }
}

pub fn request(offer: &Dict, args: &RequestArgs, cfg: &Dict) -> Result<Vec<Value>> {
    let mode = args.mode;
    let script = startup_script(args.repo, mode, cfg, args.deadline)?;
    let label = resolve_repo(args.repo.as_arg(), &py_str(config::get(cfg, "owner")?))?.1;
    request_script(offer, args, cfg, script, label)
}

/// Job startup shares the exact create flags, but never clones a repository.
pub fn run_request(
    offer: &Dict,
    disk: Option<i64>,
    cfg: &Dict,
    deadline: Timestamp,
    label: String,
) -> Result<Vec<Value>> {
    let args = RequestArgs {
        repo: &Repo::NoRepo,
        mode: "container",
        interruptible: false,
        disk,
        deadline: Some(deadline),
    };
    request_script(
        offer,
        &args,
        cfg,
        crate::startup::run_script(deadline)?,
        label,
    )
}

fn request_script(
    offer: &Dict,
    args: &RequestArgs,
    cfg: &Dict,
    script: String,
    label: String,
) -> Result<Vec<Value>> {
    let mode = args.mode;
    let none = Value::Null;
    let (key, price) = if args.interruptible {
        ("min_bid", offer.get("min_bid").unwrap_or(&none))
    } else {
        price_field(offer).unwrap_or(("dph_total", &Value::Int(0)))
    };
    let hourly = offer_number(key, price)?;
    let disk = match args.disk {
        Some(disk) if disk != 0 => disk.to_string(),
        _ => py_str(config::get(
            cfg,
            if mode == "vm" { "vm_disk" } else { "disk" },
        )?),
    };
    let mut command: Vec<Value> = vec![
        Value::str("vastai"),
        Value::str("create"),
        Value::str("instance"),
        Value::Str(py_str(offer.get("id").ok_or_else(|| Error::key("id"))?)),
        Value::str("--image"),
        config::get(cfg, &format!("{mode}_image"))?.clone(),
        Value::str("--disk"),
        Value::Str(disk),
        Value::str("--onstart-cmd"),
        Value::Str(script),
        Value::str("--ssh"),
        Value::str("--direct"),
        // Fail when the host cannot start it now, instead of creating a
        // stopped instance that can still bill storage.
        Value::str("--cancel-unavail"),
        Value::str("--label"),
        Value::Str(label),
    ];
    if mode == "container" {
        command.extend([Value::str("--env"), Value::str(CONTAINER_ENV)]);
    }
    if args.interruptible {
        command.extend([Value::str("--bid_price"), Value::Str(float_repr(hourly))]);
    }
    command.push(Value::str("--raw"));
    Ok(command)
}

/// The request as process arguments; a non-string item fails with the
/// `TypeError` text `subprocess.run` raises before starting anything.
pub fn argv(request: &[Value]) -> Result<Vec<String>> {
    request
        .iter()
        .map(|item| match item {
            Value::Str(s) => Ok(s.clone()),
            other => Err(Error::msg(format!(
                "Request argument must be text, got {}. Check image and repository config.",
                other.kind()
            ))),
        })
        .collect()
}

/// `vastai show instances --raw` as instance dicts.
pub fn list_instances(ctx: &Ctx, timeout: Option<f64>) -> Result<Vec<Dict>> {
    let value = parse_reply(&run(
        ctx,
        &["vastai", "show", "instances", "--raw"],
        timeout,
    )?)?;
    let unreadable = || {
        Error::ProviderUnavailable(
            "Vast returned an unreadable instance list. Retry: gpu ls".into(),
        )
    };
    if !matches!(value, Value::List(_)) {
        return Err(unreadable());
    }
    let instances = crate::offers::offer_dicts(value).map_err(|_| unreadable())?;
    if instances.iter().any(|item| {
        item.get("id")
            .is_none_or(|id| !crate::pyfmt::is_digits(&py_str(id)))
    }) {
        return Err(unreadable());
    }
    Ok(instances)
}

/// Decode provider data without exposing raw output or decoder diagnostics.
pub(crate) fn parse_reply(output: &str) -> Result<Value> {
    let value = pyjson::loads(output).map_err(|_| {
        crate::provider_errors::vast(output).unwrap_or_else(|| {
            Error::ProviderUnavailable("Vast returned no readable data; the provider may be temporarily unavailable. Retry: gpu ls".into())
        })
    })?;
    if let Some(dict) = value.as_dict() {
        if dict.get("success") == Some(&Value::Bool(false)) || dict.get("error").is_some() {
            for key in ["error", "msg"] {
                if let Some(detail) = dict.get(key) {
                    if let Some(error) = crate::provider_errors::vast(&py_str(detail)) {
                        return Err(error);
                    }
                }
            }
            return Err(Error::ProviderUnavailable(
                "Vast returned an unsuccessful reply. Retry: gpu ls".into(),
            ));
        }
    }
    Ok(value)
}

/// The listed instance whose `str(id)` equals `instance_id`.
pub fn find_instance<'a>(instances: &'a [Dict], instance_id: &str) -> Option<&'a Dict> {
    instances
        .iter()
        .find(|item| py_str(item.get("id").unwrap_or(&Value::Null)) == instance_id)
}

#[cfg(test)]
mod tests;
