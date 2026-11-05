//! Locations are not capacity: only accelerator-type catalog entries prove a
//! type is offered in a zone. All fan-out runs inside a Job or outside the UI.
use crate::{
    ctx::Ctx,
    error::{Error, Result},
    gcp,
};

#[derive(Clone, Debug)]
pub struct Zone {
    pub name: String,
    pub types: Vec<String>,
}
pub fn canonical(tpu: &str) -> String {
    tpu.strip_prefix("v5e-")
        .map(|n| format!("v5litepod-{n}"))
        .unwrap_or_else(|| tpu.into())
}
pub fn types(ctx: &Ctx, project: &str, zone: &str) -> Result<Vec<String>> {
    let rows = gcp::objects(gcp::json(
        ctx,
        vec![
            "gcloud".into(),
            "compute".into(),
            "tpus".into(),
            "tpu-vm".into(),
            "accelerator-types".into(),
            "list".into(),
            format!("--project={project}"),
            "--quiet".into(),
            format!("--zone={zone}"),
        ],
        "gpu setup",
    )?)?;
    rows.iter()
        .map(|d| {
            gcp::text(d, "type")
                .or_else(|| gcp::text(d, "name").and_then(|n| n.rsplit('/').next()))
                .filter(|s| {
                    !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
                })
                .map(String::from)
                .ok_or_else(gcp::bad_reply)
        })
        .collect()
}
pub fn zones(ctx: &Ctx, project: &str, tpu: Option<&str>) -> Result<Vec<Zone>> {
    let locations = gcp::locations(ctx, project)?;
    let wanted = tpu.map(canonical);
    let mut result = std::thread::scope(|scope| -> Result<Vec<Zone>> {
        let chunk = locations.len().div_ceil(4).max(1);
        let handles: Vec<_> = locations
            .chunks(chunk)
            .map(|group| {
                scope.spawn(move || -> Result<Vec<Zone>> {
                    group
                        .iter()
                        .map(|name| {
                            Ok(Zone {
                                name: name.clone(),
                                types: types(ctx, project, name)?,
                            })
                        })
                        .collect()
                })
            })
            .collect();
        let mut zones = Vec::new();
        for handle in handles {
            zones.extend(
                handle
                    .join()
                    .map_err(|_| Error::msg("TPU catalog worker failed. Next: gpu setup"))??,
            );
        }
        Ok(zones)
    })?;
    result.retain(|z| !z.types.is_empty() && wanted.as_ref().is_none_or(|t| z.types.contains(t)));
    crate::gcp_geo::order(&ctx.env, &mut result, |z| &z.name);
    Ok(result)
}
pub fn label(zone: &Zone) -> String {
    let mut generations: Vec<_> = zone
        .types
        .iter()
        .filter_map(|t| {
            t.split_once('-')
                .map(|(g, _)| if g == "v5litepod" { "v5e" } else { g })
        })
        .collect();
    generations.sort();
    generations.dedup();
    format!("{} — {}", zone.name, generations.join(", "))
}
pub fn verify(ctx: &Ctx, project: &str, zone: &str, tpu: &str) -> Result<()> {
    if types(ctx, project, zone)?.contains(&canonical(tpu)) {
        return Ok(());
    }
    let alternatives = zones(ctx, project, Some(tpu))?;
    let offered = alternatives
        .iter()
        .map(|z| z.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let next = alternatives
        .first()
        .map(|z| {
            format!(
                "gpu up --tpu {} --zone {} --no-repo --expire 2h",
                crate::pyfmt::shell_quote(tpu),
                z.name
            )
        })
        .unwrap_or_else(|| "gpu up --provider gcp".into());
    Err(Error::msg(format!("{tpu} is not offered in {zone}. Zones that offer it: {}. No TPU or deletion task created. Next: {next}", if offered.is_empty() { "none returned" } else { &offered })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    #[test]
    fn empty_locations_are_not_offered_and_type_filtering_uses_live_catalogs() {
        let env = TestEnv::new();
        let runner = FakeRunner::new(|cmd| {
            assert_eq!(cmd.program(), "gcloud");
            assert!(!cmd.argv.iter().any(|s| s == "create" || s == "delete"));
            if cmd.argv[3] == "locations" {
                return ok(
                    r#"[{"locationId":"us-west1-b"},{"locationId":"us-east5-b"},{"locationId":"us-central1-b"},{"locationId":"us-west1-c"}]"#,
                );
            }
            match cmd.argv.iter().find_map(|s| s.strip_prefix("--zone=")) {
                Some("us-west1-b") => ok("[]"),
                Some("us-west1-c") => ok(r#"[{"type":"v5litepod-1"},{"type":"v6e-1"}]"#),
                Some("us-central1-b") => ok(r#"[{"type":"v2-8"}]"#),
                Some("us-east5-b") => ok(r#"[{"type":"v6e-1"}]"#),
                _ => panic!("unexpected catalog request"),
            }
        });
        let ctx = env
            .ctx()
            .runner(&runner)
            .env(env.env().with("TZ", "America/Los_Angeles"))
            .build()
            .for_gcp_setup();
        let all = zones(&ctx, "test-project", None).unwrap();
        assert_eq!(
            all.iter().map(|z| z.name.as_str()).collect::<Vec<_>>(),
            ["us-west1-c", "us-central1-b", "us-east5-b"]
        );
        assert_eq!(
            zones(&ctx, "test-project", Some("v2-8")).unwrap()[0].name,
            "us-central1-b"
        );
        assert_eq!(
            zones(&ctx, "test-project", Some("v5e-1")).unwrap()[0].name,
            "us-west1-c"
        );
        let message = verify(&ctx, "test-project", "us-west1-b", "v2-8")
            .unwrap_err()
            .text()
            .to_string();
        assert!(message.contains("v2-8 is not offered in us-west1-b"));
        assert!(message.contains("Zones that offer it: us-central1-b"));
    }
}
