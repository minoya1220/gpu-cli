//! REST fixtures adapt existing stateful read handlers, never a real executable.
use crate::{
    process::{Cmd, Input, Output, RunError},
    pyjson::{self, Dict, Value},
    testing::*,
};
pub fn curl(
    cmd: &Cmd,
    mut response: impl FnMut(&str) -> (u16, String),
) -> Option<Result<Output, RunError>> {
    if cmd.program() != "curl" {
        return None;
    }
    let Input::Text(config) = &cmd.stdin else {
        panic!("missing curl config")
    };
    assert!(config.contains("Authorization: Bearer"));
    let mut url = None;
    let mut output = String::new();
    for line in config.lines() {
        if let Some(s) = line
            .strip_prefix("url = \"")
            .and_then(|s| s.strip_suffix('"'))
        {
            url = Some(s.to_string());
        }
        if let Some(path) = line
            .strip_prefix("output = \"")
            .and_then(|s| s.strip_suffix('"'))
        {
            let (status, body) = response(url.take().as_deref().expect("URL before output"));
            let path = path.replace("\\\"", "\"").replace("\\\\", "\\");
            std::fs::write(&path, body).unwrap();
            output.push_str(&format!("{path}\t{status}\t0.001\n"));
        }
    }
    if !output.is_empty() {
        return Some(ok(&output));
    }
    let (status, body) = response(cmd.argv.last().unwrap());
    Some(ok(&format!("{body}\n{status}")))
}
pub fn adapt(
    cmd: &Cmd,
    native: impl Fn(&Cmd) -> Result<Output, RunError>,
) -> Option<Result<Output, RunError>> {
    if cmd.argv.get(1).is_some_and(|a| a == "auth") {
        return Some(ok("test-oauth-token\n"));
    }
    curl(cmd, |url| {
        if url.starts_with("https://cloudbilling.") {
            return (403, r#"{"error":{"status":"PERMISSION_DENIED"}}"#.into());
        }
        let rest = url.split("/projects/").nth(1).unwrap();
        let project = rest.split('/').next().unwrap();
        let path = rest.split('?').next().unwrap();
        let zone = path
            .split("/locations/")
            .nth(1)
            .unwrap_or("")
            .split('/')
            .next()
            .unwrap();
        let (args, key) = if url.contains("acceleratorTypes") {
            (
                vec![
                    "compute".into(),
                    "tpus".into(),
                    "tpu-vm".into(),
                    "accelerator-types".into(),
                    "list".into(),
                    format!("--zone={zone}"),
                ],
                "acceleratorTypes",
            )
        } else if url.contains("/nodes") {
            (
                vec![
                    "compute".into(),
                    "tpus".into(),
                    "tpu-vm".into(),
                    "list".into(),
                    format!("--zone={zone}"),
                ],
                "nodes",
            )
        } else if url.contains("/tasks?") {
            let queue = path
                .split("/queues/")
                .nth(1)
                .unwrap()
                .split('/')
                .next()
                .unwrap();
            (
                vec![
                    "tasks".into(),
                    "list".into(),
                    format!("--location={zone}"),
                    format!("--queue={queue}"),
                ],
                "tasks",
            )
        } else {
            (
                vec![
                    "compute".into(),
                    "tpus".into(),
                    "locations".into(),
                    "list".into(),
                ],
                "locations",
            )
        };
        let mut argv = vec!["gcloud".to_string()];
        argv.extend(args);
        argv.extend([
            format!("--project={project}"),
            "--quiet".into(),
            "--format=json".into(),
        ]);
        let out = native(&Cmd::new(&argv)).unwrap();
        if out.code != 0 {
            return (
                if out.stderr.contains("NOT_FOUND") {
                    404
                } else {
                    403
                },
                "{}".into(),
            );
        }
        let mut page = Dict::new();
        page.insert(key, json(&out.stdout));
        (200, pyjson::dumps(&Value::Dict(page), None))
    })
}

pub fn runner(
    native: impl Fn(&Cmd) -> Result<Output, RunError> + Send + Sync + 'static,
) -> std::sync::Arc<FakeRunner> {
    FakeRunner::new(move |cmd| adapt(cmd, &native).unwrap_or_else(|| native(cmd)))
}
