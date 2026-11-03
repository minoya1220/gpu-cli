//! The authenticated `vastai` CLI: offer search, instance listing, and the
//! exact `create instance` request.

use crate::config::{self, CONTAINER_ENV};
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::process::run;
use crate::pyfmt::{float_repr, py_float, py_str, type_name};
use crate::pyjson::{self, Dict, Value};
use crate::repos::{resolve_repo, Repo};
use crate::startup::startup_script;

/// The choices a create request depends on.
pub struct RequestArgs<'a> {
    pub repo: &'a Repo,
    pub mode: &'a str,
    pub interruptible: bool,
    pub disk: Option<i64>,
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
        Ok(pyjson::loads(&run(&self.ctx, &argv, None)?)?)
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
}

/// The create request as Python builds it: every item is a string except the
/// image, which is the raw config value.
pub fn request(offer: &Dict, args: &RequestArgs, cfg: &Dict) -> Result<Vec<Value>> {
    let mode = args.mode;
    let script = startup_script(args.repo, mode, cfg)?;
    let none = Value::Null;
    let price = if args.interruptible {
        offer.get("min_bid").unwrap_or(&none)
    } else {
        offer
            .get("dph_total")
            .or_else(|| offer.get("dph"))
            .unwrap_or(&Value::Int(0))
    };
    let hourly = py_float(price)?;
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
        Value::str("--label"),
        Value::Str(resolve_repo(args.repo.as_arg(), &py_str(config::get(cfg, "owner")?))?.1),
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
                type_name(other)
            ))),
        })
        .collect()
}

/// `vastai show instances --raw` as instance dicts.
pub fn list_instances(ctx: &Ctx, timeout: Option<f64>) -> Result<Vec<Dict>> {
    let value = pyjson::loads(&run(
        ctx,
        &["vastai", "show", "instances", "--raw"],
        timeout,
    )?)?;
    crate::offers::offer_dicts(value)
}

/// The listed instance whose `str(id)` equals `instance_id`.
pub fn find_instance<'a>(instances: &'a [Dict], instance_id: &str) -> Option<&'a Dict> {
    instances
        .iter()
        .find(|item| py_str(item.get("id").unwrap_or(&Value::Null)) == instance_id)
}
