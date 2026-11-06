//! Discovery is project-scoped, cached for 15 minutes, and fetched with one
//! parallel curl process rather than a Python/gcloud process per location.
//! v1 locations matches gcloud; v2 locations omits v4-only zones.
//! Launch preflight still checks its selected zone fresh before confirmation.
use crate::{
    ctx::Ctx,
    error::{Error, Result},
    gcp, gcp_http,
    pyjson::{Dict, Value},
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
fn invalid() -> Error {
    Error::msg("Google returned an incomplete TPU catalog. Next: gpu up --provider gcp")
}
fn type_rows(data: &Dict) -> Result<Vec<String>> {
    let rows = gcp::objects(
        data.get("acceleratorTypes")
            .cloned()
            .unwrap_or(Value::List(Vec::new())),
    )?;
    rows.iter()
        .map(|d| {
            gcp::text(d, "type")
                .or_else(|| gcp::text(d, "name").and_then(|n| n.rsplit('/').next()))
                .filter(|s| gcp::component(s))
                .map(String::from)
                .ok_or_else(invalid)
        })
        .collect()
}
fn base(project: &str, zone: &str) -> Result<String> {
    if !gcp::component(project) || !gcp::component(zone) {
        return Err(invalid());
    }
    Ok(format!(
        "https://tpu.googleapis.com/v2/projects/{project}/locations/{zone}/acceleratorTypes"
    ))
}
pub fn types(ctx: &Ctx, project: &str, zone: &str) -> Result<Vec<String>> {
    let rows = gcp_http::pages(
        ctx,
        project,
        &base(project, zone)?,
        "acceleratorTypes",
        1000,
    )?;
    let mut data = Dict::new();
    data.insert(
        "acceleratorTypes",
        Value::List(rows.into_iter().map(Value::Dict).collect()),
    );
    type_rows(&data)
}
fn file(project: &str) -> String {
    format!("tpu-catalog-{project}.json")
}
fn cached(ctx: &Ctx, project: &str) -> Option<Vec<Zone>> {
    let data = gcp_http::read_cache(ctx, &file(project), 900)?;
    let rows = data.get("zones")?.as_list()?;
    rows.iter()
        .map(|v| {
            let d = v.as_dict()?;
            let name = gcp::text(d, "name")
                .filter(|s| gcp::component(s))?
                .to_string();
            let types = d
                .get("types")?
                .as_list()?
                .iter()
                .map(|v| match v {
                    Value::Str(s) if gcp::component(s) => Some(s.clone()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            Some(Zone { name, types })
        })
        .collect()
}
pub fn cached_types(ctx: &Ctx, project: &str, zone: &str) -> Result<Vec<String>> {
    ctx.require_gcloud()?;
    if !gcp::component(project) || !gcp::component(zone) {
        return Err(invalid());
    }
    if let Some(z) = cached(ctx, project).and_then(|zs| zs.into_iter().find(|z| z.name == zone)) {
        return Ok(z.types);
    }
    types(ctx, project, zone)
}
fn discover(ctx: &Ctx, project: &str) -> Result<Vec<Zone>> {
    let _flight = ctx.gcp_catalog_lock.lock().map_err(|_| invalid())?;
    if let Some(zones) = cached(ctx, project) {
        return Ok(zones);
    }
    if !gcp::component(project) {
        return Err(invalid());
    }
    let locations = gcp_http::pages(
        ctx,
        project,
        &format!("https://tpu.googleapis.com/v1/projects/{project}/locations"),
        "locations",
        1000,
    )?;
    let names = locations
        .iter()
        .map(|d| {
            gcp::text(d, "locationId")
                .or_else(|| gcp::text(d, "name").and_then(|n| n.rsplit('/').next()))
                .filter(|s| gcp::component(s))
                .map(String::from)
                .ok_or_else(invalid)
        })
        .collect::<Result<Vec<_>>>()?;
    let urls = names
        .iter()
        .map(|z| base(project, z).map(|url| format!("{url}?pageSize=1000")))
        .collect::<Result<Vec<_>>>()?;
    let responses = gcp_http::many(ctx, project, &urls)?;
    let mut zones = Vec::new();
    for (name, mut response) in names.into_iter().zip(responses) {
        let mut types = Vec::new();
        let mut seen = std::collections::HashSet::new();
        loop {
            types.extend(type_rows(&response)?);
            let next = gcp::text(&response, "nextPageToken").unwrap_or("");
            if next.is_empty() {
                break;
            }
            if seen.len() >= 30 || !seen.insert(next.to_string()) {
                return Err(invalid());
            }
            response = gcp_http::get(
                ctx,
                project,
                &format!(
                    "{}?pageSize=1000&pageToken={}",
                    base(project, &name)?,
                    gcp_http::encode(next)
                ),
            )?;
        }
        zones.push(Zone { name, types });
    }
    let mut data = Dict::new();
    data.insert(
        "zones",
        Value::List(
            zones
                .iter()
                .map(|z| {
                    let mut d = Dict::new();
                    d.insert("name", Value::str(&z.name));
                    d.insert(
                        "types",
                        Value::List(z.types.iter().map(Value::str).collect()),
                    );
                    Value::Dict(d)
                })
                .collect(),
        ),
    );
    gcp_http::write_cache(ctx, &file(project), data);
    Ok(zones)
}
pub fn known(ctx: &Ctx, project: &str, tpu: Option<&str>) -> Vec<Zone> {
    let wanted = tpu.map(canonical);
    let mut zones = crate::gcp_facts::known_zones(ctx, project);
    zones.retain(|z| !z.types.is_empty() && wanted.as_ref().is_none_or(|t| z.types.contains(t)));
    crate::gcp_geo::order(&ctx.env, &mut zones, |z| &z.name);
    zones
}
pub fn types_job(ctx: &Ctx, cfg: &Dict) -> crate::picker::Job<Vec<String>> {
    let worker = ctx.clone();
    let cfg = cfg.clone();
    crate::picker::Job::new(move || {
        if !crate::setup::gcp_ready(&cfg) {
            return Ok(Vec::new());
        }
        let project = gcp::Settings::load(&cfg)
            .map_err(|e| e.text().to_string())?
            .project;
        zones(&worker, &project, None)
            .map(|zs| {
                let mut types = zs.into_iter().flat_map(|z| z.types).collect::<Vec<_>>();
                types.sort();
                types.dedup();
                types
            })
            .map_err(|e| e.text().to_string())
    })
}
pub fn zones(ctx: &Ctx, project: &str, tpu: Option<&str>) -> Result<Vec<Zone>> {
    ctx.require_gcloud()?;
    if !gcp::component(project) {
        return Err(invalid());
    }
    let mut result = discover(ctx, project)?;
    let wanted = tpu.map(canonical);
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
    format!("{}  {}", zone.name, generations.join(", "))
}
pub fn verify(ctx: &Ctx, project: &str, zone: &str, tpu: &str) -> Result<()> {
    if types(ctx, project, zone)?.contains(&canonical(tpu)) {
        return Ok(());
    }
    // A stale discovery entry must not recommend the just-rejected zone.
    let path = ctx
        .env
        .xdg("XDG_CACHE_HOME", ".cache")
        .join("gpu")
        .join(file(project));
    let _ = std::fs::remove_file(path);
    let alternatives = known(ctx, project, Some(tpu))
        .into_iter()
        .filter(|z| z.name != zone)
        .collect::<Vec<_>>();
    let offered = alternatives
        .iter()
        .take(3)
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
    Err(Error::msg(format!(
        "{tpu} unavailable in {zone}. No TPU or task created.\nTry: {}.\nNext: {next}",
        if offered.is_empty() {
            "another type"
        } else {
            &offered
        }
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    #[test]
    fn empty_locations_are_not_offered_and_type_filtering_uses_live_catalogs() {
        let env = TestEnv::new();
        let runner = crate::tests::gcp_transport::runner(|cmd| {
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
        assert!(message.contains("v2-8 unavailable in us-west1-b"));
        assert!(message.contains("us-central1-b"));
    }
}
