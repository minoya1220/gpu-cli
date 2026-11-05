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

pub fn validate_deadline(ctx: &Ctx, at: Timestamp) -> Result<()> {
    let seconds = at.as_second() - ctx.clock.now().as_second();
    // Tasks permits 30 days ahead with 31-day retention from creation. Leave
    // two days for retries and a minute for submitting the create request.
    if !(60..=29 * 86400).contains(&seconds) {
        return Err(Error::msg("TPU expiry must be 1 minute to 29 days away, including selection/boot time. Next: gpu up --provider gcp --expire 2h"));
    }
    Ok(())
}
impl Queue {
    pub fn load(settings: &Settings) -> Result<Self> {
        let d = settings.expiry.as_ref().ok_or_else(|| Error::msg("Configure gcp.expiry (Cloud Tasks queue, location and service_account) before renting a TPU. Next: gpu up --provider gcp --help"))?;
        let get = |key| {
            text(d, key).filter(|s| component(s)).map(String::from).ok_or_else(|| Error::msg(format!("Set gcp.expiry.{key} in gpu/config.json. Next: gpu up --provider gcp --help")))
        };
        let service_account = text(d, "service_account").filter(|s| {
            let default_compute = s.strip_suffix("-compute@developer.gserviceaccount.com")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
            (default_compute || s.ends_with(&format!("@{}.iam.gserviceaccount.com", settings.project)))
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-@.".contains(&b))
        })
            .ok_or_else(|| Error::msg("Set gcp.expiry.service_account to a service account in the queue's project. Next: gpu up --provider gcp --help"))?.into();
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
            return Err(Error::msg(format!("Expiry queue must be RUNNING with unlimited retries, max backoff <=60s and no HTTP target override. Next: gcloud tasks queues describe {} --project={} --location={}", self.name, self.project, self.location)));
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
            return Err(Error::msg(format!(
                "Expiry deadline passed; no new deletion task created. Next: gpu expire {} 2h",
                id.cli()
            )));
        }
        if self.service_account.is_empty() {
            return Err(Error::msg("Configure the expiry service account for this project before re-arming. Next: gpu ls --provider gcp"));
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
            return Err(Error::msg(format!(
                "TPU deletion task could not be verified; expiry is unverified. Next: {next}"
            )));
        }
        Ok(())
    }
    fn own_task_name(&self, task: &Dict) -> bool {
        text(task, "name")
            .and_then(|n| n.strip_prefix(&format!("{}/tasks/", self.resource())))
            .is_some_and(|name| name.starts_with("gpu-expire-") && component(name))
    }
    fn tasks(&self, ctx: &Ctx, id: &TpuId) -> Result<Vec<Dict>> {
        let data = gcp::json(
            ctx,
            self.args("list"),
            &format!("gpu expire {} show", id.cli()),
        )?;
        let url = id.url();
        Ok(gcp::objects(data)?
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
    /// Opportunistic cleanup only for this queue's gpu-owned exact DELETEs.
    /// A failed presence check must never remove a safety task.
    pub fn reconcile(&self, ctx: &Ctx) -> Result<()> {
        let tasks = gcp::objects(gcp::json(ctx, self.args("list"), "gpu ls --provider gcp")?)?;
        for task in tasks {
            let own = self.own_task_name(&task);
            let Some(http) = task.get("httpRequest").and_then(Value::as_dict) else {
                continue;
            };
            let account = http
                .get("oauthToken")
                .and_then(Value::as_dict)
                .and_then(|d| text(d, "serviceAccountEmail"));
            if !own
                || account != Some(&self.service_account)
                || text(http, "httpMethod") != Some("DELETE")
            {
                continue;
            }
            let Some(path) = text(http, "url")
                .and_then(|u| u.strip_prefix("https://tpu.googleapis.com/v2/projects/"))
            else {
                continue;
            };
            let p: Vec<_> = path.split('/').collect();
            if p.len() != 5
                || p[0] != self.project
                || p[1] != "locations"
                || p[3] != "nodes"
                || ![p[0], p[2], p[4]].iter().all(|s| gcp::component(s))
            {
                continue;
            }
            let id = TpuId {
                project: p[0].into(),
                zone: p[2].into(),
                name: p[4].into(),
            };
            match gcp::absent(ctx, &id) {
                Ok(true) => self.delete_tasks(ctx, &id, &[task])?,
                Ok(false) => {}
                Err(_) => ctx.eprintln(&format!(
                    "Could not verify {}; kept its expiry task. Next: gpu expire {} show",
                    id.cli(),
                    id.cli()
                ))?,
            }
        }
        Ok(())
    }

    pub fn cancel(&self, ctx: &Ctx, id: &TpuId) -> Result<()> {
        self.delete_tasks(ctx, id, &self.tasks(ctx, id)?)?;
        if !self.tasks(ctx, id)?.is_empty() {
            return Err(Error::msg(format!(
                "Expiry cancellation is unverified. Next: gpu expire {} show",
                id.cli()
            )));
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
        let parts: Vec<_> = resource.split('/').collect();
        if parts.len() != 6
            || parts[0] != "projects"
            || parts[2] != "locations"
            || parts[4] != "queues"
            || ![parts[1], parts[3], parts[5]].iter().all(|s| component(s))
            || parts[1] != id.project
        {
            return Err(gcp::bad_reply());
        }
        let sa = configured
            .ok()
            .filter(|q| q.resource() == resource)
            .map(|q| q.service_account)
            .unwrap_or_default();
        return Ok(Queue {
            project: parts[1].into(),
            location: parts[3].into(),
            name: parts[5].into(),
            service_account: sa,
        });
    }
    let queue = configured?;
    if queue.project != id.project {
        return Err(Error::msg(
            "Expiry config belongs to a different project. Next: gpu ls --provider gcp",
        ));
    }
    Ok(queue)
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
    reconcile(ctx, cfg)?;
    // Listing also lets cancel clean up tasks left by a failed/expired create.
    // Authentication failures are not confused with absence.
    let node = gcp::nodes(ctx, &id.project, &id.zone)?
        .into_iter()
        .find(|d| text(d, "name").is_some_and(|n| n.rsplit('/').next() == Some(&id.name)));
    if node.is_none() && action == "arm" {
        return Err(Error::msg(
            "TPU was not found; no new deletion task created. Next: gpu ls --provider gcp",
        ));
    }
    // Metadata pins the queue even if local defaults have subsequently changed.
    let node = node.unwrap_or_default();
    if node.get("queuedResource").is_some_and(crate::pyfmt::truthy) {
        return Err(Error::msg("Queued-resource expiry is not supported; deleting only the node would leak its request. Next: gpu ls --provider gcp"));
    }
    let queue = queue_for_node(cfg, id, &node)?;
    if action == "cancel" {
        queue.cancel(ctx, id)?;
        return ctx.println(&format!("Cancelled TPU expiry; billing continues. An in-flight delete cannot be undone.\nNext: gpu down {}", id.cli()));
    }
    queue.check(ctx)?;
    let old = queue.tasks(ctx, id)?;
    if action == "show" {
        if old.is_empty() {
            return Err(Error::msg("No pending deletion task; expiry is NOT armed (it may already have dispatched). Next: gpu ls --provider gcp"));
        }
        for task in old {
            ctx.println(&format!("TPU {}: deletion dispatch/retry scheduled for {}; queue is RUNNING (API/IAM health is not guaranteed).", id.cli(), text(&task, "scheduleTime").unwrap_or("unknown")))?;
        }
        return Ok(());
    }
    let deadline = deadline.ok_or_else(gcp::bad_reply)?;
    // Create and verify replacement first. If interrupted, the earlier of both
    // deadlines wins; never silently lose the only safety task.
    queue.arm(ctx, id, deadline)?;
    queue.delete_tasks(ctx, id, &old).map_err(|_| Error::msg(format!("New expiry is armed but old tasks remain; an earlier deadline may still destroy this TPU. Next: gpu expire {} show", id.cli())))?;
    ctx.println(&format!("Scheduled TPU {} for deletion at {deadline}; laptop and VM may be offline.\nNext: gpu expire {} show", id.cli(), id.cli()))
}
