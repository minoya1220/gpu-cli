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
fn invalid() -> Error {
    Error::msg("Google returned an incomplete TPU catalog. Next: gpu up --provider gcp")
}
fn type_names(rows: &[Dict]) -> Result<Vec<String>> {
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
    type_names(&rows)
}

pub(crate) fn file(project: &str) -> String {
    format!("tpu-catalog-{project}.json")
}
pub(crate) fn parse_zone(d: &Dict) -> Option<Zone> {
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
}
fn cached(ctx: &Ctx, project: &str) -> Option<Vec<Zone>> {
    let data = gcp_http::read_cache(ctx, &file(project), i64::MAX)?;
    let rows = data.get("zones")?.as_list()?;
    rows.iter()
        .map(|v| v.as_dict().and_then(parse_zone))
        .collect()
}
#[cfg(test)]
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
pub fn refresh(ctx: &Ctx, project: &str) -> Result<Vec<Zone>> {
    ctx.require_gcloud()?;
    let _flight = ctx.gcp_catalog_lock.lock().map_err(|_| invalid())?;
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
    for (name, response) in names.into_iter().zip(responses) {
        let rows = gcp_http::pages_from(
            ctx,
            project,
            &base(project, &name)?,
            "acceleratorTypes",
            1000,
            response,
            invalid,
        )?;
        zones.push(Zone {
            name,
            types: type_names(&rows)?,
        });
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
    select_zones(ctx, crate::gcp_facts::known_zones(ctx, project), tpu)
}
fn select_zones(ctx: &Ctx, mut zones: Vec<Zone>, tpu: Option<&str>) -> Vec<Zone> {
    let wanted = tpu.map(crate::gcp_types::canonical);
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
        refresh(&worker, &project)
            .or_else(|error| cached(&worker, &project).ok_or(error))
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
    let result = match cached(ctx, project) {
        Some(zones) => zones,
        None => refresh(ctx, project)?,
    };
    Ok(select_zones(ctx, result, tpu))
}

pub fn verify(ctx: &Ctx, project: &str, zone: &str, tpu: &str) -> Result<()> {
    if types(ctx, project, zone)?.contains(&crate::gcp_types::canonical(tpu)) {
        return Ok(());
    }
    // A stale discovery entry must not recommend the just-rejected zone.
    if let Some(mut data) = gcp_http::read_cache(ctx, &file(project), i64::MAX) {
        if let Some(Value::List(mut zones)) = data.get("zones").cloned() {
            zones.retain(|v| v.as_dict().and_then(|d| gcp::text(d, "name")) != Some(zone));
            data.insert("zones", Value::List(zones));
        }
        gcp_http::write_cache(ctx, &file(project), data);
    }
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
    fn launch_jobs_refresh_even_new_caches_and_keep_offline_choices() {
        use crate::picker::{testing::*, JobState, Next, Opt, Outcome, Screen};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        let env = TestEnv::new();
        let count = Arc::new(AtomicUsize::new(0));
        let shared = count.clone();
        let runner = crate::tests::gcp_transport::runner(move |cmd| {
            if cmd.argv[3] == "locations" {
                let n = shared.fetch_add(1, Ordering::SeqCst);
                if n >= 2 {
                    return Ok(crate::process::Output::failed(1, "offline"));
                }
                return ok(r#"[{"locationId":"us-east5-b"}]"#);
            }
            ok(&format!(
                r#"[{{"type":"v6e-{}"}}]"#,
                shared.load(Ordering::SeqCst)
            ))
        });
        let ctx = env.ctx().runner(&runner).build().for_gcp_setup();
        let cfg = dict(
            r#"{"gcp":{"project":"test-project","setup":{"version":1,"project":"test-project"}}}"#,
        );
        for expected in ["v6e-1", "v6e-2", "v6e-2"] {
            let job = types_job(&ctx, &cfg);
            let rows = job.clone();
            let observer = job.observer();
            let screen = Screen::new(
                "types",
                move || {
                    rows.result()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|s| Opt::new(s.clone(), s))
                        .collect()
                },
                Next::Done,
            )
            .load(job);
            let result = drive(screen, move |keys| {
                keys.wait(|_| observer.state() == JobState::Done);
                keys.press(RIGHT);
            });
            assert_eq!(result, Outcome::Done(expected.to_string()));
        }
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }
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
