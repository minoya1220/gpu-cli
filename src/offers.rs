//! Vast offer search, the known GPU model catalog, and their display labels.

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use regex::Regex;

use crate::config;
use crate::ctx::{Ctx, Env};
use crate::error::{Error, Result};
use crate::history::stable_sort_by_key;
use crate::launch::UpArgs;
use crate::pyfmt::{fixed, float_repr, percent, py_float, py_str};
use crate::pyjson::{Dict, Value};
use crate::vast::Vast;

pub const NO_OFFERS: &str =
    "No matching offers found; try a different model, GPU count, reliability, or disk size";
pub const CATEGORY_ORDER: [&str; 7] = [
    "Datacenter Blackwell/Hopper",
    "Datacenter Ampere/Ada/older",
    "Workstation/Pro",
    "Consumer RTX 50 series",
    "Consumer RTX 40 series",
    "Consumer RTX 30 and older",
    "Other",
];
const BUNDLED_MODELS: &str = include_str!("../assets/gpu_models.txt");

/// Cheapest hourly price and offer count per GPU model.
pub type Stats = HashMap<String, (f64, usize)>;

// Offer searches can run on several picker threads at once.
static CACHE_LOCK: Mutex<()> = Mutex::new(());

/// Vast offer queries for one `gpu up` run; the mode is chosen later.
#[derive(Clone)]
pub struct OfferSearch {
    vast: Vast,
    bid: bool,
    max_price: Option<f64>,
    hide_deverified: bool,
    base: String,
    container_disk: String,
    vm_disk: String,
}

impl OfferSearch {
    pub fn new(vast: Vast, args: &UpArgs, cfg: &Dict) -> Result<OfferSearch> {
        let choosing_offer =
            !args.yes && args.offer.is_none() && (!args.dry_run || vast.ctx.interactive());
        let verified = if args.unverified || choosing_offer {
            ""
        } else {
            " verified=true"
        };
        let reliability = match args.reliability {
            Some(value) => float_repr(value),
            None => py_str(config::get(cfg, "reliability")?),
        };
        let disk = |key: &str| -> Result<String> {
            Ok(match args.disk {
                Some(disk) if disk != 0 => disk.to_string(),
                _ => py_str(config::get(cfg, key)?),
            })
        };
        Ok(OfferSearch {
            vast,
            bid: args.interruptible,
            max_price: args.max_price,
            // An interactive offer choice shows unverified hosts but never deverified ones.
            hide_deverified: choosing_offer && !args.unverified,
            base: format!(
                "external=false rentable=true{verified} num_gpus={} reliability>={reliability}",
                args.num_gpus
            ),
            container_disk: disk("disk")?,
            vm_disk: disk("vm_disk")?,
        })
    }

    pub fn query(&self, mode: &str, gpu: Option<&str>) -> String {
        let disk = if mode == "vm" {
            &self.vm_disk
        } else {
            &self.container_disk
        };
        let mut query = format!("{} disk_space>={disk}", self.base);
        if mode == "vm" {
            query.push_str(" vms_enabled=true");
        }
        if let Some(gpu) = gpu.filter(|g| !g.is_empty()) {
            query.push_str(&format!(" gpu_name={}", gpu.replace(' ', "_")));
        }
        if let Some(max_price) = self.max_price {
            let field = if self.bid { "min_bid" } else { "dph" };
            query.push_str(&format!(" {field}<={}", float_repr(max_price)));
        }
        query
    }

    /// Matching offers, cheapest first. Blocks on the Vast CLI.
    pub fn offers(&self, mode: &str, gpu: Option<&str>) -> Result<Vec<Dict>> {
        let mut items = offer_dicts(self.vast.offers(&self.query(mode, gpu), self.bid)?)?;
        // An empty price-filtered response does not distinguish capacity from
        // price. One read-only unpriced query supplies that distinction.
        if items.is_empty() && self.max_price.is_some() && !self.vast.ctx.interactive() {
            let mut unpriced = self.clone();
            unpriced.max_price = None;
            if !unpriced.offers(mode, gpu)?.is_empty() {
                return Err(Error::status(crate::error::ExitCode::OverMaxPrice,
                    "Offers exceed --max-price. Next: raise --max-price or choose a cheaper --gpu model."));
            }
        }
        update_gpu_cache(&self.vast.ctx.env, &items, None);
        if let Some(max_price) = self.max_price {
            let had_offers = !items.is_empty();
            let mut kept = Vec::new();
            for offer in items {
                if offer_price(&offer)? <= max_price {
                    kept.push(offer);
                }
            }
            if kept.is_empty() && had_offers && !self.vast.ctx.interactive() {
                return Err(Error::status(crate::error::ExitCode::OverMaxPrice,
                    "Offers exceed --max-price. Next: raise --max-price or choose a cheaper --gpu model."));
            }
            items = kept;
        }
        if self.hide_deverified {
            items.retain(|o| o.get("verification") != Some(&Value::str("deverified")));
        }
        let mut keyed = Vec::new();
        for offer in items {
            keyed.push((offer_price(&offer)?, offer));
        }
        stable_sort_by_key(&mut keyed, |pair| pair.0);
        Ok(keyed.into_iter().map(|(_, offer)| offer).collect())
    }

    /// Cheapest hourly price and offer count per GPU model. Blocks on the Vast CLI.
    pub fn stats(&self, mode: &str) -> Result<Stats> {
        gpu_stats(&self.offers(mode, None)?)
    }

    pub fn ctx(&self) -> &Ctx {
        &self.vast.ctx
    }
}

/// The search result as offer dicts, or Python's error for other shapes.
pub fn offer_dicts(value: Value) -> Result<Vec<Dict>> {
    match value {
        Value::List(items) => items
            .into_iter()
            .map(|item| match item {
                Value::Dict(dict) => Ok(dict),
                other => Err(Error::msg(format!(
                    "Vast returned an entry that is {}; expected an instance or offer object. Retry the command.",
                    other.kind()
                ))),
            })
            .collect(),
        Value::Dict(dict) if dict.is_empty() => Ok(Vec::new()),
        Value::Dict(_) | Value::Str(_) => Err(Error::msg(
            "Vast returned an object or text; expected a list of instances or offers. Retry the command.",
        )),
        other => Err(Error::msg(format!(
            "Vast returned {}; expected a list of instances or offers. Retry the command.",
            other.kind()
        ))),
    }
}

/// `offer[key]`, or Python's `KeyError`.
pub fn field<'a>(offer: &'a Dict, key: &str) -> Result<&'a Value> {
    offer.get(key).ok_or_else(|| Error::key(key))
}

pub fn offer_price(offer: &Dict) -> Result<f64> {
    match price_field(offer) {
        Some((key, value)) => offer_number(key, value),
        None => Ok(f64::INFINITY),
    }
}

/// The offer's hourly price field: `dph_total`, else `dph`.
pub fn price_field(offer: &Dict) -> Option<(&'static str, &Value)> {
    offer
        .get("dph_total")
        .map(|value| ("dph_total", value))
        .or_else(|| offer.get("dph").map(|value| ("dph", value)))
}

/// `float(offer[key])`, failing with the field and what it holds.
pub fn offer_number(key: &str, value: &Value) -> Result<f64> {
    py_float(value).map_err(|_| {
        Error::msg(format!(
            "Vast offer '{key}' must be a number, got {}. Retry the command.",
            value.kind()
        ))
    })
}

pub fn gpu_stats(offers: &[Dict]) -> Result<Stats> {
    let mut stats = Stats::new();
    for offer in offers {
        let name = py_str(field(offer, "gpu_name")?);
        let price = offer_price(offer)?;
        let (cheapest, count) = stats.get(&name).copied().unwrap_or((f64::INFINITY, 0));
        // Python's min(cheapest, price) keeps the first unless the second is smaller.
        let cheapest = if price < cheapest { price } else { cheapest };
        stats.insert(name, (cheapest, count + 1));
    }
    Ok(stats)
}

pub fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

fn reliability(offer: &Dict) -> Result<f64> {
    offer_number(
        "reliability",
        offer.get("reliability").unwrap_or(&Value::Int(0)),
    )
}

fn text_or(offer: &Dict, key: &str, default: &str) -> String {
    offer
        .get(key)
        .map(py_str)
        .unwrap_or_else(|| default.to_string())
}

pub fn offer_label(offer: &Dict) -> Result<String> {
    Ok(format!(
        "id={} {} x{} ${}/hr reliability={} location={} verification={}",
        py_str(field(offer, "id")?),
        py_str(field(offer, "gpu_name")?),
        py_str(field(offer, "num_gpus")?),
        fixed(offer_price(offer)?, 3),
        fixed(reliability(offer)?, 3),
        text_or(offer, "geolocation", "?"),
        text_or(offer, "verification", "unknown"),
    ))
}

/// Aligned one-line offer rows for a single GPU model (the picker title names it).
pub fn offer_rows(offers: &[Dict]) -> Result<Vec<String>> {
    let rows = offers
        .iter()
        .map(|offer| {
            Ok([
                format!("${}/hr", fixed(offer_price(offer)?, 3)),
                format!("{:>6} reliable", percent(reliability(offer)?, 1)),
                text_or(offer, "verification", "unknown"),
                format!("id={}", py_str(field(offer, "id")?)),
                text_or(offer, "geolocation", "?"),
            ])
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(aligned_rows(
        &rows,
        [
            Column::Right,
            Column::Plain,
            Column::Left,
            Column::Left,
            Column::Plain,
        ],
    ))
}

#[derive(Clone, Copy)]
pub(crate) enum Column {
    Left,
    Right,
    Plain,
}

/// The Vast picker's measured widths and double-space gutters, shared by TPU
/// rows. Plain cells retain their own formatting (e.g. reliability and location).
pub(crate) fn aligned_rows<const N: usize>(
    rows: &[[String; N]],
    columns: [Column; N],
) -> Vec<String> {
    let widths: [usize; N] =
        std::array::from_fn(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0));
    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, cell)| match columns[i] {
                    Column::Left => format!("{cell:<w$}", w = widths[i]),
                    Column::Right => format!("{cell:>w$}", w = widths[i]),
                    Column::Plain => cell.clone(),
                })
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect()
}

/// Hardware discovery uses one four-column grid across providers. Vast Spot
/// remains unquoted until the separate interruptible UI work is approved.
pub(crate) type HardwareColumns = [String; 4];

pub(crate) fn hardware_labels(rows: &[HardwareColumns]) -> Vec<String> {
    aligned_rows(
        rows,
        [Column::Left, Column::Right, Column::Left, Column::Plain],
    )
    .into_iter()
    .map(|label| label.trim_end().to_string())
    .collect()
}

pub(crate) fn model_columns(model: &str, stats: &Stats, pending: &str) -> HardwareColumns {
    let price = stats
        .get(model)
        .map(|(price, _)| *price)
        .filter(|price| price.is_finite() && *price >= 0.0);
    [
        model.into(),
        price
            .map(|price| format!("${}/hr", fixed(price, 3)))
            .unwrap_or_else(|| pending.into()),
        "Spot -".into(),
        stats
            .get(model)
            .map(|(_, count)| plural(*count, "offer"))
            .unwrap_or_else(|| "-".into()),
    ]
}

pub(crate) fn category_columns(
    category: &str,
    models: &[String],
    stats: &Stats,
    pending: &str,
) -> HardwareColumns {
    let values = models
        .iter()
        .filter_map(|model| stats.get(model))
        .collect::<Vec<_>>();
    let cheapest = values
        .iter()
        .map(|(price, _)| *price)
        .filter(|price| price.is_finite() && *price >= 0.0)
        .min_by(f64::total_cmp);
    [
        category.into(),
        cheapest
            .map(|price| format!("from ${}/hr", fixed(price, 3)))
            .unwrap_or_else(|| pending.into()),
        "Spot -".into(),
        if values.is_empty() {
            "-".into()
        } else {
            plural(values.iter().map(|(_, count)| count).sum(), "offer")
        },
    ]
}

pub fn model_label(model: &str, stats: &Stats) -> String {
    match stats.get(model) {
        None => model.to_string(),
        Some((price, count)) => format!(
            "{model}  ${}/hr ({})",
            fixed(*price, 3),
            plural(*count, "offer")
        ),
    }
}

pub fn category_label(category: &str, models: &[String], stats: &Stats) -> String {
    let priced: Vec<&String> = models.iter().filter(|m| stats.contains_key(*m)).collect();
    if priced.is_empty() {
        return format!("{category} ({})", plural(models.len(), "model"));
    }
    let mut cheapest = stats[priced[0]].0;
    for model in &priced[1..] {
        let price = stats[*model].0;
        if price < cheapest {
            cheapest = price;
        }
    }
    format!(
        "{category} ({}, from ${}/hr)",
        plural(priced.len(), "model"),
        fixed(cheapest, 3)
    )
}

static RTX_ADA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^rtx\d+ada").unwrap());

pub fn gpu_category(model: &str) -> &'static str {
    let mut name: String = model
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect();
    if name.starts_with("qrtx") {
        name = format!("quadro{}", &name[1..]);
    }
    let has = |tokens: &[&str]| tokens.iter().any(|t| name.contains(t));
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| name.starts_with(p));
    if has(&["b200", "b300", "h100", "h200", "h800", "h20"]) {
        return "Datacenter Blackwell/Hopper";
    }
    if starts(&["quadro", "rtxpro", "rtxa", "rtx6000d"]) || RTX_ADA.is_match(&name) {
        return "Workstation/Pro";
    }
    if has(&[
        "a100", "a800", "a40", "a10", "a30", "l4", "l40", "t4", "p100", "p4", "v100", "tesla",
    ]) {
        return "Datacenter Ampere/Ada/older";
    }
    if name.starts_with("rtx5") {
        return "Consumer RTX 50 series";
    }
    if name.starts_with("rtx4") {
        return "Consumer RTX 40 series";
    }
    if starts(&["rtx3", "rtx2", "gtx", "titan"]) {
        return "Consumer RTX 30 and older";
    }
    "Other"
}

/// Models per category, in display order, omitting empty categories.
pub fn group_by_category(models: &[String]) -> Vec<(String, Vec<String>)> {
    let mut groups: Vec<(String, Vec<String>)> = CATEGORY_ORDER
        .iter()
        .map(|c| (c.to_string(), Vec::new()))
        .collect();
    for model in models {
        let category = gpu_category(model);
        if let Some(group) = groups.iter_mut().find(|(name, _)| name == category) {
            group.1.push(model.clone());
        }
    }
    groups.retain(|(_, members)| !members.is_empty());
    groups
}

/// Bundled model names merged with names seen in earlier offer searches.
pub fn known_gpu_models(env: &Env) -> Vec<String> {
    let mut names: BTreeSet<String> = BUNDLED_MODELS
        .lines()
        .filter(|n| valid_gpu_name(n))
        .map(String::from)
        .collect();
    names.extend(read_gpu_cache(env, None));
    names.into_iter().collect()
}

pub fn gpu_cache_path(env: &Env) -> PathBuf {
    env.xdg("XDG_CACHE_HOME", ".cache")
        .join("gpu")
        .join("gpu_models.txt")
}

pub fn valid_gpu_name(value: &str) -> bool {
    let length = value.chars().count();
    (2..=100).contains(&length)
        && value == value.trim()
        && value
            .chars()
            .all(|c| c.is_alphanumeric() || " .,+()/_-".contains(c))
}

pub fn read_gpu_cache(env: &Env, path: Option<&Path>) -> Vec<String> {
    let cache = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| gpu_cache_path(env));
    match std::fs::read_to_string(cache) {
        Ok(text) => {
            let names: BTreeSet<String> = text
                .lines()
                .filter(|n| valid_gpu_name(n))
                .map(String::from)
                .collect();
            names.into_iter().collect()
        }
        Err(_) => Vec::new(),
    }
}

pub fn update_gpu_cache(env: &Env, items: &[Dict], path: Option<&Path>) {
    let mut names: BTreeSet<String> = items
        .iter()
        .filter_map(|item| item.get("gpu_name").and_then(Value::as_str))
        .filter(|name| valid_gpu_name(name))
        .map(String::from)
        .collect();
    if names.is_empty() {
        return;
    }
    let cache = path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| gpu_cache_path(env));
    let _guard = CACHE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    names.extend(read_gpu_cache(env, Some(&cache)));
    let Some(parent) = cache.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    // Replace atomically so a concurrent reader never sees a partial file.
    let Ok(mut temp) = tempfile::Builder::new().suffix(".tmp").tempfile_in(parent) else {
        return;
    };
    let text = names.into_iter().collect::<Vec<_>>().join("\n") + "\n";
    if temp.write_all(text.as_bytes()).is_ok() {
        let _ = temp.persist(&cache);
    }
}
