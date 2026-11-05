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
        let d = cfg
            .get("gcp")
            .and_then(Value::as_dict)
            .ok_or_else(|| Error::msg(crate::setup::GCP_NOT_SETUP))?;
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
            service_account: optional(d, "service_account")?,
            expiry,
        })
    }
}

/// Never relay gcloud stderr: it may contain credentials, URLs or tracebacks.
pub fn run(ctx: &Ctx, args: &[String], next: &str) -> Result<String> {
    match ctx.run(&Cmd::new(args).timeout(Some(600.0))) {
        Err(RunError::Denied(message)) => Err(Error::msg(message)),
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
            Err(Error::msg(
                "TPU absence unverified; expiry tasks kept. Next: gpu ls --provider gcp",
            ))
        }
        _ => Err(Error::msg(
            "Could not verify TPU absence; deletion tasks were kept. Next: gpu ls --provider gcp",
        )),
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
pub(crate) fn node_id(node: &Dict, project: &str) -> Result<TpuId> {
    let parts: Vec<_> = text(node, "name")
        .ok_or_else(bad_reply)?
        .split('/')
        .collect();
    if parts.len() != 6
        || parts[0] != "projects"
        || parts[1] != project
        || parts[2] != "locations"
        || parts[4] != "nodes"
        || ![parts[1], parts[3], parts[5]].iter().all(|s| component(s))
    {
        return Err(bad_reply());
    }
    Ok(TpuId {
        project: project.into(),
        zone: parts[3].into(),
        name: parts[5].into(),
    })
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
        let data=crate::gcp_http::get(ctx,project,&format!("https://tpu.googleapis.com/v2/projects/{project}/locations/-/nodes?pageSize=1000&pageToken={}",crate::gcp_http::encode(&next)))?;
        if let Some(unreachable) = data.get("unreachable") {
            if !unreachable.as_list().is_some_and(|xs| xs.is_empty()) {
                return Err(Error::msg("Some TPU zones are unreachable; expiry tasks kept. Next: gpu ls --provider gcp"));
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
pub fn list(ctx: &Ctx, settings: &Settings) -> Result<Vec<Dict>> {
    let queue = crate::gcp_expiry::Queue::load(settings)?;
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
    nodes
        .iter()
        .filter(|n| {
            !text(n, "apiVersion")
                .is_some_and(|v| v.starts_with("V1") || v == "API_VERSION_UNSPECIFIED")
        })
        .map(|node| {
            let id = node_id(node, &settings.project)?;
            let mut item = Dict::new();
            item.insert("id", Value::str(id.cli()));
            item.insert("provider", Value::str("gcp"));
            item.insert(
                "gpu_name",
                Value::str(text(node, "acceleratorType").unwrap_or("TPU")),
            );
            item.insert(
                "actual_status",
                Value::str(text(node, "state").unwrap_or("UNKNOWN")),
            );
            item.insert("label", Value::str("no-repo"));
            Ok(item)
        })
        .collect()
}

/// Validate launch configuration locally, before leaving the hardware picker.
/// This does not call gcloud or verify remote IAM/queue readiness.
pub fn launch_setup(ctx: &Ctx, cfg: &Dict) -> Result<(Settings, crate::gcp_expiry::Queue)> {
    let load = || {
        ctx.require_gcloud()?;
        let settings = Settings::load(cfg)?;
        let queue = crate::gcp_expiry::Queue::load(&settings)?;
        Ok((settings, queue))
    };
    load().map_err(|error: Error| {
        Error::msg(format!(
            "{}\nConfig: {}",
            error.text(),
            crate::config::config_path(&ctx.env).display()
        ))
    })
}

/// Carry the normal wizard's repository and TPU choice into the cloud flow.
/// Only the mode chosen *in the UI* is dropped: explicit Vast-only flags remain
/// present so the TPU adapter can reject them rather than silently ignore them.
pub fn launch_from_picker(
    ctx: &Ctx,
    args: &crate::launch::UpArgs,
    picks: &crate::wizard::Picks,
    cfg: &Dict,
    deadline: Option<Timestamp>,
) -> Result<()> {
    let repo = picks.repo.clone().unwrap_or(crate::repos::Repo::NoRepo);
    let tpu = match &picks.tpu {
        Some(crate::wizard::TpuChoice::Type(tpu)) => Some(tpu.clone()),
        _ => None,
    };
    let flags = UpFlags {
        provider: Some("gcp".into()),
        tpu,
        zone: None,
        runtime: None,
        spot: false,
        on_demand: false,
        repo: if repo == crate::repos::Repo::NoRepo {
            None
        } else {
            Some(repo.as_arg().into())
        },
        no_repo: repo == crate::repos::Repo::NoRepo,
        mode: args.mode.clone(),
        gpu: args.gpu.clone(),
        num_gpus: args.num_gpus,
        max_price: args.max_price,
        interruptible: args.interruptible,
        offer: args.offer,
        disk: args.disk,
        yes: args.yes,
        dry_run: args.dry_run,
        expire: args.expire.clone(),
        reliability: args.reliability,
        unverified: args.unverified,
    };
    launch(ctx, &flags, cfg, deadline)
}

pub fn launch(ctx: &Ctx, flags: &UpFlags, cfg: &Dict, deadline: Option<Timestamp>) -> Result<()> {
    let (settings, queue) = launch_setup(ctx, cfg)?;
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
    let picks = crate::gcp_wizard::choose(ctx, flags, &settings, cfg, deadline)?;
    let deadline = picks.deadline;
    crate::gcp_expiry::validate_deadline(ctx, deadline)?;
    if !flags.dry_run {
        queue.reconcile(ctx)?;
        crate::gcp_catalog::verify(ctx, &settings.project, &picks.zone, &picks.tpu)?;
    }
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
    let price =
        match crate::gcp_prices::load_for(ctx, cfg, &picks.zone, Some((&picks.tpu, picks.spot))) {
            Ok(prices) => prices.quote(&picks.tpu, picks.spot),
            Err(e) => format!(
                "price unavailable ({})",
                e.text().split(". Next:").next().unwrap_or("read failed")
            ),
        };
    let local = deadline
        .to_zoned(crate::expiry::local_zone(&ctx.env))
        .strftime("%b %d %H:%M %Z")
        .to_string();
    ctx.println(&format!("Launch {} {} in {}, {price}, delete at {local}.\nPaid; USD list. Storage/network/tax extra; discounts excluded.",picks.tpu,if picks.spot {"Spot"} else {"on-demand"},picks.zone))?;
    if !flags.yes && !crate::prompt::confirm(ctx, "Type yes: ")? {
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
        // A known request rejection plus an authenticated 404 is terminal.
        // Keep the timer after ambiguous errors/timeouts (create may be in flight).
        let rejected = [
            "No TPU capacity",
            "Google Cloud resource was not found",
            "Google Cloud quota",
            "Google Cloud denied access",
        ]
        .iter()
        .any(|s| e.text().starts_with(s));
        if rejected && absent(ctx, &id).unwrap_or(false) && queue.cancel(ctx, &id).is_ok() {
            return Err(Error::msg(format!("Create {} rejected: {}. No node exists; deletion task removed. Next: gpu up --tpu {} --zone {} --spot --no-repo --expire 2h", id.cli(), e.text().split(". Next:").next().unwrap_or("request rejected"), picks.tpu, picks.zone)));
        }
        return Err(Error::msg(format!(
            "{}\nCreate {} unverified; deletion task remains armed. Next: gpu ls --provider gcp",
            e.text()
                .split(". Next:")
                .next()
                .unwrap_or("TPU create failed"),
            id.cli()
        )));
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
    if destroy {
        crate::gcp_expiry::reconcile(ctx, cfg)?;
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
        return Err(Error::msg(format!("Queued-resource TPU: manage its owning request, not just the node. Next: gcloud compute tpus queued-resources list --project={} --zone={}", id.project, id.zone)));
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
        ctx.println(&format!(
            "Stopped {}; expiry unchanged; storage may still bill.\nNext: gpu down {}",
            id.cli(),
            id.cli()
        ))
    }
}
