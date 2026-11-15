//! Modern runpodctl v3 Pods, Secure Cloud only; no independent network volumes.
use super::*;

const GPU: &str = "AMD Instinct MI300X OAM";

pub(super) fn catalog(ctx: &Ctx) -> Result<Vec<Quote>> {
    let value = call(
        ctx,
        &cli(Provider::Runpod, &["gpu", "list", "--include-unavailable"]),
    )?;
    let mut result = Vec::new();
    for row in rows(&value)? {
        if row.get("gpuId").and_then(Value::as_str) != Some(GPU) {
            continue;
        }
        // communityPrice is not a rentable quote when communityCloud is false.
        if row.get("secureCloud") != Some(&Value::Bool(true)) {
            continue;
        }
        let price = rate(row.get("securePricePerHr").ok_or_else(invalid)?)?;
        let centers = row
            .get("dataCenterAvailability")
            .and_then(Value::as_list)
            .ok_or_else(invalid)?;
        for dc in centers {
            let location = text(dc, "dataCenterId")?;
            if !component(location) {
                return Err(invalid());
            }
            let stock = text(dc, "stockStatus")?.to_ascii_lowercase();
            result.push(Quote {
                rate: price,
                available: matches!(stock.as_str(), "high" | "medium" | "low"),
                location: location.into(),
            });
        }
    }
    Ok(result)
}

pub(super) fn plan(
    ctx: &Ctx,
    cfg: &Dict,
    disk: Option<i64>,
    image: Option<&str>,
    label: &str,
    setup: &str,
) -> Result<Plan> {
    let image = image.or_else(|| setting(cfg, Provider::Runpod, "image"))
        .ok_or_else(|| Error::usage("Set runpod.image to an explicit ROCm image with bash, sshd, git, tar and SFTP support. Real rentals are untested."))?;
    if image.is_empty() || image.chars().any(char::is_whitespace) || image.starts_with('-') {
        return Err(Error::usage("Invalid runpod.image."));
    }
    let location = setting(cfg, Provider::Runpod, "datacenter").ok_or_else(|| {
        Error::usage(
            "Set runpod.datacenter from gpu offers --provider runpod; no silent location fallback.",
        )
    })?;
    let quote = catalog(ctx)?.into_iter().find(|q| q.location == location && q.available)
        .ok_or_else(|| Error::status(ExitCode::NoCapacity, "No available Secure single-MI300X quote in runpod.datacenter; no resource created."))?;
    let disk = disk.unwrap_or(20);
    if !(1..=2000).contains(&disk) {
        return Err(Error::usage("Runpod --disk must be 1..=2000 GB."));
    }
    // A failed clone marks setup failed but must not kill sshd/container access.
    let script = format!(
        "bash -lc {}\nexec sleep infinity\n",
        crate::pyfmt::shell_quote(setup)
    );
    let args = crate::pyfmt::shell_join(&["bash".to_string(), "-lc".into(), script]);
    let command = cli(
        Provider::Runpod,
        &[
            "pod",
            "create",
            "--cloud-type",
            "SECURE",
            "--gpu-id",
            GPU,
            "--gpu-count",
            "1",
            "--image",
            image,
            "--data-center-ids",
            location,
            "--ports",
            "22/tcp",
            "--ssh",
            "--container-disk-in-gb",
            &disk.to_string(),
            "--volume-in-gb",
            "0",
            "--name",
            label,
            "--docker-args",
            &args,
        ],
    );
    // Official container-disk rate $0.10/GB-month, estimated at 730h/month.
    // No persistent or independent network volume is allocated.
    Ok(Plan {
        command,
        rate: quote.rate + disk as f64 * 0.10 / 730.0,
        _data: None,
    })
}

pub(super) fn created(reply: &Value) -> Result<String> {
    let raw = text(reply, "id").map_err(|_| Error::msg("Create outcome uncertain: no Pod ID returned. Inspect gpu ls --provider runpod before retrying; a Pod may be billing."))?;
    let id = format!("runpod:{raw}");
    Id::parse(&id)?;
    Ok(id)
}

pub(super) fn list(ctx: &Ctx) -> Result<Vec<Dict>> {
    let reply = call(ctx, &cli(Provider::Runpod, &["pod", "list", "--all"]))?;
    rows(&reply)?
        .iter()
        .map(|row| {
            let id = created(row)?;
            let mut item = Dict::new();
            item.insert("id", Value::str(id));
            item.insert("provider", Value::str("runpod"));
            for (from, to) in [
                ("name", "label"),
                ("gpuId", "gpu_name"),
                ("gpuCount", "num_gpus"),
                ("costPerHr", "dph_total"),
                ("runtimeStatus", "actual_status"),
                ("uptimeSeconds", "uptime"),
            ] {
                if let Some(v) = row.get(from) {
                    item.insert(to, v.clone());
                }
            }
            Ok(item)
        })
        .collect()
}

pub(super) fn endpoint(ctx: &Ctx, raw: &str, timeout: f64) -> Result<(String, u16)> {
    let reply = call(
        ctx,
        &cli(Provider::Runpod, &["pod", "get", raw]).timeout(Some(timeout)),
    )?;
    if text(&reply, "id")? != raw {
        return Err(invalid());
    }
    if reply.get("runtimeStatus").and_then(Value::as_str) != Some("running") {
        return Err(Error::status(
            ExitCode::WaitTimeout,
            "Pod container is not running yet.",
        ));
    }
    let ssh = reply.get("ssh").ok_or_else(invalid)?;
    let host = ssh.get("ip").and_then(Value::as_str);
    let port = ssh
        .get("port")
        .and_then(|v| match v {
            Value::Int(v) => u16::try_from(*v).ok(),
            _ => None,
        })
        .filter(|&v| v > 0);
    match (host, port) {
        (Some(host), Some(port)) if public_ip(host) => Ok((host.into(), port)),
        _ => Err(Error::status(
            ExitCode::WaitTimeout,
            "No direct public TCP/22 mapping. Proxy SSH is not supported for copy/editors.",
        )),
    }
}
