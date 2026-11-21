//! The provider model. Numeric IDs are Vast IDs; every other provider's IDs
//! carry its prefix. Short TPU names require a fresh, unambiguous
//! configured-project lookup.
use jiff::Timestamp;

use crate::ctx::Ctx;
use crate::error::{Error, ExitCode, Result};
use crate::pyjson::{Dict, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Vast,
    Gcp,
    Modal,
    Runpod,
    DigitalOcean,
}

impl Provider {
    /// Every provider, in listing and picker order.
    pub const ALL: [Provider; 5] = [
        Provider::Vast,
        Provider::Gcp,
        Provider::Modal,
        Provider::Runpod,
        Provider::DigitalOcean,
    ];

    pub fn amd(self) -> bool {
        matches!(self, Self::Runpod | Self::DigitalOcean)
    }

    /// The `--provider` value and the `provider` field of instance records.
    pub fn key(self) -> &'static str {
        match self {
            Provider::Vast => "vast",
            Provider::Gcp => "gcp",
            Provider::Modal => "modal",
            Provider::Runpod => "runpod",
            Provider::DigitalOcean => "do",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::Vast => "Vast.ai",
            Provider::Gcp => "Google Cloud",
            Provider::Modal => "Modal",
            Provider::Runpod => "Runpod",
            Provider::DigitalOcean => "DigitalOcean",
        }
    }

    pub fn from_key(key: &str) -> Option<Provider> {
        Provider::ALL
            .into_iter()
            .find(|provider| provider.key() == key)
    }

    /// The provider that produced an instance record (Vast when unmarked).
    pub fn of(item: &Dict) -> Provider {
        item.get("provider")
            .and_then(Value::as_str)
            .and_then(Provider::from_key)
            .unwrap_or(Provider::Vast)
    }

    /// The prefix of this provider's qualified instance IDs.
    fn id_prefix(self) -> Option<&'static str> {
        match self {
            Provider::Vast => None,
            Provider::Gcp => Some("gcp:"),
            Provider::Modal => Some("modal:"),
            Provider::Runpod => Some("runpod:"),
            Provider::DigitalOcean => Some("do:"),
        }
    }

    /// A clap parser for `--provider` that accepts only `accepted`.
    pub(crate) fn parser(
        accepted: &'static [Provider],
    ) -> impl clap::builder::TypedValueParser<Value = Provider> {
        use clap::builder::TypedValueParser;
        clap::builder::PossibleValuesParser::new(accepted.iter().map(|provider| provider.key()))
            .map(|key| Provider::from_key(&key).expect("clap accepted a provider key"))
    }

    /// Whether an unfiltered `gpu ls` and the instance picker include this provider.
    fn listed(self, ctx: &Ctx, cfg: &Dict) -> bool {
        match self {
            Provider::Vast => true,
            Provider::Gcp => crate::gcp::setup::gcp_ready(cfg),
            Provider::Modal => crate::modal::configured(ctx),
            provider if provider.amd() => crate::amd::configured(ctx, provider),
            _ => false,
        }
    }

    /// Live instances, each marked with its `provider` key.
    fn instances(self, ctx: &Ctx, cfg: &Dict) -> Result<Vec<Dict>> {
        match self {
            Provider::Vast => {
                let mut items = crate::vast::list_instances(ctx, None)?;
                for item in &mut items {
                    item.insert("provider", Value::str(self.key()));
                }
                Ok(items)
            }
            Provider::Gcp => {
                ctx.require_gcloud()?;
                crate::gcp::list(ctx, &crate::gcp::Settings::load(cfg)?)
            }
            Provider::Modal => crate::modal::list(ctx),
            provider => crate::amd::list(ctx, provider),
        }
    }
}

/// Whether `id` carries a provider prefix (Vast IDs never do).
pub(crate) fn qualified(id: &str) -> bool {
    Provider::ALL
        .iter()
        .filter_map(|provider| provider.id_prefix())
        .any(|prefix| id.starts_with(prefix))
}

/// Operations on one provider's instance. A provider implements what it
/// supports; every other command reports the provider's unsupported error.
pub(crate) trait Instance {
    fn provider(&self) -> Provider;

    fn unsupported(&self) -> Error {
        Error::usage(format!(
            "{} does not support this command",
            self.provider().label()
        ))
        .next(format!("gpu ls --provider {}", self.provider().key()))
    }

    /// Wait until SSH answers; the managed SSH alias.
    fn wait_ssh(&self, _ctx: &Ctx, _timeout: f64) -> Result<String> {
        Err(self.unsupported())
    }

    fn expire(
        &self,
        _ctx: &Ctx,
        _cfg: &Dict,
        _action: &str,
        _deadline: Option<Timestamp>,
    ) -> Result<()> {
        Err(self.unsupported())
    }

    /// Whether an auto-destroy deadline is set; `alias` is its SSH alias.
    fn expiry_armed(&self, _ctx: &Ctx, _cfg: &Dict, _alias: &str) -> Result<bool> {
        Ok(false)
    }

    fn edit(&self, _ctx: &Ctx, _editor: Option<&str>, _nvim_config: Option<&Value>) -> Result<()> {
        Err(self.unsupported())
    }

    fn ssh(&self, _ctx: &Ctx, _exec: bool) -> Result<()> {
        Err(self.unsupported())
    }

    fn stop(&self, _ctx: &Ctx, _cfg: &Dict) -> Result<()> {
        Err(self.unsupported())
    }

    fn down(&self, ctx: &Ctx, cfg: &Dict, yes: bool) -> Result<()>;
}

/// A numeric Vast instance ID.
pub(crate) struct VastId(pub String);

impl Instance for VastId {
    fn provider(&self) -> Provider {
        Provider::Vast
    }
    fn wait_ssh(&self, ctx: &Ctx, timeout: f64) -> Result<String> {
        crate::vast::instance::wait_for_ssh(ctx, &self.0, timeout).map(|(alias, _)| alias)
    }
    fn expire(
        &self,
        ctx: &Ctx,
        _cfg: &Dict,
        action: &str,
        deadline: Option<Timestamp>,
    ) -> Result<()> {
        crate::vast::instance::expire_instance(ctx, &self.0, action, deadline).map(drop)
    }
    fn expiry_armed(&self, ctx: &Ctx, _cfg: &Dict, alias: &str) -> Result<bool> {
        let reply = crate::vast::expiry::manage_expiry(ctx, alias, &self.0, "show", None)?;
        Ok(matches!(
            reply.get("status").and_then(Value::as_str),
            Some("armed" | "retrying")
        ))
    }
    fn edit(&self, ctx: &Ctx, editor: Option<&str>, nvim_config: Option<&Value>) -> Result<()> {
        crate::editors::open(ctx, &self.0, editor, nvim_config, |_| {
            let (alias, directory) =
                crate::vast::instance::wait_for_ssh(ctx, &self.0, crate::editors::EDITOR_WAIT)?;
            Ok(crate::editors::Target {
                shown: alias.clone(),
                alias,
                directory,
            })
        })
    }
    fn ssh(&self, ctx: &Ctx, exec: bool) -> Result<()> {
        crate::vast::instance::ssh(ctx, &self.0, exec)
    }
    fn stop(&self, ctx: &Ctx, _cfg: &Dict) -> Result<()> {
        crate::vast::instance::lifecycle(ctx, &self.0, false, false)
    }
    fn down(&self, ctx: &Ctx, _cfg: &Dict, yes: bool) -> Result<()> {
        crate::vast::instance::lifecycle(ctx, &self.0, true, yes)
    }
}

impl Instance for crate::gcp::TpuId {
    fn provider(&self) -> Provider {
        Provider::Gcp
    }
    fn wait_ssh(&self, ctx: &Ctx, timeout: f64) -> Result<String> {
        crate::gcp::ssh::ready_alias(ctx, self, timeout)
    }
    fn expire(
        &self,
        ctx: &Ctx,
        cfg: &Dict,
        action: &str,
        deadline: Option<Timestamp>,
    ) -> Result<()> {
        crate::gcp::expiry::manage(ctx, cfg, self, action, deadline)
    }
    fn expiry_armed(&self, ctx: &Ctx, cfg: &Dict, _alias: &str) -> Result<bool> {
        crate::gcp::expiry::armed(ctx, cfg, self)
    }
    fn edit(&self, ctx: &Ctx, editor: Option<&str>, nvim_config: Option<&Value>) -> Result<()> {
        ctx.require_gcloud()?;
        crate::editors::open(ctx, &self.cli(), editor, nvim_config, |editor| {
            crate::gcp::ssh::editor_target(ctx, self, editor)
        })
    }
    fn ssh(&self, ctx: &Ctx, exec: bool) -> Result<()> {
        crate::gcp::ssh::ssh(ctx, self, exec)
    }
    fn stop(&self, ctx: &Ctx, cfg: &Dict) -> Result<()> {
        crate::gcp::lifecycle(ctx, self, cfg, false, false)
    }
    fn down(&self, ctx: &Ctx, cfg: &Dict, yes: bool) -> Result<()> {
        crate::gcp::lifecycle(ctx, self, cfg, true, yes)
    }
}

/// A Modal sandbox, `modal:sb-...`.
pub(crate) struct SandboxId(pub String);

impl Instance for SandboxId {
    fn provider(&self) -> Provider {
        Provider::Modal
    }
    fn unsupported(&self) -> Error {
        crate::modal::unsupported()
    }
    fn ssh(&self, ctx: &Ctx, exec: bool) -> Result<()> {
        crate::modal::ssh(ctx, &self.0, exec)
    }
    fn down(&self, ctx: &Ctx, _cfg: &Dict, yes: bool) -> Result<()> {
        crate::modal::down(ctx, &self.0, yes)
    }
}

pub(crate) struct VolumeId(pub String);
impl Instance for VolumeId {
    fn provider(&self) -> Provider {
        Provider::Modal
    }
    fn down(&self, ctx: &Ctx, _cfg: &Dict, yes: bool) -> Result<()> {
        crate::modal::down_volume(ctx, &self.0, yes)
    }
}

/// The instance an ID names, by its provider prefix.
pub(crate) fn parse(id: &str) -> Result<Box<dyn Instance>> {
    if id.starts_with("runpod:") || id.starts_with("do:") {
        return Ok(Box::new(crate::amd::Id::parse(id)?));
    }
    if id.starts_with("modal:volume:") {
        crate::modal::volume_name(id)?;
        return Ok(Box::new(VolumeId(id.into())));
    }
    if id.starts_with("modal:") {
        crate::modal::sandbox_id(id)?;
        return Ok(Box::new(SandboxId(id.into())));
    }
    if id.starts_with("gcp:") {
        return Ok(Box::new(crate::gcp::TpuId::parse(id)?));
    }
    // Never send another provider's qualified identifier to Vast.
    if id.contains(':') {
        return Err(Error::msg("Unknown provider in instance ID").next("gpu ls"));
    }
    Ok(Box::new(VastId(id.into())))
}

/// Instances from every reachable provider, and the providers that failed.
struct Inventory {
    items: Vec<Dict>,
    failed: Vec<(Provider, Error)>,
    /// Providers whose listing succeeded: only they can prove an instance is gone.
    answered: Vec<Provider>,
}

fn inventory(ctx: &Ctx, cfg: &Dict, only: Option<Provider>) -> Result<Inventory> {
    let mut inventory = Inventory {
        items: Vec::new(),
        failed: Vec::new(),
        answered: Vec::new(),
    };
    let selected = Provider::ALL
        .into_iter()
        .filter(|&provider| match only {
            Some(only) => only == provider,
            None => provider.listed(ctx, cfg),
        })
        .collect::<Vec<_>>();
    for &provider in &selected {
        match provider.instances(ctx, cfg) {
            Ok(items) => {
                inventory.items.extend(items);
                inventory.answered.push(provider);
            }
            // A listing of one provider is not partial: it fails as that provider.
            Err(error) if selected.len() == 1 => return Err(error),
            // Modal credentials without its runtime must not break other inventories.
            Err(Error::Status(ExitCode::NotSetup, _)) if provider == Provider::Modal => {}
            Err(error) => inventory.failed.push((provider, error)),
        }
    }
    Ok(inventory)
}

/// Live instances, and the providers whose answer is complete.
pub(crate) struct Live {
    pub items: Vec<Dict>,
    pub answered: Vec<Provider>,
}

pub(crate) fn live(ctx: &Ctx, cfg: &Dict) -> Result<Live> {
    let Inventory {
        items, answered, ..
    } = inventory(ctx, cfg, None)?;
    Ok(Live { items, answered })
}

fn labels(failed: &[(Provider, Error)]) -> String {
    failed
        .iter()
        .map(|(provider, _)| provider.label())
        .collect::<Vec<_>>()
        .join(" and ")
}

/// The given ID, or one picked from the live inventory (`None` when there is
/// nothing to pick). A short TPU name resolves to its full ID.
pub fn selected_id(
    ctx: &Ctx,
    cfg: &Dict,
    id: Option<&str>,
    action: &str,
) -> Result<Option<String>> {
    if let Some(id) = id {
        if !short_candidate(id) {
            ctx.result("id", Value::str(id));
            return Ok(Some(id.into()));
        }
        if !crate::gcp::setup::gcp_ready(cfg) {
            return Err(Error::status(
                ExitCode::NotFound,
                format!("No instance ID like '{id}'. Vast IDs are numeric"),
            )
            .next("gpu ls"));
        }
        ctx.require_gcloud()?;
        let settings = crate::gcp::Settings::load(cfg)?;
        let nodes = crate::gcp::all_nodes(ctx, &settings.project)?;
        let matched = nodes
            .iter()
            .filter(|node| crate::gcp::vm_node(node))
            .map(|node| crate::gcp::node_id(node, &settings.project))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|tpu| tpu.matches_short(id))
            .collect::<Vec<_>>();
        return match matched.as_slice() {
            [tpu] => {
                ctx.result("id", Value::str(tpu.cli()));
                Ok(Some(tpu.cli()))
            }
            [] => Err(Error::status(
                ExitCode::NotFound,
                format!("TPU {id} was not found in {}", settings.project),
            )
            .next("gpu ls --provider gcp")),
            _ => Err(
                Error::msg(format!("TPU ID {id} is ambiguous. Use a full ID"))
                    .next("gpu ls --provider gcp --long"),
            ),
        };
    }
    if !ctx.interactive() {
        return Err(Error::usage("Missing instance ID").next(format!("gpu {action} ID")));
    }
    let inventory = inventory(ctx, cfg, None)?;
    if !inventory.failed.is_empty() {
        ctx.eprintln(&format!(
            "Could not list {}; showing only reachable providers. Next: gpu ls",
            labels(&inventory.failed)
        ))?;
        if inventory.items.is_empty() {
            return Err(Error::msg("Instance listing is incomplete").next("gpu ls"));
        }
    }
    crate::instances::select_instance(ctx, &inventory.items, action)
}

/// `selected_id`, parsed into its provider's operations.
pub(crate) fn select(
    ctx: &Ctx,
    cfg: &Dict,
    id: Option<&str>,
    action: &str,
) -> Result<Option<Box<dyn Instance>>> {
    selected_id(ctx, cfg, id, action)?
        .map(|id| parse(&id))
        .transpose()
}

pub(crate) fn short_candidate(id: &str) -> bool {
    crate::gcp::component(id) && id.as_bytes()[0].is_ascii_alphabetic()
}

pub fn ls(
    ctx: &Ctx,
    cfg: &Dict,
    json: bool,
    long: bool,
    only: Option<Provider>,
    envelope: bool,
) -> Result<()> {
    let Inventory {
        items, mut failed, ..
    } = inventory(ctx, cfg, only)?;
    if json {
        let value = crate::redact::redact(&Value::List(
            items.iter().cloned().map(Value::Dict).collect(),
        ));
        if envelope {
            ctx.result("instances", value);
            ctx.result("complete", Value::Bool(failed.is_empty()));
            ctx.result(
                "unavailable_providers",
                Value::List(
                    failed
                        .iter()
                        .map(|(provider, _)| Value::str(provider.label()))
                        .collect(),
                ),
            );
        } else {
            ctx.println(&crate::pyjson::dumps(&value, Some(1)))?;
        }
    }
    if !json && (!items.is_empty() || failed.is_empty()) {
        crate::instances::render_table(ctx, &items, long)?;
    }
    if failed.is_empty() {
        return Ok(());
    }
    let unavailable = labels(&failed);
    let (provider, error) = failed.swap_remove(0);
    let message = format!(
        "Instance listing is incomplete: {unavailable} unavailable. {}",
        error.summary()
    );
    let next = error.next_step().map_or_else(
        || format!("gpu ls --provider {}", provider.key()),
        String::from,
    );
    Err(if error.retryable() {
        Error::unavailable(message)
    } else {
        error.with_text(message)
    }
    .next(next))
}
