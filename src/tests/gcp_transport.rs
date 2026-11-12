//! REST fixtures consume curl config and emit real HTTP JSON pages/statuses.
use crate::{
    process::{Cmd, Input, Output, RunError},
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

pub fn runner(
    response: impl Fn(&str) -> (u16, String) + Send + Sync + 'static,
) -> std::sync::Arc<FakeRunner> {
    FakeRunner::new(move |cmd| {
        if cmd.argv.get(1).is_some_and(|a| a == "auth") {
            assert_eq!(cmd.program(), "gcloud");
            return ok("test-oauth-token\n");
        }
        curl(cmd, &response).unwrap_or_else(|| panic!("unexpected non-REST call: {cmd:?}"))
    })
}
