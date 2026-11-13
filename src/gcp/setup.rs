//! Explicit, project-scoped Google Cloud onboarding and removal for
//! `gpu setup`: project choice, APIs, IAM and the expiry queue. The local
//! journal survives partial setup and removal.

use crate::cli::SetupFlags;
use crate::ctx::Ctx;
use crate::error::{Error, ExitCode, Result};
use crate::gcp::{self, component, text};
use crate::picker::{pick, Job, Next, Opt, Screen};
use crate::pyjson::{Dict, Value};

pub(crate) fn gcp_not_setup(code: Option<ExitCode>) -> Error {
    let text = "Google Cloud is not set up. No gcloud command ran";
    match code {
        Some(code) => Error::status(code, text),
        None => Error::msg(text),
    }
    .next_line("gpu setup")
}
const QUEUE: &str = "gpu-expiry";
// Compute supplies the default compute identity and SSH infrastructure.
const APIS: [&str; 4] = [
    "compute.googleapis.com",
    "tpu.googleapis.com",
    "cloudtasks.googleapis.com",
    "cloudbilling.googleapis.com",
];

pub fn gcp_ready(cfg: &Dict) -> bool {
    let Some(gcp) = cfg.get("gcp").and_then(Value::as_dict) else {
        return false;
    };
    let Some(setup) = gcp.get("setup").and_then(Value::as_dict) else {
        return false;
    };
    setup.get("version") == Some(&Value::Int(1))
        && setup.get("removing") != Some(&Value::Bool(true))
        && text(gcp, "project").is_some_and(|p| component(p) && text(setup, "project") == Some(p))
}

fn cloud_json(ctx: &Ctx, args: &[&str]) -> Result<Value> {
    let mut argv = vec!["gcloud".to_string()];
    argv.extend(args.iter().map(|s| s.to_string()));
    argv.push("--quiet".into());
    gcp::json(ctx, argv, "gpu setup")
}

fn choose_project(ctx: &Ctx, explicit: Option<&str>) -> Result<Dict> {
    if let Some(project) = explicit {
        if !component(project) {
            return Err(Error::msg("Invalid project ID").next("gpu setup"));
        }
        return gcp::objects(cloud_json(
            ctx,
            &["projects", "list", "--filter=lifecycleState:ACTIVE"],
        )?)?
        .into_iter()
        .find(|p| text(p, "projectId") == Some(project))
        .ok_or_else(|| {
            Error::msg("Selected project is not in your active project list").next("gpu setup")
        });
    }
    if !ctx.interactive() {
        return Err(Error::msg("Choose an existing project with --project")
            .next("gpu setup --provider gcp --help"));
    }
    let worker = ctx.clone();
    let job = Job::new(move || {
        cloud_json(
            &worker,
            &["projects", "list", "--filter=lifecycleState:ACTIVE"],
        )
        .and_then(gcp::objects)
        .map_err(|e| e.text().into())
    });
    let options = job.clone();
    let selected: String = pick(
        ctx,
        Screen::new(
            "Google Cloud project",
            move || {
                let projects = options.result().unwrap_or_default();
                let rows = projects
                    .iter()
                    .filter_map(|p| {
                        let id = text(p, "projectId")?;
                        Some([id.to_string(), text(p, "name").unwrap_or(id).to_string()])
                    })
                    .collect::<Vec<_>>();
                let labels = crate::table::aligned_rows(
                    &rows,
                    [crate::table::Column::Left, crate::table::Column::Plain],
                );
                rows.into_iter()
                    .zip(labels)
                    .map(|(row, label)| Opt::new(row[0].clone(), label))
                    .collect()
            },
            Next::Done,
        )
        .load(job.clone())
        .loading("Loading Google Cloud projects…")
        .empty("No active Google Cloud projects"),
    )?;
    job.result()
        .unwrap_or_default()
        .into_iter()
        .find(|p| text(p, "projectId") == Some(&selected))
        .ok_or_else(gcp::bad_reply)
}

#[derive(Clone)]
struct Plan {
    project: String,
    number: String,
    zone: Option<String>,
    location: String,
    enabled: Vec<String>,
    created_queue: bool,
    version: i64,
    removing: bool,
}
impl Plan {
    fn account(&self) -> String {
        format!("{}-compute@developer.gserviceaccount.com", self.number)
    }
    fn resource(&self) -> String {
        format!(
            "projects/{}/locations/{}/queues/{QUEUE}",
            self.project, self.location
        )
    }
    fn args(&self, parts: &[&str]) -> Vec<String> {
        let mut args = vec!["gcloud".into()];
        args.extend(parts.iter().map(|s| s.to_string()));
        args.extend([format!("--project={}", self.project), "--quiet".into()]);
        args
    }
    fn json(&self, ctx: &Ctx, parts: &[&str]) -> Result<Value> {
        gcp::json(ctx, self.args(parts), "gpu setup")
    }
    fn run(&self, ctx: &Ctx, parts: &[&str]) -> Result<()> {
        gcp::run(ctx, &self.args(parts), "gpu setup").map(|_| ())
    }
    fn services(&self, ctx: &Ctx) -> Result<Vec<String>> {
        gcp::objects(self.json(ctx, &["services", "list", "--enabled"])?)?
            .iter()
            .map(|s| {
                s.get("config")
                    .and_then(Value::as_dict)
                    .and_then(|c| text(c, "name"))
                    .map(String::from)
                    .ok_or_else(gcp::bad_reply)
            })
            .collect()
    }
    fn queue_exists(&self, ctx: &Ctx) -> Result<bool> {
        Ok(gcp::objects(self.json(
            ctx,
            &[
                "tasks",
                "queues",
                "list",
                &format!("--location={}", self.location),
            ],
        )?)?
        .iter()
        .any(|q| text(q, "name") == Some(&self.resource())))
    }
    fn queue(&self) -> Result<crate::gcp::expiry::Queue> {
        let mut cfg = Dict::new();
        cfg.insert("gcp", Value::Dict(self.config()));
        crate::gcp::expiry::Queue::load(&gcp::Settings::load(&cfg)?)
    }
    fn config(&self) -> Dict {
        let mut setup = Dict::new();
        setup.insert("version", Value::Int(self.version));
        setup.insert("project", Value::str(&self.project));
        setup.insert("project_number", Value::str(&self.number));
        setup.insert(
            "enabled_apis",
            Value::List(self.enabled.iter().map(Value::str).collect()),
        );
        setup.insert("created_queue", Value::Bool(self.created_queue));
        setup.insert("removing", Value::Bool(self.removing));
        let mut expiry = Dict::new();
        expiry.insert("queue", Value::str(QUEUE));
        expiry.insert("location", Value::str(&self.location));
        expiry.insert("service_account", Value::str(self.account()));
        let mut gcp = Dict::new();
        gcp.insert("project", Value::str(&self.project));
        if let Some(zone) = &self.zone {
            gcp.insert("zone", Value::str(zone));
        }
        gcp.insert("service_account", Value::str(self.account()));
        gcp.insert("expiry", Value::Dict(expiry));
        gcp.insert("setup", Value::Dict(setup));
        gcp
    }
    fn recorded(cfg: &Dict) -> Result<Option<Self>> {
        let Some(gcp) = cfg.get("gcp").and_then(Value::as_dict) else {
            return Ok(None);
        };
        let Some(setup) = gcp.get("setup").and_then(Value::as_dict) else {
            return Ok(None);
        };
        let invalid = || {
            Error::msg(
                "Setup record incomplete. Restore the original config backup before removal.",
            )
        };
        let project = text(setup, "project")
            .filter(|s| component(s))
            .ok_or_else(invalid)?
            .to_string();
        if text(gcp, "project") != Some(&project) {
            return Err(invalid());
        }
        let number = text(setup, "project_number")
            .filter(|s| crate::pyfmt::is_digits(s))
            .ok_or_else(invalid)?
            .to_string();
        let enabled = match setup.get("enabled_apis") {
            Some(Value::List(apis)) => apis
                .iter()
                .map(|v| match v {
                    Value::Str(s) if APIS.contains(&s.as_str()) => Ok(s.clone()),
                    _ => Err(invalid()),
                })
                .collect::<Result<Vec<_>>>()?,
            _ => return Err(invalid()),
        };
        let created_queue = match setup.get("created_queue") {
            Some(Value::Bool(b)) => *b,
            _ => return Err(invalid()),
        };
        let expiry = gcp
            .get("expiry")
            .and_then(Value::as_dict)
            .ok_or_else(invalid)?;
        if text(expiry, "queue") != Some(QUEUE) {
            return Err(invalid());
        }
        let location = text(expiry, "location")
            .filter(|s| component(s))
            .ok_or_else(invalid)?
            .to_string();
        Ok(Some(Self {
            project,
            number,
            location,
            enabled,
            created_queue,
            zone: text(gcp, "zone").map(String::from),
            version: if setup.get("version") == Some(&Value::Int(1)) {
                1
            } else {
                0
            },
            removing: setup.get("removing") == Some(&Value::Bool(true)),
        }))
    }
    // Read-only, conservative preflight. Do not silently grant IAM or enable an
    // additional troubleshooting API. Conditional/inherited/custom grants are
    // not assumed sufficient. Deny policies can still override an allow grant.
    fn check_identity(&self, ctx: &Ctx) -> Result<()> {
        let sa = self.json(
            ctx,
            &["iam", "service-accounts", "describe", &self.account()],
        )?;
        let sa = sa.as_dict().ok_or_else(gcp::bad_reply)?;
        if text(sa, "email") != Some(&self.account())
            || sa.get("disabled") == Some(&Value::Bool(true))
        {
            return Err(Error::msg(
                "Default Compute identity missing/disabled. Ask an admin. gpu creates no identity.",
            ));
        }
        let policy = self.json(ctx, &["projects", "get-iam-policy", &self.project])?;
        let bindings = policy.as_dict().and_then(|d| d.get("bindings"));
        let granted = if let Some(Value::List(bindings)) = bindings {
            bindings.iter().filter_map(Value::as_dict).any(|binding| {
                !binding.contains_key("condition")
                    && matches!(text(binding, "role"), Some("roles/owner" | "roles/editor" | "roles/tpu.admin"))
                    && matches!(binding.get("members"), Some(Value::List(m)) if m.contains(&Value::str(format!("serviceAccount:{}", self.account()))))
            })
        } else {
            false
        };
        if !granted {
            return Err(Error::msg(format!("No recognized tpu.nodes.delete grant for {}.\nAsk an admin to review conditional/inherited/custom \
                grants. gpu changes no IAM.", self.account())));
        }
        ctx.println("TPU-delete grant found. Deny policies or IAM changes can block expiry.")
    }
    fn confirm(&self, ctx: &Ctx, flags: &SetupFlags) -> Result<bool> {
        if flags.dry_run {
            ctx.println("DRY RUN: no cloud resources or config changed.")?;
            return Ok(false);
        }
        if flags.confirm_project.as_deref() == Some(&self.project) {
            return Ok(true);
        }
        if !ctx.interactive() {
            return Err(Error::msg(format!(
                "Approve changes explicitly with --confirm-project {}.",
                self.project
            )));
        }
        let reply = crate::prompt::read_text(
            ctx,
            &format!("Type project ID '{}' to confirm: ", self.project),
        )?;
        if reply.trim() == self.project {
            Ok(true)
        } else {
            ctx.println("Cancelled. No cloud resources or config changed.")?;
            Ok(false)
        }
    }
}

pub(crate) fn setup_gcp(ctx: &Ctx, cfg: &Dict, flags: &SetupFlags) -> Result<()> {
    if flags.confirm_project.is_some() && flags.confirm_project != flags.project {
        return Err(Error::msg(
            "--confirm-project must match the explicit --project.",
        ));
    }
    let previous = Plan::recorded(cfg)?;
    let location = flags.location.as_deref().unwrap_or_else(|| {
        previous
            .as_ref()
            .map_or("us-central1", |p| p.location.as_str())
    });
    if !component(location) {
        return Err(Error::msg("Invalid queue region."));
    }
    if previous.as_ref().is_some_and(|p| p.removing) {
        return Err(Error::msg(
            "Finish the interrupted removal before configuring again. Select Remove in gpu setup.",
        ));
    }
    let selected = choose_project(
        ctx,
        flags
            .project
            .as_deref()
            .or_else(|| previous.as_ref().map(|p| p.project.as_str())),
    )?;
    let project = text(&selected, "projectId")
        .filter(|s| component(s))
        .ok_or_else(gcp::bad_reply)?
        .to_string();
    let number = selected
        .get("projectNumber")
        .map(crate::pyfmt::py_str)
        .filter(|s| crate::pyfmt::is_digits(s))
        .ok_or_else(gcp::bad_reply)?;
    if previous
        .as_ref()
        .is_some_and(|p| p.project != project || p.number != number || p.location != location)
    {
        return Err(Error::msg(
            "Remove setup before changing project/queue region. Keep the undo record.",
        ));
    }
    let mut plan = Plan {
        project,
        number,
        zone: previous.as_ref().and_then(|p| p.zone.clone()),
        location: location.into(),
        enabled: previous
            .as_ref()
            .map(|p| p.enabled.clone())
            .unwrap_or_default(),
        created_queue: previous.as_ref().is_some_and(|p| p.created_queue),
        version: previous.as_ref().map_or(0, |p| p.version),
        removing: false,
    };
    ctx.result("project", Value::str(&plan.project));
    ctx.result("ready", Value::Bool(gcp_ready(cfg)));
    ctx.result("queue", Value::str(plan.resource()));
    let enabled = plan.services(ctx)?;
    let missing: Vec<_> = APIS
        .iter()
        .filter(|api| !enabled.iter().any(|s| s == **api))
        .copied()
        .collect();
    ctx.result(
        "enable_apis",
        Value::List(missing.iter().map(|api| Value::str(*api)).collect()),
    );
    // Listing queues with a disabled API can trigger an enable prompt; never do it.
    let exists =
        enabled.iter().any(|s| s == "cloudtasks.googleapis.com") && plan.queue_exists(ctx)?;
    if exists {
        plan.queue()?.check(ctx)?;
    }
    ctx.println(&format!(
        "Set up {} (use a disposable project). No TPU created.",
        plan.project
    ))?;
    ctx.println(&format!(
        "  Queue region: {}. Choose TPU type/zone at launch.",
        plan.location
    ))?;
    for api in &missing {
        ctx.println(&format!("  Enable API: {api}"))?;
    }
    ctx.println(&format!(
        "  {} queue: {} (unlimited retries, backoff <=60s)",
        if exists { "Reuse unchanged" } else { "Create" },
        plan.resource()
    ))?;
    ctx.println(&format!(
        "  VM + expiry identity: {}. VM users inherit its roles.",
        plan.account()
    ))?;
    ctx.println(&format!(
        "Adds only the {QUEUE} queue. No project/identity/IAM/network/billing/quota edits.\n\
         Enabling APIs may add Google-managed defaults. Undo keeps those."
    ))?;
    ctx.println(&format!(
        "Keep undo record: {}.\nNo concurrent setup or cloud edits.",
        crate::config::config_path(&ctx.env).display()
    ))?;
    if !plan.confirm(ctx, flags)? {
        return Ok(());
    }
    // Journal intent BEFORE requests: a timeout/interruption may still have applied
    // the mutation. On retry list actual state, never replay a blind delete/create.
    save_setup_progress(ctx, &plan)?;
    for api in missing {
        // A service enabled during review is not ours to disable later.
        if plan.services(ctx)?.iter().any(|s| s == api) {
            continue;
        }
        if !plan.enabled.iter().any(|s| s == api) {
            plan.enabled.push(api.into());
        }
        save_setup_progress(ctx, &plan)?;
        plan.run(ctx, &["services", "enable", api]).map_err(|_| {
            Error::msg(format!(
                "Setup stopped enabling {api}. Completion unverified."
            ))
        })?;
        ctx.println(&format!("Enabled API: {api}."))?;
    }
    // An API-disabled project's pre-consent list was necessarily offline.
    plan.check_identity(ctx)?;
    let now_exists = plan.queue_exists(ctx)?;
    if exists && !now_exists {
        return Err(Error::msg(
            "Queue disappeared during setup. Rerun to review a new plan.",
        ));
    }
    if !now_exists {
        plan.created_queue = true;
        crate::config::save_gcp(&ctx.env, Some(plan.config()))?;
        plan.run(
            ctx,
            &[
                "tasks",
                "queues",
                "create",
                QUEUE,
                &format!("--location={}", plan.location),
                "--max-attempts=-1",
                "--max-retry-duration=0s",
                "--min-backoff=5s",
                "--max-backoff=60s",
            ],
        )
        .map_err(|_| {
            Error::msg(format!(
                "Setup stopped creating {QUEUE}. Completion unverified."
            ))
        })?;
        ctx.println(&format!("Created queue: {QUEUE}."))?;
    } else {
        ctx.println(&format!("Reused queue: {QUEUE} (unchanged)."))?;
    }
    plan.queue()?.check(ctx)?;
    ctx.println("Fetching TPU types, zones and SKU IDs...")?;
    crate::gcp::catalog::refresh(ctx, &plan.project)?;
    crate::gcp::facts::refresh_skus(ctx, &plan.project)?;
    plan.version = 1;
    crate::config::save_gcp(&ctx.env, Some(plan.config()))?;
    ctx.result("ready", Value::Bool(true));
    ctx.println("Google Cloud  ✓ set up\nNext: gpu up --provider gcp")
}

fn save_setup_progress(ctx: &Ctx, plan: &Plan) -> Result<()> {
    crate::config::save_gcp(&ctx.env, Some(plan.config()))
}

/// Rechecked after confirmation as well: a user may spend minutes reviewing
/// the plan. Still require no concurrent cloud edits; this is not a cloud lock.
fn ensure_unused(ctx: &Ctx, plan: &Plan, enabled: &[String]) -> Result<bool> {
    if !plan.removing {
        // Disabling an API does not prove its resources were destroyed. Only an
        // interrupted removal has already checked absence before disabling it.
        for (api, may_have_resources) in [
            ("tpu.googleapis.com", plan.version == 1),
            (
                "cloudtasks.googleapis.com",
                plan.version == 1 || plan.created_queue,
            ),
        ] {
            if may_have_resources && !enabled.iter().any(|s| s == api) {
                return Err(Error::msg(format!("{api} disabled. Absence unverified. Nothing removed. Ask an admin to restore it.")));
            }
        }
    }
    if enabled.iter().any(|s| s == "tpu.googleapis.com") {
        for node in gcp::all_nodes(ctx, &plan.project)? {
            let id = gcp::node_id(&node, &plan.project)?;
            let owned = node
                .get("labels")
                .and_then(Value::as_dict)
                .and_then(|d| text(d, "gpu-cli"))
                == Some("true")
                || node
                    .get("metadata")
                    .and_then(Value::as_dict)
                    .is_some_and(|d| text(d, "gpu-expiry-queue").is_some());
            if owned {
                return Err(Error::msg("Destroy this TPU before removing setup")
                    .next(format!("gpu down {}", id.cli())));
            }
        }
    }
    let exists =
        enabled.iter().any(|s| s == "cloudtasks.googleapis.com") && plan.queue_exists(ctx)?;
    if exists {
        let tasks = gcp::objects(plan.json(
            ctx,
            &[
                "tasks",
                "list",
                &format!("--location={}", plan.location),
                &format!("--queue={QUEUE}"),
            ],
        )?)?;
        if !tasks.is_empty() {
            return Err(Error::msg(
                "Pending tasks block removal. Use gpu down or gpu expire ID cancel",
            )
            .next("gpu ls --provider gcp"));
        }
    }
    Ok(exists)
}

pub(crate) fn remove_gcp(ctx: &Ctx, cfg: &Dict, flags: &SetupFlags) -> Result<()> {
    let Some(mut plan) = Plan::recorded(cfg)? else {
        return if cfg.get("gcp").is_some() {
            Err(Error::msg(
                "No gpu setup record. Cannot remove untracked infrastructure.",
            ))
        } else {
            ctx.result("ready", Value::Bool(false));
            ctx.println("Google Cloud  not set up (nothing to remove).")
        };
    };
    if flags.project.as_ref().is_some_and(|p| p != &plan.project)
        || flags
            .confirm_project
            .as_ref()
            .is_some_and(|p| p != &plan.project)
    {
        return Err(Error::msg(
            "Removal project must match the recorded setup project.",
        ));
    }
    let enabled = plan.services(ctx)?;
    // Fail closed on list failures, and inspect ALL zones, not just the default.
    let exists = ensure_unused(ctx, &plan, &enabled)?;
    ctx.println(&format!("Remove gpu setup from project {}:", plan.project))?;
    if exists && plan.created_queue {
        ctx.println(&format!("  Delete empty queue: {}", plan.resource()))?;
    } else {
        ctx.println("  Keep any pre-existing queue unchanged.")?;
    }
    for api in &plan.enabled {
        if enabled.contains(api) {
            ctx.println(&format!("  Disable API enabled by gpu: {api}"))?;
        }
    }
    ctx.println("Remove local gcp config. Keep pre-existing APIs/queues and other config. No project/IAM/billing/quota edits.")?;
    if !plan.confirm(ctx, flags)? {
        return Ok(());
    }
    let enabled = plan.services(ctx)?;
    let exists = ensure_unused(ctx, &plan, &enabled)?;
    // Block ordinary cloud commands while removal is incomplete, but retain undo.
    plan.removing = true;
    crate::config::save_gcp(&ctx.env, Some(plan.config()))?;
    if exists && plan.created_queue {
        plan.run(
            ctx,
            &[
                "tasks",
                "queues",
                "delete",
                QUEUE,
                &format!("--location={}", plan.location),
            ],
        )
        .map_err(|_| {
            Error::msg(format!(
                "Removal stopped deleting {QUEUE}. Completion unverified."
            ))
        })?;
        if plan.queue_exists(ctx)? {
            return Err(Error::msg("Queue deletion unverified. APIs not disabled."));
        }
        ctx.println(&format!("Deleted queue: {QUEUE}."))?;
    }
    // Reverse enable order; never --force (do not disable dependents).
    for api in plan.enabled.iter().rev() {
        if enabled.contains(api) {
            plan.run(ctx, &["services", "disable", api]).map_err(|_| {
                Error::msg(format!(
                    "Removal stopped disabling {api}. Retry Remove. Dependent services are never force-disabled."
                ))
            })?;
            ctx.println(&format!("Disabled API: {api}."))?;
        }
    }
    let remaining = plan.services(ctx)?;
    if plan.enabled.iter().any(|s| remaining.contains(s)) {
        return Err(Error::msg("API removal unverified. Undo record kept."));
    }
    crate::config::save_gcp(&ctx.env, None)?;
    ctx.result("ready", Value::Bool(false));
    ctx.println("Removed gpu setup. Google Cloud  not set up\nNext: gpu setup")
}
