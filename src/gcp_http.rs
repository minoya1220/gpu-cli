//! Read-only Google API transport. One in-memory OAuth token per CLI context;
//! credentials go to curl on stdin, never argv, disk, diagnostics or caches.
use crate::{
    ctx::Ctx,
    error::{Error, Result},
    gcp,
    process::{Cmd, Input},
    pyjson::{self, Dict, Value},
};
use std::io::Write;

pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn error(reason: &str) -> Error {
    Error::msg(format!(
        "Google catalog unavailable ({reason}). Next: gpu setup"
    ))
}
pub fn token(ctx: &Ctx, project: &str) -> Result<String> {
    ctx.require_gcloud()?;
    let mut tokens = ctx
        .gcp_tokens
        .lock()
        .map_err(|_| error("credential cache unavailable"))?;
    if let Some(token) = tokens.get(project) {
        return Ok(token.clone());
    }
    let out = ctx
        .run(
            &Cmd::new(&[
                "gcloud",
                "auth",
                "print-access-token",
                &format!("--project={project}"),
                "--quiet",
            ])
            .timeout(Some(30.0)),
        )
        .map_err(|_| error("could not read gcloud credentials"))?;
    let token = out.stdout.trim();
    if out.code != 0
        || token.is_empty()
        || !token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
    {
        return Err(error("gcloud authentication failed"));
    }
    tokens.insert(project.into(), token.into());
    Ok(token.into())
}
fn headers(ctx: &Ctx, project: &str) -> Result<String> {
    if !gcp::component(project) {
        return Err(error("invalid project"));
    }
    Ok(format!(
        "header = \"Authorization: Bearer {}\"\nheader = \"x-goog-user-project: {project}\"\n",
        token(ctx, project)?
    ))
}
fn body(ctx: &Ctx, project: &str, raw: &str, status: u16) -> Result<Dict> {
    if status == 401 {
        if let Ok(mut tokens) = ctx.gcp_tokens.lock() {
            tokens.remove(project);
        }
    }
    if !(200..300).contains(&status) {
        let reason = if raw.contains("SERVICE_DISABLED") {
            "API is disabled"
        } else if status == 401 {
            "authentication expired; retry"
        } else if raw.contains("PERMISSION_DENIED") || status == 403 {
            "access was denied"
        } else {
            "HTTPS request failed"
        };
        return Err(error(reason));
    }
    pyjson::loads(raw)
        .ok()
        .and_then(|v| v.as_dict().cloned())
        .ok_or_else(|| error("unreadable API reply"))
}
pub fn get(ctx: &Ctx, project: &str, url: &str) -> Result<Dict> {
    ctx.require_gcloud()?;
    if !(url.starts_with("https://cloudbilling.googleapis.com/v1/")
        || url.starts_with("https://cloudbilling.googleapis.com/v1beta/skus/")
        || url.starts_with("https://tpu.googleapis.com/v2/")
        || url.starts_with("https://tpu.googleapis.com/v1/")
        || url.starts_with("https://cloudtasks.googleapis.com/v2/"))
    {
        return Err(error("unexpected API endpoint"));
    }
    let config = headers(ctx, project)?;
    let out = ctx
        .run(
            &Cmd::new(&[
                "curl",
                "--disable",
                "--silent",
                "--show-error",
                "--noproxy",
                "*",
                "--proto",
                "=https",
                "--max-time",
                "15",
                "--config",
                "-",
                "--write-out",
                "\n%{http_code}",
                url,
            ])
            .stdin(Input::Text(config))
            .timeout(Some(20.0)),
        )
        .map_err(|_| error("could not run HTTPS request"))?;
    let (raw, status) = out
        .stdout
        .rsplit_once('\n')
        .ok_or_else(|| error("incomplete HTTP reply"))?;
    let status = status
        .trim()
        .parse()
        .map_err(|_| error("invalid HTTP status"))?;
    if out.code != 0 {
        return Err(error("HTTPS request failed"));
    }
    body(ctx, project, raw, status)
}
/// Curl multiplexes independent requests in one process. Response bodies are
/// public catalog data; per-response status/timing is separate from the bodies.
pub fn many(ctx: &Ctx, project: &str, urls: &[String]) -> Result<Vec<Dict>> {
    many_results(ctx, project, urls)?.into_iter().collect()
}
pub fn many_results(ctx: &Ctx, project: &str, urls: &[String]) -> Result<Vec<Result<Dict>>> {
    ctx.require_gcloud()?;
    if urls.is_empty() {
        return Ok(Vec::new());
    }
    let root = ctx.env.xdg("XDG_CACHE_HOME", ".cache").join("gpu");
    std::fs::create_dir_all(&root).map_err(|_| error("could not prepare request buffers"))?;
    let dir = tempfile::Builder::new()
        .prefix("tpu-catalog-")
        .tempdir_in(root)
        .map_err(|_| error("could not prepare request buffers"))?;
    if dir.path().to_string_lossy().chars().any(char::is_control) {
        return Err(error("invalid request buffer path"));
    }
    let mut config = headers(ctx, project)?;
    for (i, url) in urls.iter().enumerate() {
        if !(url.starts_with("https://tpu.googleapis.com/v2/")
            || url.starts_with("https://cloudbilling.googleapis.com/v1beta/skus/"))
            || url.contains(['"', '\n', '\r'])
        {
            return Err(error("unexpected API endpoint"));
        }
        let path = dir.path().join(i.to_string());
        let path = path
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        config.push_str(&format!("url = \"{url}\"\noutput = \"{path}\"\n"));
    }
    let out = ctx
        .run(
            &Cmd::new(&[
                "curl",
                "--disable",
                "--silent",
                "--show-error",
                "--noproxy",
                "*",
                "--proto",
                "=https",
                "--max-time",
                "15",
                "--parallel",
                "--parallel-max",
                "8",
                "--config",
                "-",
                "--write-out",
                "%{filename_effective}\t%{http_code}\t%{time_total}\n",
            ])
            .stdin(Input::Text(config))
            .timeout(Some(90.0)),
        )
        .map_err(|_| error("parallel HTTPS request failed"))?;
    if out.code != 0 {
        return Err(error("parallel HTTPS request failed"));
    }
    let mut statuses = std::collections::HashMap::new();
    for line in out.stdout.lines() {
        let mut fields = line.split('\t');
        let name = fields
            .next()
            .and_then(|s| std::path::Path::new(s).strip_prefix(dir.path()).ok())
            .and_then(|p| p.to_str())
            .ok_or_else(|| error("unreadable response metadata"))?;
        let status = fields
            .next()
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| error("unreadable response status"))?;
        if statuses.insert(name.to_string(), status).is_some() {
            return Err(error("duplicate response"));
        }
    }
    Ok((0..urls.len())
        .map(|i| {
            let status = *statuses
                .get(&i.to_string())
                .ok_or_else(|| error("missing response"))?;
            let raw = std::fs::read_to_string(dir.path().join(i.to_string()))
                .map_err(|_| error("missing response body"))?;
            body(ctx, project, &raw, status)
        })
        .collect())
}
pub fn pages(ctx: &Ctx, project: &str, base: &str, key: &str, size: usize) -> Result<Vec<Dict>> {
    let mut result = Vec::new();
    let mut next = String::new();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..30 {
        let url = format!(
            "{base}{}pageSize={size}&pageToken={}",
            if base.contains('?') { "&" } else { "?" },
            encode(&next)
        );
        let data = get(ctx, project, &url)?;
        result.extend(gcp::objects(
            data.get(key).cloned().unwrap_or(Value::List(Vec::new())),
        )?);
        next = match data.get("nextPageToken") {
            None => String::new(),
            Some(Value::Str(s)) => s.clone(),
            _ => return Err(error("invalid page token")),
        };
        if next.is_empty() {
            return Ok(result);
        }
        if !seen.insert(next.clone()) {
            return Err(error("repeated page token"));
        }
    }
    Err(error("pagination limit reached"))
}
pub fn read_cache(ctx: &Ctx, name: &str, ttl: i64) -> Option<Dict> {
    let path = ctx
        .env
        .xdg("XDG_CACHE_HOME", ".cache")
        .join("gpu")
        .join(name);
    let raw = std::fs::read_to_string(path).ok()?;
    let value = pyjson::loads(&raw).ok()?;
    let d = value.as_dict()?;
    let Value::Int(at) = d.get("at")? else {
        return None;
    };
    let age = ctx.clock.now().as_second().checked_sub(*at)?;
    (0..ttl).contains(&age).then(|| d.clone())
}
pub fn write_cache(ctx: &Ctx, name: &str, mut data: Dict) {
    data.insert("at", Value::Int(ctx.clock.now().as_second()));
    let root = ctx.env.xdg("XDG_CACHE_HOME", ".cache").join("gpu");
    if std::fs::create_dir_all(&root).is_ok() {
        if let Ok(mut file) = tempfile::NamedTempFile::new_in(&root) {
            if write!(file, "{}", pyjson::dumps(&Value::Dict(data), None)).is_ok() {
                let _ = file.persist(root.join(name));
            }
        }
    }
}
