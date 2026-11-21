//! Control-plane TPU expiry via Cloud Tasks HTTP DELETE with an OAuth service
//! account. Unlike a guest worker this survives stop, preemption and laptop loss.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::gcp::{self, component, text, Settings, TpuId};
use crate::pyjson::{Dict, Value};
use jiff::Timestamp;

#[derive(Clone, Debug)]
pub struct Queue {
    project: String,
    location: String,
    name: String,
    service_account: String,
}

pub(crate) const EXPIRY_SECONDS: std::ops::RangeInclusive<i64> = 60..=29 * 86400;
pub(crate) fn expiry_bound() -> Error {
    Error::msg("Expiry must be 1 minute–29 days away; selection/boot uses that time")
        .next("gpu up --provider gcp --expire 2h")
}

pub fn validate_deadline(ctx: &Ctx, at: Timestamp) -> Result<()> {
    let seconds = at.as_second() - ctx.clock.now().as_second();
    // Tasks permits 30 days ahead with 31-day retention from creation. Leave
    // two days for retries and a minute for submitting the create request.
    if !EXPIRY_SECONDS.contains(&seconds) {
        return Err(expiry_bound());
    }
    Ok(())
}
impl Queue {
    pub fn load(settings: &Settings) -> Result<Self> {
        let d = settings
            .expiry
            .as_ref()
            .ok_or_else(|| Error::msg("Expiry queue not configured").next("gpu setup"))?;
        let get = |key| {
            text(d, key)
                .filter(|s| component(s))
                .map(String::from)
                .ok_or_else(|| {
                    Error::msg(format!("Set gcp.expiry.{key} in gpu/config.json"))
                        .next("gpu up --provider gcp --help")
                })
        };
        let service_account = text(d, "service_account")
            .filter(|s| {
                let default_compute = s
                    .strip_suffix("-compute@developer.gserviceaccount.com")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
                (default_compute
                    || s.ends_with(&format!("@{}.iam.gserviceaccount.com", settings.project)))
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-@.".contains(&b))
            })
            .ok_or_else(|| {
                Error::msg(
                    "Set gcp.expiry.service_account to a service account in the queue's project",
                )
                .next("gpu up --provider gcp --help")
            })?
            .into();
        Ok(Self {
            project: settings.project.clone(),
            location: get("location")?,
            name: get("queue")?,
            service_account,
        })
    }
    pub fn resource(&self) -> String {
        format!(
            "projects/{}/locations/{}/queues/{}",
            self.project, self.location, self.name
        )
    }
    fn args(&self, action: &str) -> Vec<String> {
        vec![
            "gcloud".into(),
            "tasks".into(),
            action.into(),
            format!("--project={}", self.project),
            format!("--location={}", self.location),
            format!("--queue={}", self.name),
            "--quiet".into(),
        ]
    }
    pub fn check(&self, ctx: &Ctx) -> Result<()> {
        let args = vec![
            "gcloud".into(),
            "tasks".into(),
            "queues".into(),
            "describe".into(),
            self.name.clone(),
            format!("--project={}", self.project),
            format!("--location={}", self.location),
            "--quiet".into(),
        ];
        let data = gcp::json(ctx, args, "gpu ls --provider gcp")?;
        let d = data.as_dict().ok_or_else(gcp::bad_reply)?;
        let retry = d.get("retryConfig").and_then(Value::as_dict);
        let unlimited = retry.is_some_and(|r| {
            r.get("maxAttempts") == Some(&Value::Int(-1))
                && matches!(text(r, "maxRetryDuration"), None | Some("0s"))
        });
        let short_backoff = retry
            .and_then(|r| text(r, "maxBackoff"))
            .and_then(|s| s.strip_suffix('s'))
            .and_then(|s| s.parse::<f64>().ok())
            .is_some_and(|s| s > 0.0 && s <= 60.0);
        if text(d, "state") != Some("RUNNING")
            || !unlimited
            || !short_backoff
            || d.get("httpTarget").is_some()
        {
            return Err(Error::msg(
                "Expiry queue must be RUNNING with unlimited retries, max backoff <=60s and no HTTP target override",
            )
            .next(format!(
                "gcloud tasks queues describe {} --project={} --location={}",
                self.name, self.project, self.location
            )));
        }
        Ok(())
    }
    pub fn create_args(&self, ctx: &Ctx, id: &TpuId, deadline: Timestamp) -> Result<Vec<String>> {
        let mut args = self.args("create-http-task");
        args.extend([
            gcp::unique_name(ctx, "gpu-expire")?,
            format!("--url={}", id.url()),
            "--method=DELETE".into(),
            // Cloud Tasks stores scheduleTime with microsecond precision. Use
            // whole seconds so read-back verification also works with a real
            // nanosecond-resolution local clock (never round the deadline up).
            format!(
                "--schedule-time={}",
                Timestamp::from_second(deadline.as_second()).map_err(|_| gcp::bad_reply())?
            ),
            format!("--oauth-service-account-email={}", self.service_account),
            "--oauth-token-scope=https://www.googleapis.com/auth/cloud-platform".into(),
        ]);
        Ok(args)
    }
    pub fn arm(&self, ctx: &Ctx, id: &TpuId, deadline: Timestamp) -> Result<()> {
        if deadline.as_second() <= ctx.clock.now().as_second() {
            return Err(
                Error::msg("Expiry deadline passed; no new deletion task created")
                    .next(format!("gpu expire {} 2h", id.cli())),
            );
        }
        if self.service_account.is_empty() {
            return Err(Error::msg(
                "Configure the expiry service account for this project before re-arming",
            )
            .next("gpu ls --provider gcp"));
        }
        let next = format!("gpu expire {} show", id.cli());
        let args = self.create_args(ctx, id, deadline)?;
        let task_id = args
            .iter()
            .find(|s| s.starts_with("gpu-expire-"))
            .ok_or_else(gcp::bad_reply)?
            .clone();
        // Read back by the requested name, not a name supplied in an opaque reply.
        gcp::json(ctx, args, &next)?;
        let mut args = self.args("describe");
        args.push(task_id);
        let data = gcp::json(ctx, args, &next)?;
        let task = data.as_dict().ok_or_else(gcp::bad_reply)?;
        let http = task
            .get("httpRequest")
            .and_then(Value::as_dict)
            .ok_or_else(gcp::bad_reply)?;
        let oauth = http
            .get("oauthToken")
            .and_then(Value::as_dict)
            .ok_or_else(gcp::bad_reply)?;
        let when = text(task, "scheduleTime").and_then(|s| s.parse::<Timestamp>().ok());
        if text(http, "url") != Some(id.url().as_str())
            || text(http, "httpMethod") != Some("DELETE")
            || text(oauth, "serviceAccountEmail") != Some(&self.service_account)
            || when.map(|t| t.as_second()) != Some(deadline.as_second())
        {
            return Err(Error::msg(
                "TPU deletion task could not be verified; expiry is unverified",
            )
            .next(next));
        }
        Ok(())
    }
    fn own_task_name(&self, task: &Dict) -> bool {
        text(task, "name")
            .and_then(|n| n.strip_prefix(&format!("{}/tasks/", self.resource())))
            .is_some_and(|name| name.starts_with("gpu-expire-") && component(name))
    }
    fn tasks(&self, ctx: &Ctx, id: &TpuId) -> Result<Vec<Dict>> {
        let data = self.pending(ctx)?;
        let url = id.url();
        Ok(data
            .into_iter()
            .filter(|d| {
                let own_name = self.own_task_name(d);
                let target = d.get("httpRequest").and_then(Value::as_dict);
                own_name
                    && target.is_some_and(|h| {
                        text(h, "url") == Some(&url) && text(h, "httpMethod") == Some("DELETE")
                    })
            })
            .collect())
    }
    fn delete_tasks(&self, ctx: &Ctx, id: &TpuId, tasks: &[Dict]) -> Result<()> {
        for task in tasks {
            let name = text(task, "name")
                .and_then(|s| s.rsplit('/').next())
                .ok_or_else(gcp::bad_reply)?;
            if !component(name) {
                return Err(gcp::bad_reply());
            }
            let mut args = self.args("delete");
            args.push(name.into());
            gcp::run(ctx, &args, &format!("gpu expire {} show", id.cli()))?;
        }
        Ok(())
    }
    pub(crate) fn pending(&self, ctx: &Ctx) -> Result<Vec<Dict>> {
        crate::gcp::http::pages(
            ctx,
            &self.project,
            &format!(
                "https://cloudtasks.googleapis.com/v2/{}/tasks?responseView=BASIC",
                self.resource()
            ),
            "tasks",
            1000,
        )
    }
    fn target(&self, task: &Dict) -> Option<TpuId> {
        if !self.own_task_name(task) {
            return None;
        }
        let http = task.get("httpRequest")?.as_dict()?;
        if text(http, "httpMethod") != Some("DELETE")
            || http
                .get("oauthToken")
                .and_then(Value::as_dict)
                .and_then(|d| text(d, "serviceAccountEmail"))
                != Some(&self.service_account)
        {
            return None;
        }
        let resource = text(http, "url")?.strip_prefix("https://tpu.googleapis.com/v2/")?;
        TpuId::from_resource(resource).filter(|id| id.project == self.project)
    }
    pub(crate) fn reconcile_inventory(
        &self,
        ctx: &Ctx,
        nodes: &[Dict],
        tasks: Vec<Dict>,
    ) -> Result<()> {
        let present = nodes
            .iter()
            .filter_map(|n| text(n, "name"))
            .collect::<std::collections::HashSet<_>>();
        for task in tasks {
            let Some(id) = self.target(&task) else {
                continue;
            };
            if present.contains(id.resource().as_str()) {
                continue;
            }
            // An arming task can briefly precede its node. Never reap that window.
            let created = text(&task, "createTime")
                .and_then(|t| t.parse::<Timestamp>().ok())
                .map(|t| t.as_second())
                .or_else(|| {
                    text(&task, "name")?
                        .rsplit('/')
                        .next()?
                        .strip_prefix("gpu-expire-")?
                        .split('-')
                        .next()?
                        .parse::<i64>()
                        .ok()
                });
            if !created.is_some_and(|at| ctx.clock.now().as_second().saturating_sub(at) >= 60) {
                continue;
            }
            self.delete_tasks(ctx, &id, &[task])?;
        }
        Ok(())
    }
    /// Complete cross-location inventory proves absence without a GET per task.
    pub fn reconcile(&self, ctx: &Ctx) -> Result<()> {
        let (nodes, tasks) = std::thread::scope(|scope| {
            let pending = scope.spawn(|| self.pending(ctx));
            (
                gcp::all_nodes(ctx, &self.project),
                pending.join().unwrap_or_else(|_| Err(gcp::bad_reply())),
            )
        });
        self.reconcile_inventory(ctx, &nodes?, tasks?)
    }

    pub fn has_tasks(&self, ctx: &Ctx, id: &TpuId) -> Result<bool> {
        Ok(!self.tasks(ctx, id)?.is_empty())
    }

    pub fn cancel(&self, ctx: &Ctx, id: &TpuId) -> Result<()> {
        self.delete_tasks(ctx, id, &self.tasks(ctx, id)?)?;
        if !self.tasks(ctx, id)?.is_empty() {
            return Err(Error::msg("Expiry cancellation is unverified")
                .next(format!("gpu expire {} show", id.cli())));
        }
        Ok(())
    }
}

pub fn reconcile(ctx: &Ctx, cfg: &Dict) -> Result<()> {
    ctx.require_gcloud()?;
    Queue::load(&Settings::load(cfg)?)?.reconcile(ctx)
}

pub fn queue_for_node(cfg: &Dict, id: &TpuId, node: &Dict) -> Result<Queue> {
    let configured = Settings::load(cfg).and_then(|s| Queue::load(&s));
    if let Some(resource) = node
        .get("metadata")
        .and_then(Value::as_dict)
        .and_then(|m| text(m, "gpu-expiry-queue"))
    {
        let (project, location, name) = gcp::resource_parts(resource, "queues")
            .filter(|(project, _, _)| *project == id.project)
            .ok_or_else(gcp::bad_reply)?;
        let sa = configured
            .ok()
            .filter(|q| q.resource() == resource)
            .map(|q| q.service_account)
            .unwrap_or_default();
        return Ok(Queue {
            project: project.into(),
            location: location.into(),
            name: name.into(),
            service_account: sa,
        });
    }
    let queue = configured?;
    if queue.project != id.project {
        return Err(Error::msg("Expiry config belongs to a different project")
            .next("gpu ls --provider gcp"));
    }
    Ok(queue)
}

/// Whether a deletion task is pending for `id`.
pub fn armed(ctx: &Ctx, cfg: &Dict, id: &TpuId) -> Result<bool> {
    let node = gcp::find_node(&gcp::nodes(ctx, &id.project, &id.zone)?, &id.name)
        .cloned()
        .unwrap_or_default();
    queue_for_node(cfg, id, &node)?.has_tasks(ctx, id)
}

pub fn manage(
    ctx: &Ctx,
    cfg: &Dict,
    id: &TpuId,
    action: &str,
    deadline: Option<Timestamp>,
) -> Result<()> {
    if let Some(deadline) = deadline {
        validate_deadline(ctx, deadline)?;
    }
    // Listing also lets cancel clean up tasks left by a failed/expired create.
    // Authentication failures are not confused with absence.
    let node = gcp::find_node(&gcp::nodes(ctx, &id.project, &id.zone)?, &id.name).cloned();
    if node.is_none() && action == "arm" {
        return Err(Error::not_found(&id.cli()));
    }
    let absent = node.is_none();
    // Metadata pins the queue even if local defaults have subsequently changed.
    let node = node.unwrap_or_default();
    if node.get("queuedResource").is_some_and(crate::pyfmt::truthy) {
        return Err(Error::msg("Queued-resource expiry is not supported; deleting only the node would leak its request").next("gpu ls --provider gcp"));
    }
    let queue = queue_for_node(cfg, id, &node)?;
    // A known orphan expiry task may still be inspected/cancelled after its
    // node disappeared. An ID with neither a node nor a task is not a success.
    if absent && queue.tasks(ctx, id)?.is_empty() {
        return Err(Error::not_found(&id.cli()));
    }
    reconcile(ctx, cfg)?;
    if action == "cancel" {
        queue.cancel(ctx, id)?;
        crate::expiry::report(ctx, Value::str("cancelled"), Vec::new());
        return ctx.println(&format!("Expiry cancelled; billing continues. In-flight deletion cannot be stopped.\nNext: gpu down {}", id.cli()));
    }
    queue.check(ctx)?;
    let old = queue.tasks(ctx, id)?;
    if action == "show" {
        if old.is_empty() {
            return Err(
                Error::msg("No pending task; expiry unarmed or already dispatched")
                    .next("gpu ls --provider gcp"),
            );
        }
        crate::expiry::report(
            ctx,
            Value::str("armed"),
            old.iter()
                .map(|task| text(task, "scheduleTime").map_or(Value::Null, Value::str))
                .collect(),
        );
        for task in old {
            ctx.println(&format!(
                "TPU {}: deletion dispatch/retry scheduled for {}; queue is RUNNING (API/IAM health is not guaranteed).",
                id.cli(),
                text(&task, "scheduleTime").unwrap_or("unknown")
            ))?;
        }
        return Ok(());
    }
    let deadline = deadline.ok_or_else(gcp::bad_reply)?;
    // Create and verify replacement first. If interrupted, the earlier of both
    // deadlines wins; never silently lose the only safety task.
    queue.arm(ctx, id, deadline)?;
    crate::expiry::report(
        ctx,
        Value::str("armed"),
        vec![Value::str(deadline.to_string())],
    );
    queue.delete_tasks(ctx, id, &old).map_err(|_| {
        Error::msg(
            "New expiry is armed but old tasks remain; an earlier deadline may still destroy this TPU",
        )
        .next(format!("gpu expire {} show", id.cli()))
    })?;
    ctx.println(&format!(
        "Delete {} at {deadline}; works offline.\nNext: gpu expire {} show",
        id.cli(),
        id.cli()
    ))
}
