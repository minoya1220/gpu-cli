//! DigitalOcean GPU Droplets: root VM SSH, no account PAT on the guest.
use super::*;

const SIZE: &str = "gpu-mi300x1-192gb";

pub(super) fn catalog(ctx: &Ctx) -> Result<Vec<Quote>> {
    // doctl traverses API pagination internally; do not pass --page/--per-page.
    let value = call(
        ctx,
        &cli(Provider::DigitalOcean, &["compute", "size", "list"]),
    )?;
    let mut result = Vec::new();
    for row in rows(&value)? {
        if row.get("slug").and_then(Value::as_str) != Some(SIZE) {
            continue;
        }
        let price = rate(row.get("price_hourly").ok_or_else(invalid)?)?;
        let regions = row
            .get("regions")
            .and_then(Value::as_list)
            .ok_or_else(invalid)?;
        for region in regions {
            let location = region
                .as_str()
                .filter(|r| component(r))
                .ok_or_else(invalid)?;
            result.push(Quote {
                rate: price,
                available: row.get("available") == Some(&Value::Bool(true)),
                location: location.into(),
            });
        }
    }
    Ok(result)
}

pub(super) fn plan(
    ctx: &Ctx,
    cfg: &Dict,
    image: Option<&str>,
    label: &str,
    setup: &str,
) -> Result<Plan> {
    let location = setting(cfg, Provider::DigitalOcean, "region").unwrap_or("atl1");
    let keys = setting(cfg, Provider::DigitalOcean, "ssh_keys").ok_or_else(|| {
        Error::usage(
            "Set do.ssh_keys to registered SSH key IDs/fingerprints for ~/.ssh/id_ed25519.pub.",
        )
    })?;
    if !keys
        .chars()
        .all(|c| c.is_ascii_hexdigit() || c == ',' || c == ':')
    {
        return Err(Error::usage(
            "Invalid do.ssh_keys; use comma-separated IDs/fingerprints.",
        ));
    }
    let quote = catalog(ctx)?.into_iter().find(|q| q.location == location && q.available)
        .ok_or_else(|| Error::status(ExitCode::NoCapacity, "No single-MI300X catalog size available in do.region; allocation/quota is not guaranteed."))?;
    let image = image
        .or_else(|| setting(cfg, Provider::DigitalOcean, "image"))
        .unwrap_or("gpu-amd-base");
    if image.starts_with('-') || image.chars().any(char::is_whitespace) {
        return Err(Error::usage("Invalid DigitalOcean image."));
    }
    // Resolve the rolling slug now, and create using the resolved immutable ID.
    let images = call(
        ctx,
        &cli(Provider::DigitalOcean, &["compute", "image", "get", image]),
    )?;
    let entries = rows(&images)?;
    if entries.len() != 1 {
        return Err(invalid());
    }
    let image_id = numeric_id(&entries[0])?;
    if let Some(regions) = entries[0].get("regions").and_then(Value::as_list) {
        if !regions.iter().any(|r| r.as_str() == Some(location)) {
            return Err(Error::status(
                ExitCode::NoCapacity,
                "AMD image is not available in the selected region.",
            ));
        }
    }
    let mut data = tempfile::Builder::new()
        .prefix("gpu-amd-")
        .suffix(".sh")
        .tempfile()?;
    data.write_all(setup.as_bytes())?;
    data.flush()?;
    let command = cli(
        Provider::DigitalOcean,
        &[
            "compute",
            "droplet",
            "create",
            label,
            "--size",
            SIZE,
            "--region",
            location,
            "--image",
            &image_id,
            "--ssh-keys",
            keys,
            "--tag-names",
            "gpu-cli",
            "--user-data-file",
            &data.path().to_string_lossy(),
        ],
    );
    Ok(Plan {
        command,
        rate: quote.rate,
        _data: Some(data),
        ssh_setup: None,
    })
}

fn numeric_id(row: &Value) -> Result<String> {
    match row.get("id") {
        Some(Value::Int(id)) if *id > 0 => Ok(id.to_string()),
        _ => Err(invalid()),
    }
}
fn qualified(row: &Value) -> Result<String> {
    let raw = numeric_id(row)?;
    let region = text(row.get("region").ok_or_else(invalid)?, "slug")?;
    let id = format!("do:{region}:{raw}");
    Id::parse(&id)?;
    Ok(id)
}
pub(super) fn created(reply: &Value, region: &str) -> Result<String> {
    let entries = rows(reply)?;
    if entries.len() != 1 {
        return Err(Error::msg("Create outcome uncertain: no unique Droplet ID. Inspect gpu ls --provider do before retrying; billing may continue."));
    }
    let raw = numeric_id(&entries[0]).map_err(|_| Error::msg("Create outcome uncertain: no usable Droplet ID. Inspect gpu ls --provider do before retrying."))?;
    // ID ownership is recorded immediately, even if create omitted region data.
    let region = entries[0]
        .get("region")
        .and_then(|v| v.get("slug"))
        .and_then(Value::as_str)
        .unwrap_or(region);
    let id = format!("do:{region}:{raw}");
    Id::parse(&id)?;
    Ok(id)
}

pub(super) fn list(ctx: &Ctx) -> Result<Vec<Dict>> {
    // List ALL Droplets, not just gpu-cli tagged ones: absence verification
    // must not misclassify a resource whose tag was changed as deleted.
    let value = call(
        ctx,
        &cli(Provider::DigitalOcean, &["compute", "droplet", "list"]),
    )?;
    rows(&value)?
        .iter()
        .map(|row| {
            let mut item = Dict::new();
            item.insert("id", Value::str(qualified(row)?));
            item.insert("provider", Value::str("do"));
            for (from, to) in [("name", "label"), ("status", "actual_status")] {
                if let Some(v) = row.get(from) {
                    item.insert(to, v.clone());
                }
            }
            if row.get("size_slug").and_then(Value::as_str) == Some(SIZE) {
                item.insert("gpu_name", Value::str("MI300X"));
                item.insert("num_gpus", Value::Int(1));
            }
            if let Some(price) = row.get("size").and_then(|s| s.get("price_hourly")) {
                item.insert("dph_total", price.clone());
            }
            Ok(item)
        })
        .collect()
}

pub(super) fn endpoint(ctx: &Ctx, id: &Id, timeout: f64) -> Result<(String, u16)> {
    let value = call(
        ctx,
        &cli(
            Provider::DigitalOcean,
            &["compute", "droplet", "get", &id.raw],
        )
        .timeout(Some(timeout)),
    )?;
    let entries = rows(&value)?;
    if entries.len() != 1 || qualified(&entries[0])? != id.qualified() {
        return Err(Error::usage(
            "DigitalOcean region or resource ID does not match.",
        ));
    }
    let row = &entries[0];
    if row.get("status").and_then(Value::as_str) != Some("active") {
        return Err(Error::status(
            ExitCode::WaitTimeout,
            "Droplet is not active yet.",
        ));
    }
    let hosts = row
        .get("networks")
        .and_then(|n| n.get("v4"))
        .and_then(Value::as_list);
    let host = hosts
        .and_then(|h| {
            h.iter()
                .find(|h| h.get("type").and_then(Value::as_str) == Some("public"))
        })
        .and_then(|h| h.get("ip_address"))
        .and_then(Value::as_str)
        .filter(|host| public_ip(host))
        .ok_or_else(|| Error::status(ExitCode::WaitTimeout, "Droplet has no public SSH IP yet."))?;
    Ok((host.into(), 22))
}
