//! Experimental single-MI300X workspaces. Provider CLIs own authentication.
//! No guest credentials, implicit fallback, or unverified expiry promises.
mod digitalocean;
mod runpod;
#[cfg(test)]
mod tests;

use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::DirBuilderExt;

use jiff::Timestamp;

use crate::cli::{RunFlags, UpFlags};
use crate::ctx::Ctx;
use crate::error::{Error, ExitCode, Result};
use crate::process::Cmd;
use crate::provider::{Instance, Provider};
use crate::pyjson::{Dict, Value};
use crate::run::{RunBackend, RunPlan};

pub const EXPIRY: &str = "AMD provider expiry is not validated. Live attached runs and --expire are disabled; no resource created. Use a manual workspace and explicitly destroy it with gpu down ID --yes. Billing continues until deletion.";

fn invalid() -> Error {
    Error::msg("Invalid provider response; no raw response displayed. Check provider CLI version and authentication.")
}

fn cli(provider: Provider, args: &[&str]) -> Cmd {
    let binary = if provider == Provider::Runpod {
        "runpodctl"
    } else {
        "doctl"
    };
    let mut cmd = Cmd::new(&[binary]);
    cmd.argv.extend(args.iter().map(|s| s.to_string()));
    cmd.argv.extend(["--output".into(), "json".into()]);
    cmd.timeout(Some(60.0))
}

/// Never include dependency stdout/stderr: even successful reads can carry keys.
fn call(ctx: &Ctx, cmd: &Cmd) -> Result<Value> {
    let next = format!(
        "gpu setup --provider {}",
        if cmd.program() == "runpodctl" {
            "runpod"
        } else {
            "do"
        }
    );
    let out = ctx.run(cmd).map_err(|error| match error {
        crate::process::RunError::NotFound => Error::status(
            ExitCode::NotSetup,
            "Install the provider CLI and configure its authentication.",
        )
        .next(&next),
        crate::process::RunError::TimedOut => Error::status(
            ExitCode::CommandTimeout,
            "Provider request timed out; for a create, inspect gpu ls before retrying.",
        ),
        _ => Error::unavailable(
            "Provider CLI did not complete; for a create, inspect gpu ls before retrying.",
        ),
    })?;
    if out.code != 0 {
        let code = crate::pyjson::loads(&out.stdout).ok().and_then(|value| {
            value
                .get("error")
                .and_then(|v| v.get("code"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        return Err(match code.as_deref() {
            Some("unauthorized" | "forbidden" | "auth_error") => Error::status(ExitCode::AuthExpired, "Provider authentication/permissions failed.").next(next),
            _ => Error::msg("Provider CLI request failed; check authentication, quota and capacity. For a create, inspect gpu ls before retrying; it may already be billing.").next(next),
        });
    }
    crate::pyjson::loads(&out.stdout).map_err(|_| invalid())
}

fn rows(value: &Value) -> Result<&[Value]> {
    value.as_list().map(Vec::as_slice).ok_or_else(invalid)
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(invalid)
}
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Int(v) => Some(*v as f64),
        Value::Float(v) => Some(*v),
        _ => None,
    }
}
fn rate(value: &Value) -> Result<f64> {
    number(value)
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(invalid)
}
fn component(s: &str) -> bool {
    s.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
        && s.len() <= 128
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
fn public_ip(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_broadcast())
        }
        Ok(IpAddr::V6(ip)) => {
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unique_local()
                || ip.is_unicast_link_local())
        }
        Err(_) => false,
    }
}
fn setting<'a>(cfg: &'a Dict, provider: Provider, key: &str) -> Option<&'a str> {
    cfg.get(provider.key())
        .and_then(|v| v.get(key))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

pub fn configured(ctx: &Ctx, provider: Provider) -> bool {
    match provider {
        Provider::Runpod => {
            ctx.env.get("RUNPOD_API_KEY").is_some()
                || ctx.env.home().join(".runpod/config.toml").is_file()
        }
        Provider::DigitalOcean => {
            ctx.env.get("DIGITALOCEAN_ACCESS_TOKEN").is_some()
                || ctx.env.home().join(".config/doctl/config.yaml").is_file()
                || ctx
                    .env
                    .home()
                    .join("Library/Application Support/doctl/config.yaml")
                    .is_file()
                || ctx.env.get("XDG_CONFIG_HOME").is_some_and(|path| {
                    std::path::Path::new(path)
                        .join("doctl/config.yaml")
                        .is_file()
                })
        }
        _ => false,
    }
}

fn preflight(ctx: &Ctx, provider: Provider) -> Result<Vec<Value>> {
    let binary = if provider == Provider::Runpod {
        "runpodctl"
    } else {
        "doctl"
    };
    let output = ctx
        .run(&Cmd::new(&[binary, "version"]).timeout(Some(15.0)))
        .map_err(|_| {
            Error::status(
                ExitCode::NotSetup,
                format!("Install {binary}; configure authentication using its own CLI."),
            )
        })?;
    let minimum = if provider == Provider::Runpod {
        (3, 1, 1)
    } else {
        (2, 4, 0)
    };
    let version = output
        .stdout
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find_map(|word| {
            let parts: Vec<_> = word.split('.').collect();
            if parts.len() != 3 {
                return None;
            }
            Some((
                parts[0].parse::<u64>().ok()?,
                parts[1].parse::<u64>().ok()?,
                parts[2].parse::<u64>().ok()?,
            ))
        });
    if output.code != 0 || version.is_none_or(|v| v < minimum) {
        return Err(Error::status(
            ExitCode::NotSetup,
            format!("Use {binary} >= {}.{}.{}.", minimum.0, minimum.1, minimum.2),
        ));
    }
    list(ctx, provider)?;
    if !ctx.env.home().join(".ssh/id_ed25519.pub").is_file() {
        return Err(Error::status(ExitCode::NotSetup, "Create ~/.ssh/id_ed25519 and register its public key using the provider CLI; gpu never reads private keys."));
    }
    let keys = call(
        ctx,
        &cli(
            provider,
            if provider == Provider::Runpod {
                &["ssh", "list-keys"]
            } else {
                &["compute", "ssh-key", "list"]
            },
        ),
    )?;
    let public = std::fs::read_to_string(ctx.env.home().join(".ssh/id_ed25519.pub"))?;
    let canonical = |s: &str| s.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    let expected = canonical(&public);
    let keys = if provider == Provider::Runpod {
        rows(keys.get("keys").ok_or_else(invalid)?)?
    } else {
        rows(&keys)?
    };
    let field = if provider == Provider::Runpod {
        "key"
    } else {
        "public_key"
    };
    let matched = keys
        .iter()
        .filter(|key| {
            key.get(field)
                .and_then(Value::as_str)
                .is_some_and(|s| canonical(s) == expected)
        })
        .cloned()
        .collect::<Vec<_>>();
    if expected.is_empty() || matched.is_empty() {
        return Err(Error::status(
            ExitCode::NotSetup,
            "Register ~/.ssh/id_ed25519.pub using the provider CLI before launching.",
        ));
    }
    Ok(matched)
}

pub fn check(ctx: &Ctx, provider: Provider) -> Result<()> {
    preflight(ctx, provider)?;
    ctx.result("ready", Value::Bool(true));
    ctx.result("expiry_supported", Value::Bool(false));
    ctx.println(&format!(
        "{}: CLI/auth/key checked; real rentals UNTESTED. Next: gpu offers --provider {}",
        provider.label(),
        provider.key()
    ))
}

pub fn list(ctx: &Ctx, provider: Provider) -> Result<Vec<Dict>> {
    match provider {
        Provider::Runpod => runpod::list(ctx),
        Provider::DigitalOcean => digitalocean::list(ctx),
        _ => Err(Error::usage("Not an AMD provider")),
    }
}

#[derive(Debug)]
pub struct Quote {
    rate: f64,
    available: bool,
    location: String,
}
fn catalog(ctx: &Ctx, provider: Provider) -> Result<Vec<Quote>> {
    match provider {
        Provider::Runpod => runpod::catalog(ctx),
        Provider::DigitalOcean => digitalocean::catalog(ctx),
        _ => Err(Error::usage("Not an AMD provider")),
    }
}
pub fn offers(ctx: &Ctx, provider: Provider) -> Result<()> {
    let quotes = catalog(ctx, provider)?;
    ctx.result("provider", Value::str(provider.key()));
    ctx.result(
        "offers",
        Value::List(
            quotes
                .iter()
                .map(|q| {
                    let mut row = Dict::new();
                    row.insert("gpu", Value::str("MI300X"));
                    row.insert("location", Value::str(&q.location));
                    row.insert("compute_per_hour", Value::Float(q.rate));
                    row.insert("stock_hint", Value::Bool(q.available));
                    Value::Dict(row)
                })
                .collect(),
        ),
    );
    for q in &quotes {
        ctx.println(&format!(
            "{} MI300X {} ${}/hr compute; stock hint: {} (allocation unverified)",
            provider.label(),
            q.location,
            crate::pyfmt::fixed(q.rate, 3),
            if q.available {
                "available"
            } else {
                "unavailable"
            }
        ))?;
    }
    if quotes.is_empty() {
        ctx.println("0 single-MI300X catalog offers found.")?;
    }
    ctx.println(&format!(
        "Next: gpu up --provider {} --gpu MI300X --no-repo --dry-run",
        provider.key()
    ))
}

pub fn validate_up(flags: &UpFlags) -> Result<()> {
    crate::launch::check_max_price(flags.max_price)?;
    if flags.expire.is_some() {
        return Err(Error::usage(EXPIRY));
    }
    if flags.tpu_flags()
        || flags.interruptible
        || flags.offer.is_some()
        || flags.reliability.is_some()
        || flags.unverified
    {
        return Err(Error::usage("AMD providers do not support TPU, spot/interruptible, Vast offer or reliability flags."));
    }
    validate_gpu(flags.gpu.as_deref(), flags.num_gpus)?;
    if flags.mode.as_deref().is_some_and(|m| {
        m != if flags.target() == Provider::Runpod {
            "container"
        } else {
            "vm"
        }
    }) {
        return Err(Error::usage(
            "Runpod requires container mode; DigitalOcean requires vm mode.",
        ));
    }
    if flags.target() == Provider::DigitalOcean && flags.disk.is_some() {
        return Err(Error::usage(
            "DigitalOcean boot disk is fixed by the size; --disk is unsupported.",
        ));
    }
    Ok(())
}
fn validate_gpu(gpu: Option<&str>, count: i64) -> Result<()> {
    if gpu.is_none_or(|g| !g.eq_ignore_ascii_case("MI300X")) || count != 1 {
        return Err(Error::usage(
            "AMD providers support only --gpu MI300X --num-gpus 1.",
        ));
    }
    Ok(())
}

struct Plan {
    command: Cmd,
    rate: f64,
    // Temporary user-data must remain alive through create.
    _data: Option<tempfile::NamedTempFile>,
    ssh_setup: Option<String>,
}
fn plan(
    ctx: &Ctx,
    provider: Provider,
    cfg: &Dict,
    disk: Option<i64>,
    image: Option<&str>,
    label: &str,
    setup: &str,
) -> Result<Plan> {
    if provider == Provider::Runpod {
        runpod::plan(ctx, cfg, disk, image, label, setup)
    } else {
        digitalocean::plan(ctx, cfg, image, label, setup)
    }
}
fn request(plan: &Plan) -> Value {
    let mut result = Dict::new();
    result.insert(
        "argv",
        Value::List(plan.command.argv.iter().map(Value::str).collect()),
    );
    result.insert("expiry_supported", Value::Bool(false));
    if let Some(data) = &plan._data {
        if let Ok(script) = std::fs::read_to_string(data.path()) {
            result.insert("user_data", Value::str(script));
        }
    }
    if let Some(script) = &plan.ssh_setup {
        result.insert("ssh_setup", Value::str(script));
    }
    Value::Dict(result)
}

pub fn up(ctx: &Ctx, flags: &UpFlags, cfg: &Dict) -> Result<crate::launch::Launched> {
    validate_up(flags)?;
    let provider = flags.target();
    if !flags.dry_run {
        let matched = preflight(ctx, provider)?;
        if provider == Provider::DigitalOcean {
            let selected = setting(cfg, provider, "ssh_keys")
                .unwrap_or("")
                .split(',')
                .collect::<Vec<_>>();
            if !matched.iter().any(|key| {
                let id = key.get("id").map(crate::pyfmt::py_str).unwrap_or_default();
                selected.contains(&id.as_str())
                    || key
                        .get("fingerprint")
                        .and_then(Value::as_str)
                        .is_some_and(|fp| selected.contains(&fp))
            }) {
                return Err(Error::status(
                    ExitCode::NotSetup,
                    "do.ssh_keys must include the registered key for ~/.ssh/id_ed25519.pub.",
                ));
            }
        }
    }
    crate::launch::planned_result(ctx, provider, None, None, None);
    let repo = match &flags.repo {
        Some(repo) => Some(
            crate::repos::resolve_repo(
                repo,
                cfg.get("owner").and_then(Value::as_str).unwrap_or(""),
            )?
            .0,
        ),
        None if flags.no_repo => None,
        None => return Err(Error::usage("Choose --repo URL or --no-repo.")),
    };
    let setup = bootstrap(repo.as_deref());
    let plan = plan(
        ctx,
        provider,
        cfg,
        flags.disk,
        None,
        &crate::owner::tag("gpu-cli-workspace", cfg),
        &setup,
    )?;
    if flags.max_price.is_some_and(|cap| plan.rate > cap) {
        return Err(Error::status(
            ExitCode::OverMaxPrice,
            "Total hourly rate exceeds --max-price; no resource created.",
        ));
    }
    // No deadline means an opt-in spend cap cannot be satisfied.
    crate::guardrails::spending(ctx, cfg, Some(plan.rate), None)?;
    ctx.result("price_per_hour", Value::Float(plan.rate));
    ctx.result("request", request(&plan));
    ctx.result("expiry_supported", Value::Bool(false));
    ctx.println(&format!("Hourly estimate ${} (Runpod includes container disk at $0.10/GB-month, 730h/month; DO includes boot disk). No lifetime limit.", crate::pyfmt::fixed(plan.rate, 3)))?;
    ctx.eprintln("UNTESTED rental backend. No auto-destroy: billing continues until gpu down ID --yes. Independent volumes/snapshots are not deleted.")?;
    if flags.dry_run {
        ctx.println(&format!(
            "DRY RUN: {}. No resource created.",
            crate::pyjson::dumps(&request(&plan), None)
        ))?;
        return Ok(Default::default());
    }
    crate::owner::warn_duplicate(ctx, cfg, provider)?;
    if !flags.yes
        && !crate::prompt::confirm(
            ctx,
            Some("Rent WITHOUT auto-destroy until manually deleted?"),
        )?
    {
        return Err(Error::Cancelled(130));
    }
    // Never retry a create, including an accepted but unreadable response.
    ctx.set_interrupt_notice(Some(format!("{} create may be pending and billing; inspect gpu ls --provider {} before retrying. No expiry is armed.", provider.label(), provider.key())));
    ctx.result("created", Value::Null);
    let uncertain = || {
        Error::msg(format!("Create outcome uncertain; a resource may be billing with NO expiry. Inspect gpu ls --provider {} before retrying.", provider.key()))
    };
    let reply = call(ctx, &plan.command).map_err(|_| uncertain())?;
    let id = (if provider == Provider::Runpod {
        runpod::created(&reply)
    } else {
        digitalocean::created(&reply, setting(cfg, provider, "region").unwrap_or("atl1"))
    })
    .map_err(|_| uncertain())?;
    ctx.result("id", Value::str(&id));
    ctx.result("created", Value::Bool(true));
    ctx.set_interrupt_notice(Some(format!(
        "Instance {id} may still be billing. No expiry is armed. Next: gpu down {id} --yes"
    )));
    ctx.println(&format!(
        "Created {id}; NO expiry. Next: gpu down {id} --yes"
    ))?;
    if let Some(script) = &plan.ssh_setup {
        crate::atomic_file::update(&pending_setup(ctx, &Id::parse(&id)?), |_| {
            Ok(script.as_bytes().to_vec())
        })
        .map_err(|e| e.next(format!("gpu down {id} --yes")))?;
        ctx.println(&format!(
            "Workspace setup starts on the first SSH connection. Next: gpu wait {id} --wait-setup"
        ))?;
    }
    Ok(crate::launch::Launched {
        id: Some(id),
        ready_alias: None,
    })
}

fn pending_setup(ctx: &Ctx, id: &Id) -> std::path::PathBuf {
    ctx.env
        .xdg("XDG_STATE_HOME", ".local/state")
        .join("gpu/amd-setup")
        .join(id.alias())
}

/// Runs setup detached over SSH so the image's own start command keeps sshd running.
/// The remote marker makes this idempotent; the local file only records that it is owed.
fn resume_setup(ctx: &Ctx, id: &Id) -> Result<()> {
    let path = pending_setup(ctx, id);
    let script = match std::fs::read_to_string(&path) {
        Ok(script) => script,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let remote = format!(
        "mkdir -p /workspace && if (set -C; printf pending >/workspace/.gpu-setup) 2>/dev/null; then nohup bash -c {} >/workspace/.gpu-setup.log 2>&1 </dev/null & fi",
        crate::pyfmt::shell_quote(&script)
    );
    let cmd = crate::ssh::command(&["ssh", "-T"], true, &id.alias(), &remote).timeout(Some(30.0));
    match ctx.run(&cmd) {
        Ok(out) if out.code == 0 => remove_pending_setup(ctx, id),
        _ => Err(Error::msg(
            "Workspace setup could not be started over SSH; it starts on the next connection. The instance is billing.",
        )
        .next(format!("gpu wait {} --wait-setup", id.qualified()))),
    }
}
fn remove_pending_setup(ctx: &Ctx, id: &Id) -> Result<()> {
    match std::fs::remove_file(pending_setup(ctx, id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn bootstrap(repo: Option<&str>) -> String {
    let clone = repo
        .map(|url| {
            format!(
                "GIT_TERMINAL_PROMPT=0 GIT_SSH_COMMAND='ssh -oBatchMode=yes' git clone -- {} /workspace/repo\n",
                crate::pyfmt::shell_quote(url)
            )
        })
        .unwrap_or_default();
    format!("#!/bin/bash\nset -e\nmkdir -p /workspace\nprintf pending >/workspace/.gpu-setup\ntrap 'printf failed >/workspace/.gpu-setup' ERR\n{clone}printf done >/workspace/.gpu-setup\n")
}

#[derive(Debug)]
pub struct Id {
    provider: Provider,
    raw: String,
    region: Option<String>,
}
impl Id {
    pub fn parse(id: &str) -> Result<Self> {
        let parts: Vec<_> = id.split(':').collect();
        match parts.as_slice() {
            ["runpod", raw] if component(raw) => Ok(Self {
                provider: Provider::Runpod,
                raw: (*raw).into(),
                region: None,
            }),
            ["do", region, raw]
                if component(region)
                    && raw
                        .parse::<u64>()
                        .is_ok_and(|id| id > 0 && id.to_string() == *raw) =>
            {
                Ok(Self {
                    provider: Provider::DigitalOcean,
                    raw: (*raw).into(),
                    region: Some((*region).into()),
                })
            }
            _ => Err(Error::usage(
                "Invalid AMD instance ID; use runpod:ID or do:REGION:NUMERIC_ID.",
            )),
        }
    }
    fn qualified(&self) -> String {
        match &self.region {
            Some(region) => format!("do:{region}:{}", self.raw),
            None => format!("runpod:{}", self.raw),
        }
    }
    fn alias(&self) -> String {
        self.qualified().replace(':', "-")
    }
    fn endpoint(&self, ctx: &Ctx, timeout: f64) -> Result<(String, u16)> {
        if self.provider == Provider::Runpod {
            runpod::endpoint(ctx, &self.raw, timeout)
        } else {
            digitalocean::endpoint(ctx, self, timeout)
        }
    }
}
impl Instance for Id {
    fn provider(&self) -> Provider {
        self.provider
    }
    fn expire(
        &self,
        _ctx: &Ctx,
        _cfg: &Dict,
        _action: &str,
        _deadline: Option<Timestamp>,
    ) -> Result<()> {
        Err(Error::usage(EXPIRY))
    }
    fn wait_ssh(&self, ctx: &Ctx, timeout: f64) -> Result<String> {
        let poll = ctx.polling(timeout, 2.0);
        loop {
            ctx.check_cancelled()?;
            if poll.remaining() <= 0.0 {
                break;
            }
            match self.endpoint(ctx, poll.remaining().min(15.0)) {
                Ok((host, port)) => {
                    write_alias(ctx, self, &host, port)?;
                    let cmd = crate::ssh::command(&["ssh", "-T"], true, &self.alias(), "true")
                        .timeout(Some(poll.remaining().clamp(0.1, 11.0)));
                    if ctx.run(&cmd).is_ok_and(|out| out.code == 0) {
                        resume_setup(ctx, self)?;
                        return Ok(self.alias());
                    }
                }
                Err(Error::Status(ExitCode::WaitTimeout | ExitCode::CommandTimeout, _)) => {}
                Err(error) if error.retryable() => {}
                Err(error) => return Err(error),
            }
            if poll.remaining() <= 0.0 {
                break;
            }
            poll.pause();
        }
        let id = self.qualified();
        if pending_setup(ctx, self).is_file() {
            return Err(Error::status(ExitCode::WaitTimeout, format!("Direct SSH is not ready; the instance is still starting and billing. Workspace setup has not started; it starts on the next connection. To stop billing instead: gpu down {id} --yes")).next(format!("gpu wait {id} --wait-setup")));
        }
        Err(Error::status(ExitCode::WaitTimeout, "Direct SSH is not ready; instance may still be billing. Runpod proxy SSH is not a copy/editor fallback.").next(format!("gpu down {id} --yes")))
    }
    fn ssh(&self, ctx: &Ctx, exec: bool) -> Result<()> {
        let alias = self.wait_ssh(ctx, 120.0)?;
        let argv = vec!["ssh".into(), alias];
        if exec {
            return Err(Error::msg(ctx.exec(&argv)));
        }
        ctx.result("provider", Value::str(self.provider.key()));
        ctx.result("ssh_alias", Value::str(self.alias()));
        ctx.result("ready", Value::Bool(true));
        ctx.result("ssh_command", Value::str(crate::pyfmt::shell_join(&argv)));
        ctx.println(&crate::pyfmt::shell_join(&argv))
    }
    fn edit(&self, ctx: &Ctx, editor: Option<&str>, nvim_config: Option<&Value>) -> Result<()> {
        crate::editors::open(ctx, &self.qualified(), editor, nvim_config, |_| {
            let alias = self.wait_ssh(ctx, crate::editors::EDITOR_WAIT)?;
            Ok(crate::editors::Target {
                shown: alias.clone(),
                alias,
                directory: "/workspace".into(),
            })
        })
    }
    fn stop(&self, ctx: &Ctx, _cfg: &Dict) -> Result<()> {
        ctx.result("provider", Value::str(self.provider.key()));
        ctx.result("action", Value::str("stop"));
        ctx.result("billing_continues", Value::Bool(true));
        if self.provider == Provider::Runpod {
            ctx.eprintln("Stopping loses the container filesystem; retained storage still bills. Use down to end billing.")?;
            call(ctx, &cli(self.provider, &["pod", "stop", &self.raw]))?;
        } else {
            // Do not silently power off a differently qualified resource.
            self.endpoint(ctx, 15.0)?;
            ctx.eprintln("Power-off continues FULL GPU billing. Use down to end billing.")?;
            call(
                ctx,
                &cli(
                    self.provider,
                    &["compute", "droplet-action", "power-off", &self.raw],
                ),
            )?;
        }
        Ok(())
    }
    fn down(&self, ctx: &Ctx, _cfg: &Dict, yes: bool) -> Result<()> {
        let id = self.qualified();
        ctx.result("provider", Value::str(self.provider.key()));
        ctx.result("id", Value::str(&id));
        ctx.result("destroyed", Value::Bool(false));
        if !yes && !crate::prompt::confirm(ctx, Some(&format!("Permanently destroy {id}?")))? {
            return Err(Error::Cancelled(130));
        }
        let present = |items: &[Dict]| -> Result<bool> {
            for item in items {
                if item.get("id").and_then(Value::as_str) == Some(id.as_str()) {
                    return Ok(true);
                }
                // Region qualifiers are a safety boundary, not a hint.
                if self.provider == Provider::DigitalOcean
                    && item
                        .get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.ends_with(&format!(":{}", self.raw)))
                {
                    return Err(Error::usage(
                        "DigitalOcean instance region does not match the ID.",
                    ));
                }
            }
            Ok(false)
        };
        if !present(&list(ctx, self.provider)?)? {
            remove_alias(ctx, self)?;
            remove_pending_setup(ctx, self)?;
            ctx.result("state", Value::str("already_absent"));
            return ctx.println(&format!("Instance {id} is already absent."));
        }
        let cmd = if self.provider == Provider::Runpod {
            cli(self.provider, &["pod", "delete", &self.raw])
        } else {
            cli(
                self.provider,
                &["compute", "droplet", "delete", &self.raw, "--force"],
            )
        };
        // A failed/ambiguous DELETE can still have taken effect. Only the
        // authenticated complete inventory read below determines success.
        let _ = call(ctx, &cmd);
        let poll = ctx.polling(30.0, 2.0);
        loop {
            if let Ok(items) = list(ctx, self.provider) {
                if !present(&items)? {
                    break;
                }
            }
            if poll.remaining() <= 0.0 {
                return Err(Error::status(
                    ExitCode::CleanupUnverified,
                    "Deletion unverified; billing may continue.",
                )
                .next(format!("gpu down {id} --yes")));
            }
            poll.pause();
        }
        ctx.result("destroyed", Value::Bool(true));
        ctx.result("state", Value::str("destroyed"));
        remove_alias(ctx, self)?;
        remove_pending_setup(ctx, self)?;
        ctx.println(&format!("Deleted {id} (verified absent). Independent volumes/snapshots/backups may still bill; inspect them in the provider account."))
    }
}

fn write_alias(ctx: &Ctx, id: &Id, host: &str, port: u16) -> Result<()> {
    // Only literal IPs, never cloud-supplied SSH snippets, users or options.
    if !public_ip(host) {
        return Err(invalid());
    }
    if port == 0 {
        return Err(invalid());
    }
    let ssh = ctx.env.home().join(".ssh");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(ssh.join("gpu_amd"))?;
    crate::atomic_file::update(&ssh.join("config"), |old| {
        let include = b"Include ~/.ssh/gpu_amd/*\n";
        if old
            .split(|&b| b == b'\n')
            .any(|line| line == &include[..include.len() - 1])
        {
            return Ok(old.to_vec());
        }
        Ok([include.as_slice(), old].concat())
    })?;
    let entry = format!("Host {}\n  HostName {host}\n  Port {port}\n  User root\n  IdentityFile ~/.ssh/id_ed25519\n  StrictHostKeyChecking accept-new\n  BatchMode yes\n{}", id.alias(), crate::ssh::config_timeout());
    crate::atomic_file::update(&ssh.join("gpu_amd").join(id.alias()), |_| {
        Ok(entry.as_bytes().to_vec())
    })
}
fn remove_alias(ctx: &Ctx, id: &Id) -> Result<()> {
    match std::fs::remove_file(ctx.env.home().join(".ssh/gpu_amd").join(id.alias())) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub struct Backend(pub Provider);
impl RunBackend for Backend {
    fn provider(&self) -> Provider {
        self.0
    }
    fn plan(
        &self,
        ctx: &Ctx,
        flags: &RunFlags,
        cfg: &Dict,
        _deadline: Timestamp,
        label: String,
    ) -> Result<RunPlan> {
        validate_run(flags)?;
        let plan = plan(
            ctx,
            self.0,
            cfg,
            flags.disk,
            flags.image.as_deref(),
            &label,
            &bootstrap(None),
        )?;
        if flags.max_price.is_some_and(|cap| plan.rate > cap) {
            return Err(Error::status(
                ExitCode::OverMaxPrice,
                "Total hourly rate exceeds --max-price.",
            ));
        }
        Ok(RunPlan {
            gpu: Value::str("MI300X"),
            rate: plan.rate,
            request: Ok(request(&plan)),
        })
    }
    fn launch(
        &mut self,
        _ctx: &Ctx,
        _request: &Value,
        _deadline: Timestamp,
        _timeout: f64,
        _created: &mut dyn FnMut(&str, Option<&str>) -> Result<()>,
    ) -> Result<String> {
        Err(Error::usage(EXPIRY).rejected())
    }
    fn remote(&self, _command: &str, _timeout: f64) -> Cmd {
        unreachable!("AMD live runs are disabled")
    }
    fn lost(&self, _ctx: &Ctx, _id: &str) -> bool {
        false
    }
    fn restore_expiry(&self, _ctx: &Ctx, _id: &str, _deadline: Timestamp) -> Result<()> {
        Err(Error::usage(EXPIRY))
    }
    fn hook(&self, _id: &str) -> String {
        String::new()
    }
    fn wait_for_startup(&self) -> bool {
        false
    }
    fn destroy(&self, _ctx: &Ctx, _id: &str) -> Result<()> {
        Err(Error::usage(EXPIRY))
    }
}
pub fn validate_run(flags: &RunFlags) -> Result<()> {
    if !flags.dry_run {
        return Err(Error::usage(EXPIRY));
    }
    validate_gpu(flags.gpu.as_deref(), flags.num_gpus)?;
    if flags.offer.is_some()
        || flags.reliability.is_some()
        || flags.unverified
        || flags.cpu.is_some()
        || flags.memory.is_some()
        || (flags.provider == Some(Provider::DigitalOcean) && flags.disk.is_some())
    {
        return Err(Error::usage(
            "Vast/Modal resource flags do not apply to this AMD provider.",
        ));
    }
    Ok(())
}
