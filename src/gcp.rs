//! Google Cloud TPU VM adapter. All external work crosses Runner; no SDK
//! credential files, account keys, or tokens are copied to a VM.
use jiff::Timestamp;

use crate::cli::UpFlags;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::process::{Cmd, RunError};
use crate::pyjson::{self, Dict, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct TpuId {
    pub project: String,
    pub zone: String,
    pub name: String,
}

pub fn component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl TpuId {
    pub fn parse(id: &str) -> Result<Self> {
        let parts: Vec<_> = id.split(':').collect();
        if parts.len() != 4 || parts[0] != "gcp" || !parts[1..].iter().all(|s| component(s)) {
            return Err(Error::msg(
                "Use a fully qualified TPU ID. Next: gpu ls --provider gcp",
            ));
        }
        Ok(Self {
            project: parts[1].into(),
            zone: parts[2].into(),
            name: parts[3].into(),
        })
    }
    pub fn cli(&self) -> String {
        format!("gcp:{}:{}:{}", self.project, self.zone, self.name)
    }
    pub fn resource(&self) -> String {
        format!(
            "projects/{}/locations/{}/nodes/{}",
            self.project, self.zone, self.name
        )
    }
    pub fn url(&self) -> String {
        format!("https://tpu.googleapis.com/v2/{}", self.resource())
    }
    pub fn args(&self, action: &str) -> Vec<String> {
        vec![
            "gcloud".into(),
            "compute".into(),
            "tpus".into(),
            "tpu-vm".into(),
            action.into(),
            self.name.clone(),
            format!("--project={}", self.project),
            format!("--zone={}", self.zone),
            "--quiet".into(),
        ]
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub project: String,
    pub zone: Option<String>,
    pub runtime: Option<String>,
    pub service_account: Option<String>,
    pub expiry: Option<Dict>,
}

pub fn text<'a>(d: &'a Dict, key: &str) -> Option<&'a str> {
    match d.get(key) {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    }
}
fn optional(d: &Dict, key: &str) -> Result<Option<String>> {
    match d.get(key) {
        None => Ok(None),
        Some(Value::Str(s)) if !s.is_empty() && !s.chars().any(char::is_control) => {
            Ok(Some(s.clone()))
        }
        _ => Err(Error::msg(format!(
            "Set gcp.{key} to a nonempty string in gpu/config.json. Next: gpu up --help"
        ))),
    }
}
impl Settings {
    pub fn load(cfg: &Dict) -> Result<Self> {
        let d = cfg.get("gcp").and_then(Value::as_dict).ok_or_else(|| {
            Error::msg(
                "Configure Google Cloud in gpu/config.json (see README setup). Next: gpu up --help",
            )
        })?;
        let project = optional(d, "project")?.filter(|s| component(s)).ok_or_else(|| Error::msg(
            "Set gcp.project in gpu/config.json; no ambient gcloud project is used. Next: gpu up --help"))?;
        let zone = optional(d, "zone")?;
        if zone.as_ref().is_some_and(|s| !component(s)) {
            return Err(Error::msg("Invalid gcp.zone. Next: gpu up --help"));
        }
        let expiry = match d.get("expiry") {
            None => None,
            Some(Value::Dict(d)) => Some(d.clone()),
            _ => {
                return Err(Error::msg(
                    "Set gcp.expiry to an object in gpu/config.json. Next: gpu up --help",
                ))
            }
        };
        Ok(Self {
            project,
            zone,
            runtime: optional(d, "runtime")?,
            service_account: optional(d, "service_account")?,
            expiry,
        })
    }
}

/// Never relay gcloud stderr: it may contain credentials, URLs or tracebacks.
pub fn run(ctx: &Ctx, args: &[String], next: &str) -> Result<String> {
    match ctx.runner.run(&Cmd::new(args).timeout(Some(600.0))) {
        Ok(out) if out.code == 0 => Ok(out.stdout.trim().into()),
        result => {
            let reason = match result {
                Err(RunError::NotFound) => "Install the Google Cloud CLI",
                Err(RunError::TimedOut) => "Google Cloud timed out; completion is unverified",
                Ok(out) => {
                    let detail = format!("{} {}", out.stdout, out.stderr).to_ascii_lowercase();
                    if detail.contains("quota") {
                        "Google Cloud quota is exhausted"
                    } else if detail.contains("capacity")
                        || detail.contains("resource_exhausted")
                        || detail.contains("no more")
                    {
                        "No TPU capacity in this zone; try another zone or Spot"
                    } else if detail.contains("permission") || detail.contains("forbidden") {
                        "Google Cloud denied access; check project IAM and enabled APIs"
                    } else if detail.contains("login")
                        || detail.contains("credential")
                        || detail.contains("reauth")
                    {
                        return Err(Error::msg(
                            "Sign in to Google Cloud. Next: gcloud auth login",
                        ));
                    } else if detail.contains("not_found") || detail.contains("not found") {
                        "Google Cloud resource was not found"
                    } else {
                        "Google Cloud request failed; completion is unverified"
                    }
                }
                _ => "Could not run Google Cloud CLI",
            };
            Err(Error::msg(format!("{reason}. Next: {next}")))
        }
    }
}
pub fn json(ctx: &Ctx, mut args: Vec<String>, next: &str) -> Result<Value> {
    args.push("--format=json".into());
    pyjson::loads(&run(ctx, &args, next)?).map_err(|_| {
        Error::msg(format!(
            "Google Cloud returned an unreadable reply. Next: {next}"
        ))
    })
}
pub fn objects(value: Value) -> Result<Vec<Dict>> {
    if let Value::List(items) = value {
        return items
            .into_iter()
            .map(|v| match v {
                Value::Dict(d) => Ok(d),
                _ => Err(bad_reply()),
            })
            .collect();
    }
    Err(bad_reply())
}
pub fn bad_reply() -> Error {
    Error::msg("Google Cloud returned an incomplete reply. Next: gpu ls --provider gcp")
}
pub fn describe(ctx: &Ctx, id: &TpuId) -> Result<Dict> {
    match json(ctx, id.args("describe"), "gpu ls --provider gcp")? {
        Value::Dict(d) => Ok(d),
        _ => Err(bad_reply()),
    }
}
pub fn locations(ctx: &Ctx, project: &str) -> Result<Vec<String>> {
    let value = json(
        ctx,
        vec![
            "gcloud".into(),
            "compute".into(),
            "tpus".into(),
            "locations".into(),
            "list".into(),
            format!("--project={project}"),
            "--quiet".into(),
        ],
        "gpu ls --provider gcp",
    )?;
    objects(value)?
        .iter()
        .map(|d| {
            text(d, "locationId")
                .or_else(|| text(d, "name").and_then(|s| s.rsplit('/').next()))
                .filter(|s| component(s))
                .map(String::from)
                .ok_or_else(bad_reply)
        })
        .collect()
}
pub fn nodes(ctx: &Ctx, project: &str, zone: &str) -> Result<Vec<Dict>> {
    let found = objects(json(
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
pub fn list(ctx: &Ctx, settings: &Settings) -> Result<Vec<Dict>> {
    let mut items = Vec::new();
    // Discover every TPU zone, rather than silently omitting --zone overrides.
    for zone in locations(ctx, &settings.project)? {
        for node in nodes(ctx, &settings.project, &zone)? {
            let name = text(&node, "name")
                .and_then(|s| s.rsplit('/').next())
                .filter(|s| component(s))
                .ok_or_else(bad_reply)?;
            let id = TpuId {
                project: settings.project.clone(),
                zone: zone.clone(),
                name: name.into(),
            };
            let mut item = Dict::new();
            item.insert("id", Value::str(id.cli()));
            item.insert("provider", Value::str("gcp"));
            item.insert(
                "gpu_name",
                Value::str(text(&node, "acceleratorType").unwrap_or("TPU")),
            );
            item.insert(
                "actual_status",
                Value::str(text(&node, "state").unwrap_or("UNKNOWN")),
            );
            item.insert("label", Value::str("no-repo"));
            items.push(item);
        }
    }
    Ok(items)
}

pub fn launch(ctx: &Ctx, flags: &UpFlags, cfg: &Dict, deadline: Option<Timestamp>) -> Result<()> {
    if flags.mode.is_some()
        || flags.gpu.is_some()
        || flags.num_gpus != 1
        || flags.max_price.is_some()
        || flags.interruptible
        || flags.offer.is_some()
        || flags.disk.is_some()
        || flags.reliability.is_some()
        || flags.unverified
    {
        return Err(Error::msg(
            "Vast GPU/offer/disk flags do not apply to TPUs. Next: gpu up --provider gcp --help",
        ));
    }
    let settings = Settings::load(cfg)?;
    let deadline = match deadline {
        Some(deadline) => deadline,
        None if ctx.interactive() => {
            let duration: String = crate::picker::pick(
                ctx,
                crate::picker::Screen::new(
                    "Destroy after (e.g. 2h)",
                    Vec::new,
                    crate::picker::Next::Done,
                )
                .text_input(Some(Box::new(|s| {
                    let seconds = crate::expiry::parse_duration(s)?;
                    if !(60..=29 * 86400).contains(&seconds) {
                        return Err("Choose 1 minute to 29 days".into());
                    }
                    Ok(())
                }))),
            )?;
            crate::expiry::parse_deadline_duration(&duration, ctx.clock.now())
                .map_err(Error::msg)?
        }
        None => return Err(Error::msg(
            "TPU launches require a destruction deadline. Next: gpu up --provider gcp --expire 2h",
        )),
    };
    crate::gcp_expiry::validate_deadline(ctx, deadline)?;
    let queue = crate::gcp_expiry::Queue::load(&settings)?;
    let picks = crate::gcp_wizard::choose(ctx, flags, &settings, cfg)?;
    let id = TpuId {
        project: settings.project.clone(),
        zone: picks.zone.clone(),
        name: unique_name(ctx, "gpu")?,
    };
    let mut args = id.args("create");
    args.extend([
        format!("--accelerator-type={}", picks.tpu),
        format!("--version={}", picks.runtime),
        "--labels=gpu-cli=true".into(),
    ]);
    if picks.spot {
        args.push("--spot".into());
    }
    if let Some(sa) = &settings.service_account {
        args.push(format!("--service-account={sa}"));
    }
    let script = startup(&picks.repo, cfg)?;
    // These locally validated scripts/paths contain no '|'. gcloud receives
    // one argv value, never a locally executed shell string.
    args.push(format!(
        "--metadata=^|^startup-script={script}|gpu-expiry-queue={}|gpu-workspace={}",
        queue.resource(),
        picks.directory
    ));
    if flags.dry_run {
        ctx.println("DRY RUN: no TPU or expiry task created")?;
        let requests = [queue.create_args(ctx, &id, deadline)?, args];
        ctx.println(&pyjson::dumps(
            &Value::List(
                requests
                    .into_iter()
                    .map(|args| Value::List(args.into_iter().map(Value::Str).collect()))
                    .collect(),
            ),
            Some(2),
        ))?;
        return Ok(());
    }
    ctx.println(&format!(
        "Launch Google Cloud {} in {} ({})? Paid TPU; price is not quoted. Destroy at {deadline}.",
        picks.tpu,
        picks.zone,
        if picks.spot { "Spot" } else { "on-demand" }
    ))?;
    if !flags.yes && !crate::prompt::confirm(ctx, "Type 'yes' to create this paid TPU: ")? {
        return ctx.println("Cancelled.");
    }
    if deadline.as_second() <= ctx.clock.now().as_second() {
        return Err(Error::msg(
            "Expiry deadline passed; no TPU created. Next: gpu up --provider gcp --expire 2h",
        ));
    }
    queue.check(ctx)?;
    // Arm and read back the control-plane task BEFORE issuing create. A laptop
    // crash, SSH failure, stopped VM or guest failure cannot remove this timer.
    queue.arm(ctx, &id, deadline)?;
    if deadline.as_second() <= ctx.clock.now().as_second() {
        return Err(Error::msg(format!("Expiry deadline passed; no TPU created, but its deletion task may remain. Next: gpu expire {} cancel", id.cli())));
    }
    if let Err(e) = run(ctx, &args, "gpu up --provider gcp --spot --expire 2h") {
        return Err(Error::msg(format!("{}\nCreation of {} is unverified; its deletion task remains armed. Next: gpu ls --provider gcp", e.text().split(". Next:").next().unwrap_or("TPU create failed"), id.cli())));
    }
    ctx.println(&format!(
        "Created TPU {}; deletion requested at {deadline}.\nNext: gpu ssh {} --exec",
        id.cli(),
        id.cli()
    ))?;
    if ctx.interactive() {
        let editor = crate::picker::pick(
            ctx,
            crate::editors::after_up_screen(crate::commands::last_editor(&ctx.env)),
        )?;
        if editor != "none" {
            crate::gcp_ssh::edit(ctx, cfg, &id, Some(&editor), None)?;
        }
    }
    Ok(())
}

pub fn unique_name(ctx: &Ctx, prefix: &str) -> Result<String> {
    // A random suffix from tempfile avoids collisions even across processes and
    // clocks. Never offer user-chosen names: a lingering delete must not hit a replacement.
    let random = tempfile::Builder::new().prefix("").tempfile()?;
    let suffix = random
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_ascii_lowercase();
    Ok(format!("{prefix}-{}-{suffix}", ctx.clock.now().as_second()))
}

fn startup(repo: &crate::repos::Repo, cfg: &Dict) -> Result<String> {
    let owner = crate::pyfmt::py_str(crate::config::get(cfg, "owner")?);
    let (url, name) = crate::repos::resolve_repo(repo.as_arg(), &owner)?;
    let mut script =
        "#!/bin/bash\nset -eu\nmkdir -p /workspace\nchmod 1777 /workspace\n".to_string();
    if !url.is_empty() {
        let dir = format!("/workspace/{name}");
        script.push_str(&format!(
            "if [ ! -e {} ]; then\n  git clone -- {} {}\n  chmod -R a+rwX {}\nfi\n",
            crate::pyfmt::shell_quote(&dir),
            crate::pyfmt::shell_quote(&url),
            crate::pyfmt::shell_quote(&dir),
            crate::pyfmt::shell_quote(&dir)
        ));
    }
    Ok(script)
}

pub fn lifecycle(ctx: &Ctx, id: &TpuId, cfg: &Dict, destroy: bool, yes: bool) -> Result<()> {
    if destroy
        && !yes
        && !crate::prompt::confirm(
            ctx,
            &format!(
                "Destroy TPU {} and its boot disk? Type 'yes' to confirm: ",
                id.cli()
            ),
        )?
    {
        return ctx.println("Cancelled.");
    }
    let listed = nodes(ctx, &id.project, &id.zone)?;
    let node = listed
        .into_iter()
        .find(|d| text(d, "name").is_some_and(|n| n.rsplit('/').next() == Some(&id.name)));
    let Some(node) = node else {
        if destroy {
            if let Ok(queue) = crate::gcp_expiry::queue_for_node(cfg, id, &Dict::new()) {
                queue.cancel(ctx, id)?;
            }
            crate::gcp_ssh::remove(ctx, id)?;
            return ctx.println(&format!("TPU {} is already absent.", id.cli()));
        }
        return Err(Error::msg("TPU was not found. Next: gpu ls --provider gcp"));
    };
    if node.get("queuedResource").is_some_and(crate::pyfmt::truthy) {
        return Err(Error::msg(format!("This TPU belongs to a queued resource; manage its owning request to avoid leaking it. Next: gcloud compute tpus queued-resources list --project={} --zone={}", id.project, id.zone)));
    }
    if !destroy {
        let schedule = node.get("schedulingConfig").and_then(Value::as_dict);
        if schedule.is_some_and(|d| {
            ["spot", "preemptible"]
                .iter()
                .any(|k| d.get(k) == Some(&Value::Bool(true)))
        }) {
            return Err(Error::msg(format!(
                "Spot/preemptible TPUs cannot be stopped and restarted. Next: gpu down {}",
                id.cli()
            )));
        }
        if text(&node, "state") == Some("STOPPED") {
            return ctx.println(&format!(
                "TPU {} is already stopped; any existing expiry is unchanged.",
                id.cli()
            ));
        }
    }
    let action = if destroy { "delete" } else { "stop" };
    run(ctx, &id.args(action), "gpu ls --provider gcp")?;
    let until = ctx.clock.monotonic() + 60.0;
    loop {
        let listed = nodes(ctx, &id.project, &id.zone)?;
        let live = listed.iter().find(|d| {
            text(d, "name").is_some_and(|n| n.rsplit('/').next() == Some(id.name.as_str()))
        });
        if (destroy && live.is_none())
            || (!destroy && live.is_some_and(|d| text(d, "state") == Some("STOPPED")))
        {
            break;
        }
        if ctx.clock.monotonic() >= until {
            return Err(Error::msg(
                "TPU lifecycle completion is unverified. Next: gpu ls --provider gcp",
            ));
        }
        ctx.clock.sleep(2.0);
    }
    if destroy {
        // Do not cancel the safety task until deletion was verified.
        let recorded_queue = node
            .get("metadata")
            .and_then(Value::as_dict)
            .is_some_and(|m| m.get("gpu-expiry-queue").is_some());
        let cleanup_error = || {
            Error::msg(format!("TPU was deleted, but deletion-task cleanup is unverified. Next: gpu expire {} cancel", id.cli()))
        };
        match crate::gcp_expiry::queue_for_node(cfg, id, &node) {
            Ok(queue) => queue.cancel(ctx, id).map_err(|_| cleanup_error())?,
            Err(_) if recorded_queue => return Err(cleanup_error()),
            // Unmanaged nodes may have no gpu expiry config or recorded queue.
            Err(_) => {}
        }
        crate::gcp_ssh::remove(ctx, id)?;
        ctx.println(&format!(
            "Destroyed TPU {} and its boot disk; separately attached storage may still bill.",
            id.cli()
        ))
    } else {
        ctx.println(&format!("Stopped TPU {}; any existing expiry is unchanged; storage charges may continue.\nNext: gpu down {}", id.cli(), id.cli()))
    }
}
