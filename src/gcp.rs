//! Google Cloud TPU VM adapter. All external work crosses Runner; no SDK
//! credential files, account keys, or tokens are copied to a VM.
use jiff::Timestamp;

use crate::cli::UpFlags;
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::process::Cmd;
use crate::pyjson::{self, Dict, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct TpuId {
    pub project: String,
    pub zone: String,
    pub name: String,
}

pub(crate) fn resource_parts<'a>(
    resource: &'a str,
    kind: &str,
) -> Option<(&'a str, &'a str, &'a str)> {
    let parts = resource.split('/').collect::<Vec<_>>();
    if parts.len() != 6
        || parts[0] != "projects"
        || parts[2] != "locations"
        || parts[4] != kind
        || ![parts[1], parts[3], parts[5]].into_iter().all(component)
    {
        return None;
    }
    Some((parts[1], parts[3], parts[5]))
}
pub(crate) fn find_node<'a>(nodes: &'a [Dict], name: &str) -> Option<&'a Dict> {
    nodes
        .iter()
        .find(|d| text(d, "name").is_some_and(|n| n.rsplit('/').next() == Some(name)))
}
fn is_spot(node: &Dict) -> bool {
    node.get("schedulingConfig")
        .and_then(Value::as_dict)
        .is_some_and(|schedule| {
            ["spot", "preemptible"]
                .iter()
                .any(|key| schedule.get(key) == Some(&Value::Bool(true)))
        })
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
            return Err(Error::msg("Use a fully qualified TPU ID").next("gpu ls --provider gcp"));
        }
        Ok(Self {
            project: parts[1].into(),
            zone: parts[2].into(),
            name: parts[3].into(),
        })
    }
    pub(crate) fn from_resource(resource: &str) -> Option<Self> {
        let (project, zone, name) = resource_parts(resource, "nodes")?;
        Some(Self {
            project: project.into(),
            zone: zone.into(),
            name: name.into(),
        })
    }
    pub fn cli(&self) -> String {
        format!("gcp:{}:{}:{}", self.project, self.zone, self.name)
    }
    /// gpu up names include a timestamp; the random suffix is enough for a
    /// compact candidate. Inventory must still prove that candidate is unique.
    pub(crate) fn short_name(&self) -> String {
        if let Some((epoch, suffix)) = self
            .name
            .strip_prefix("gpu-")
            .and_then(|s| s.split_once('-'))
        {
            if !epoch.is_empty() && epoch.bytes().all(|b| b.is_ascii_digit()) && component(suffix) {
                return format!("gpu-{suffix}");
            }
        }
        self.name.clone()
    }
    pub(crate) fn matches_short(&self, value: &str) -> bool {
        value == self.name || value == self.short_name()
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
            "Set gcp.{key} to a nonempty string in gpu/config.json"
        ))
        .next("gpu up --help")),
    }
}
impl Settings {
    pub fn load(cfg: &Dict) -> Result<Self> {
        let d = cfg
            .get("gcp")
            .and_then(Value::as_dict)
            .ok_or_else(|| crate::setup::gcp_not_setup(None))?;
        let project = optional(d, "project")?
            .filter(|s| component(s))
            .ok_or_else(|| Error::msg("Set gcp.project in gpu/config.json").next("gpu setup"))?;
        let zone = optional(d, "zone")?;
        if zone.as_ref().is_some_and(|s| !component(s)) {
            return Err(Error::msg("Invalid gcp.zone").next("gpu up --help"));
        }
        let expiry = match d.get("expiry") {
            None => None,
            Some(Value::Dict(d)) => Some(d.clone()),
            _ => {
                return Err(Error::msg("Set gcp.expiry to an object in gpu/config.json")
                    .next("gpu up --help"))
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
    run_timeout(ctx, args, next, 600.0)
}
pub(crate) fn run_timeout(ctx: &Ctx, args: &[String], next: &str, timeout: f64) -> Result<String> {
    crate::provider_errors::gcp(ctx.run(&Cmd::new(args).timeout(Some(timeout))), next)
}
pub fn json(ctx: &Ctx, args: Vec<String>, next: &str) -> Result<Value> {
    json_timeout(ctx, args, next, 600.0)
}
pub(crate) fn json_timeout(
    ctx: &Ctx,
    mut args: Vec<String>,
    next: &str,
    timeout: f64,
) -> Result<Value> {
    args.push("--format=json".into());
    pyjson::loads(&run_timeout(ctx, &args, next, timeout)?).map_err(|_| {
        Error::unavailable(format!(
            "Google Cloud returned an unreadable reply. Retry: {next}"
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
    Error::unavailable("Google Cloud returned an incomplete reply. Retry: gpu ls --provider gcp")
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
fn nodes_timeout(ctx: &Ctx, project: &str, zone: &str, timeout: f64) -> Result<Vec<Dict>> {
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
        let data=crate::gcp_http::get(ctx,project,&format!("https://tpu.googleapis.com/v2/projects/{project}/locations/-/nodes?pageSize=1000&pageToken={}",crate::gcp_http::encode(&next)))?;
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
    // One local cache read for all regions. Listing must not start price or
    // catalog requests, regardless of the number of instances.
    let prices = crate::gcp_prices::cached_project(ctx, &settings.project);
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
                Value::str(crate::gcp_types::public(
                    text(node, "acceleratorType").unwrap_or("TPU").into(),
                )),
            );
            item.insert(
                "actual_status",
                Value::str(text(node, "state").unwrap_or("UNKNOWN")),
            );
            item.insert("label", Value::str("no-repo"));
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
            if let Some(hourly) = crate::gcp_geo::region(&id.zone)
                .and_then(|region| prices.get(region))
                .and_then(|price| price.hourly(text(node, "acceleratorType").unwrap_or(""), spot))
            {
                item.insert("dph_total", Value::Float(hourly));
            }
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
        let message = format!(
            "{}\nConfig: {}",
            error.text(),
            crate::config::config_path(&ctx.env).display()
        );
        Error::status(crate::error::ExitCode::NotSetup, message)
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
    let flags = picker_flags(args, picks);
    let deadline = picks.cloud.as_ref().and_then(|s| s.expiry_at).or(deadline);
    launch(ctx, &flags, cfg, deadline)
}
pub(crate) fn picker_flags(args: &crate::launch::UpArgs, picks: &crate::wizard::Picks) -> UpFlags {
    let repo = picks.repo.clone().unwrap_or(crate::repos::Repo::NoRepo);
    let tpu = match &picks.tpu {
        Some(crate::wizard::TpuChoice::Type(tpu)) => Some(tpu.clone()),
        _ => None,
    };
    UpFlags {
        provider: Some("gcp".into()),
        tpu: picks.cloud.as_ref().and_then(|s| s.tpu.clone()).or(tpu),
        zone: picks.cloud.as_ref().and_then(|s| s.zone.clone()),
        runtime: None,
        spot: picks.cloud.as_ref().and_then(|s| s.spot).unwrap_or(false),
        on_demand: picks
            .cloud
            .as_ref()
            .and_then(|s| s.spot)
            .is_some_and(|spot| !spot),
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
        wait: false,
        wait_setup: false,
        timeout: 120.0,
    }
}

pub(crate) fn validate_tpu_flags(flags: &UpFlags) -> Result<()> {
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
        return Err(Error::msg("Vast GPU/offer/disk flags do not apply to TPUs")
            .next("gpu up --provider gcp --help"));
    }
    Ok(())
}

#[derive(Clone)]
pub(crate) struct Prepared {
    pub(crate) picks: crate::gcp_wizard::Picks,
    title: String,
    price: String,
    rate: Option<f64>,
}

fn quote(ctx: &Ctx, cfg: &Dict, picks: &crate::gcp_wizard::Picks) -> (String, Option<f64>) {
    match crate::gcp_prices::load_for(ctx, cfg, &picks.zone, Some((&picks.tpu, picks.spot))) {
        Ok(prices) => (
            prices.quote(&picks.tpu, picks.spot),
            prices.hourly(&picks.tpu, picks.spot),
        ),
        Err(e) => (format!("price unavailable ({})", e.summary()), None),
    }
}

fn launch_title(ctx: &Ctx, picks: &crate::gcp_wizard::Picks, price: &str) -> String {
    format!(
        "Launch {} {} in {} for {price}, deletes at {}? (storage and networking charges apply)",
        crate::gcp_types::public(picks.tpu.clone()),
        if picks.spot { "Spot" } else { "on-demand" },
        picks.zone,
        deadline_label(ctx, picks.deadline)
    )
}

fn prepare(ctx: &Ctx, cfg: &Dict, picks: crate::gcp_wizard::Picks) -> Result<Prepared> {
    let (settings, queue) = launch_setup(ctx, cfg)?;
    crate::gcp_expiry::validate_deadline(ctx, picks.deadline)?;
    startup(&picks.repo, cfg)?;
    queue.reconcile(ctx)?;
    crate::gcp_catalog::verify(ctx, &settings.project, &picks.zone, &picks.tpu)?;
    let (price, rate) = quote(ctx, cfg, &picks);
    let title = launch_title(ctx, &picks, &price);
    Ok(Prepared {
        picks,
        title,
        price,
        rate,
    })
}

fn preparation_job(
    ctx: &Ctx,
    cfg: &Dict,
    picks: crate::gcp_wizard::Picks,
) -> crate::picker::Job<crate::prompt::Confirmation<Prepared>> {
    let worker = ctx.clone();
    let cfg = cfg.clone();
    crate::picker::Job::new(move || {
        let prepared = prepare(&worker, &cfg, picks.clone()).map_err(|e| e.text().to_string())?;
        Ok(crate::prompt::Confirmation {
            title: prepared.title.clone(),
            value: prepared,
        })
    })
}

pub(crate) fn confirmation<V: crate::picker::PickValue>(
    ctx: &Ctx,
    cfg: &Dict,
    picks: crate::gcp_wizard::Picks,
) -> crate::picker::Screen<V, Prepared> {
    crate::prompt::confirmation_screen(preparation_job(ctx, cfg, picks))
}

fn retry_confirmation(
    ctx: &Ctx,
    cfg: &Dict,
    picks: &crate::gcp_wizard::Picks,
    screen: crate::picker::Screen<String, Option<crate::gcp_retry::Choice>>,
    approved: std::rc::Rc<std::cell::RefCell<Option<Prepared>>>,
) -> crate::picker::Screen<String, Option<crate::gcp_retry::Choice>> {
    use crate::picker::Next;
    use std::rc::Rc;
    let worker = ctx.clone();
    let cfg = cfg.clone();
    let picks = picks.clone();
    screen.remap(
        Rc::new(|v| v),
        Rc::new(|v| v),
        Rc::new(move |choice| {
            let Some(choice) = choice else {
                return Next::Done(None);
            };
            let mut picks = picks.clone();
            picks.zone = choice.zone.clone();
            let job = preparation_job(&worker, &cfg, picks);
            let estimate = choice.rate;
            let already_approved = choice.approved;
            let watched = job.clone();
            let approved = approved.clone();
            let screen =
                crate::prompt::confirmation_screen::<String, _>(job).on_settled(move || {
                    let prepared = watched.result()?.value;
                    (already_approved && prepared.rate == estimate).then_some(Next::Done(prepared))
                });
            Next::Screen(screen.remap(
                Rc::new(|v| v),
                Rc::new(|v| v),
                Rc::new(move |prepared| {
                    *approved.borrow_mut() = Some(prepared);
                    Next::Done(Some(choice.clone()))
                }),
            ))
        }),
    )
}

pub(crate) fn launch_prepared(
    ctx: &Ctx,
    flags: &UpFlags,
    cfg: &Dict,
    prepared: Prepared,
) -> Result<()> {
    launch_loop(
        ctx,
        flags,
        cfg,
        Some(prepared.picks.deadline),
        Some(prepared),
        |screen| crate::picker::pick(ctx, screen),
    )
}

pub fn launch(ctx: &Ctx, flags: &UpFlags, cfg: &Dict, deadline: Option<Timestamp>) -> Result<()> {
    launch_with_retry(ctx, flags, cfg, deadline, |screen| {
        crate::picker::pick(ctx, screen)
    })
}
pub(crate) fn launch_with_retry(
    ctx: &Ctx,
    flags: &UpFlags,
    cfg: &Dict,
    deadline: Option<Timestamp>,
    retry: impl FnMut(
        crate::picker::Screen<String, Option<crate::gcp_retry::Choice>>,
    ) -> Result<Option<crate::gcp_retry::Choice>>,
) -> Result<()> {
    launch_loop(ctx, flags, cfg, deadline, None, retry)
}

fn launch_loop(
    ctx: &Ctx,
    flags: &UpFlags,
    cfg: &Dict,
    deadline: Option<Timestamp>,
    mut prepared: Option<Prepared>,
    mut retry: impl FnMut(
        crate::picker::Screen<String, Option<crate::gcp_retry::Choice>>,
    ) -> Result<Option<crate::gcp_retry::Choice>>,
) -> Result<()> {
    let (settings, queue) = launch_setup(ctx, cfg)?;
    validate_tpu_flags(flags)?;
    if prepared.is_none() && ctx.interactive() && !flags.yes && !flags.dry_run {
        prepared = Some(crate::gcp_wizard::choose_confirmed(
            ctx, flags, &settings, cfg, deadline,
        )?);
    }
    let mut picks = match &prepared {
        Some(prepared) => prepared.picks.clone(),
        None => crate::gcp_wizard::choose(ctx, flags, &settings, cfg, deadline)?,
    };
    let deadline = picks.deadline;
    let mut confirm = !flags.yes;
    let mut approved_rate = None;
    let mut check_rate = false;
    let mut attempted = std::collections::BTreeSet::new();
    loop {
        attempted.insert(picks.zone.clone());
        crate::gcp_expiry::validate_deadline(ctx, deadline)?;
        let approved = prepared.take();
        if !flags.dry_run {
            if approved.is_none() {
                queue.reconcile(ctx)?;
                crate::gcp_catalog::verify(ctx, &settings.project, &picks.zone, &picks.tpu)?;
            }
            crate::gcp_preferences::remember(ctx, &settings.project, &picks.zone);
            crate::gcp_preferences::remember_type(ctx, &settings.project, &picks.tpu);
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
            ctx.result("provider", Value::str("gcp"));
            ctx.result("id", Value::Null);
            ctx.result("ssh_alias", Value::Null);
            let rate = crate::gcp_prices::cached(ctx, cfg, &picks.zone)
                .and_then(|prices| prices.hourly(&picks.tpu, picks.spot));
            crate::guardrails::spending(ctx, cfg, rate, Some(deadline))?;
            ctx.result("price_per_hour", rate.map_or(Value::Null, Value::Float));
            ctx.result("expires_at", Value::str(deadline.to_string()));
            ctx.result("ready", Value::Bool(false));
            ctx.result("created", Value::Bool(false));
            ctx.result("expiry_armed", Value::Bool(false));
            ctx.println("DRY RUN: no TPU or expiry task created")?;
            let requests = [queue.create_args(ctx, &id, deadline)?, args];
            let requests = Value::List(
                requests
                    .into_iter()
                    .map(|args| Value::List(args.into_iter().map(Value::Str).collect()))
                    .collect(),
            );
            ctx.result("request", requests.clone());
            ctx.println(&pyjson::dumps(&requests, Some(2)))?;
            return Ok(());
        }
        let (price, rate) = approved
            .as_ref()
            .map(|p| (p.price.clone(), p.rate))
            .unwrap_or_else(|| quote(ctx, cfg, &picks));
        crate::guardrails::spending(ctx, cfg, rate, Some(deadline))?;
        ctx.result("provider", Value::str("gcp"));
        ctx.result("id", Value::str(id.cli()));
        ctx.result("ssh_alias", Value::Null);
        ctx.result("price_per_hour", rate.map_or(Value::Null, Value::Float));
        ctx.result("expires_at", Value::str(deadline.to_string()));
        ctx.result("ready", Value::Bool(false));
        ctx.result("created", Value::Bool(false));
        ctx.result("expiry_armed", Value::Bool(false));
        let local = deadline_label(ctx, deadline);
        if approved.is_none() {
            ctx.println(&launch_title(ctx, &picks, &price))?;
        }
        if check_rate && rate != approved_rate {
            confirm = true;
        }
        if approved.is_none() && confirm && !crate::prompt::confirm(ctx, "Type yes: ")? {
            return ctx.println("Cancelled.");
        }
        if deadline.as_second() <= ctx.clock.now().as_second() {
            return Err(Error::msg("Expiry deadline passed. No TPU created")
                .next("gpu up --provider gcp --expire 2h"));
        }
        let wait = crate::progress::Wait::start(ctx, "Waiting for Google Cloud to create the TPU")?;
        queue.check(ctx)?;
        // Arm and read back the control-plane task BEFORE issuing create. A laptop
        // crash, SSH failure, stopped VM or guest failure cannot remove this timer.
        queue.arm(ctx, &id, deadline)?;
        ctx.result("expiry_armed", Value::Bool(true));
        if deadline.as_second() <= ctx.clock.now().as_second() {
            return Err(Error::msg(
                "Expiry deadline passed. No TPU created. Its deletion task may remain",
            )
            .next(format!("gpu expire {} cancel", id.cli())));
        }
        ctx.result("created", Value::Null);
        if let Err(e) = run(ctx, &args, "gpu up --provider gcp --spot --expire 2h") {
            // A known request rejection plus an authenticated 404 is terminal.
            // Keep the timer after ambiguous errors/timeouts (create may be in flight).
            let rejected = e.is_rejection();
            if rejected && absent(ctx, &id).unwrap_or(false) && queue.cancel(ctx, &id).is_ok() {
                ctx.result("expiry_armed", Value::Bool(false));
                ctx.result("created", Value::Bool(false));
                drop(wait);
                if matches!(e, Error::Status(crate::error::ExitCode::NoCapacity, _)) {
                    let failure = format!(
                        "No {} capacity in {}. No node created. Deletion task removed.",
                        if picks.spot { "Spot" } else { "on-demand" },
                        picks.zone
                    );
                    if ctx.interactive() {
                        ctx.println(&failure)?;
                        let approved_retry = std::rc::Rc::new(std::cell::RefCell::new(None));
                        let screen =
                            crate::gcp_retry::screen(ctx, &settings, cfg, &picks, &attempted);
                        let screen = if !flags.yes {
                            retry_confirmation(ctx, cfg, &picks, screen, approved_retry.clone())
                        } else {
                            screen
                        };
                        if let Some(choice) = retry(screen)? {
                            prepared = approved_retry.borrow_mut().take();
                            picks.zone = choice.zone;

                            confirm = !choice.approved;
                            approved_rate = choice.rate;
                            check_rate = choice.approved;
                            continue;
                        }
                        return ctx.println("Cancelled.");
                    }
                    let alternatives =
                        crate::gcp_retry::alternatives(ctx, &settings, &picks, &attempted, 3);
                    let zones = alternatives
                        .iter()
                        .map(|z| z.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let next = alternatives
                        .first()
                        .map(|z| crate::gcp_retry::command(&picks, &z.name))
                        .unwrap_or_else(|| "gpu up --provider gcp".into());
                    return Err(Error::status(
                        crate::error::ExitCode::NoCapacity,
                        format!(
                            "{failure}\nOther {} zones: {}.",
                            picks.tpu,
                            if zones.is_empty() {
                                "none verified"
                            } else {
                                &zones
                            }
                        ),
                    )
                    .next_line(next));
                }
                let message = format!(
                    "Create rejected: {}. No node created. Deletion task removed",
                    e.summary()
                );
                return Err(e.with_text(message).next("gpu up --provider gcp"));
            }
            let message = format!(
                "{}\nCreate {} unverified. Deletion task remains armed",
                e.summary(),
                id.cli()
            );
            return Err(e.with_text(message).next("gpu ls --provider gcp"));
        }
        drop(wait);
        ctx.result("created", Value::Bool(true));
        ctx.println(&format!(
            "Created TPU {}. Deletes at {local}.\nNext: gpu ssh {} --exec",
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
        return Ok(());
    }
}

/// Keep today's deadline short, but never hide a different date or year.
pub(crate) fn deadline_label(ctx: &Ctx, deadline: Timestamp) -> String {
    let zone = crate::expiry::local_zone(&ctx.env);
    let now = ctx.clock.now().to_zoned(zone.clone());
    let local = deadline.to_zoned(zone);
    let format = if local.date() == now.date() {
        "%-I:%M %p"
    } else if local.year() == now.year() {
        "%b %-d at %-I:%M %p"
    } else {
        "%b %-d %Y at %-I:%M %p"
    };
    local.strftime(format).to_string()
}

pub fn unique_name(ctx: &Ctx, prefix: &str) -> Result<String> {
    // Generate directly, without creating a throwaway file. Never offer user
    // names: a lingering deletion task must not target a replacement instance.
    let mut random = [0u8; 6];
    getrandom::fill(&mut random)
        .map_err(|_| Error::msg("Could not generate a unique TPU name."))?;
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let suffix: String = random
        .into_iter()
        .map(|byte| alphabet[byte as usize % alphabet.len()] as char)
        .collect();
    Ok(format!("{prefix}-{}-{suffix}", ctx.clock.now().as_second()))
}

fn startup(repo: &crate::repos::Repo, cfg: &Dict) -> Result<String> {
    let owner = crate::pyfmt::py_str(crate::config::get(cfg, "owner")?);
    let (url, name) = crate::repos::resolve_repo(repo.as_arg(), &owner)?;
    let mut script = format!(
        "#!/bin/bash\n{}\nchmod 1777 /workspace\n",
        crate::startup::SETUP_START
    );
    for (key, field) in [("git_name", "name"), ("git_email", "email")] {
        if let Some(value) = cfg
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            script.push_str(&format!(
                "git config --global user.{field} {}\n",
                crate::pyfmt::shell_quote(value)
            ));
        }
    }
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
    script.push_str(crate::startup::SETUP_DONE);
    script.push('\n');
    Ok(script)
}

/// Clean up local records of a TPU gpu recorded but that is already gone.
fn forget_absent(ctx: &Ctx, id: &TpuId, cfg: &Dict) -> Result<()> {
    let queue = crate::gcp_expiry::queue_for_node(cfg, id, &Dict::new()).ok();
    let tasks = match &queue {
        Some(queue) => queue.has_tasks(ctx, id)?,
        None => false,
    };
    if !tasks && !crate::gcp_ssh::has(ctx, id)? {
        return Err(Error::not_found(&id.cli()));
    }
    if let (Some(queue), true) = (&queue, tasks) {
        queue.cancel(ctx, id)?;
    }
    crate::gcp_ssh::remove(ctx, id)?;
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
            &format!("Destroy TPU {} and its boot disk? Type yes: ", id.cli()),
        )?
    {
        return ctx.println("Cancelled.");
    }
    let wait = crate::progress::Wait::start(
        ctx,
        if destroy {
            "Waiting for Google Cloud to delete the TPU"
        } else {
            "Waiting for Google Cloud to stop the TPU"
        },
    )?;
    if destroy {
        crate::gcp_expiry::reconcile(ctx, cfg)?;
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
        match crate::gcp_expiry::queue_for_node(cfg, id, &node) {
            Ok(queue) => queue.cancel(ctx, id).map_err(|_| cleanup_error())?,
            Err(_) if recorded_queue => return Err(cleanup_error()),
            // Unmanaged nodes may have no gpu expiry config or recorded queue.
            Err(_) => {}
        }
        crate::gcp_ssh::remove(ctx, id)?;
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
