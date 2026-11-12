//! gcloud invocation and reply parsing.
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::process::Cmd;
use crate::pyjson::{self, Dict, Value};

pub fn text<'a>(d: &'a Dict, key: &str) -> Option<&'a str> {
    match d.get(key) {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    }
}
/// Never relay gcloud stderr: it may contain credentials, URLs or tracebacks.
pub fn run(ctx: &Ctx, args: &[String], next: &str) -> Result<String> {
    run_timeout(ctx, args, next, 600.0)
}
pub(crate) fn run_timeout(ctx: &Ctx, args: &[String], next: &str, timeout: f64) -> Result<String> {
    crate::provider_errors::gcp(ctx.run(&Cmd::new(args).timeout(Some(timeout))), next)
}
pub fn json(ctx: &Ctx, args: Vec<String>, next: &str) -> Result<Value> {
    json_timeout(ctx, args, next, 600.0)
}
pub(crate) fn json_timeout(
    ctx: &Ctx,
    mut args: Vec<String>,
    next: &str,
    timeout: f64,
) -> Result<Value> {
    args.push("--format=json".into());
    pyjson::loads(&run_timeout(ctx, &args, next, timeout)?).map_err(|_| {
        Error::unavailable(format!(
            "Google Cloud returned an unreadable reply. Retry: {next}"
        ))
    })
}
pub fn objects(value: Value) -> Result<Vec<Dict>> {
    if let Value::List(items) = value {
        return items
            .into_iter()
            .map(|v| match v {
                Value::Dict(d) => Ok(d),
                _ => Err(bad_reply()),
            })
            .collect();
    }
    Err(bad_reply())
}
pub fn bad_reply() -> Error {
    Error::unavailable("Google Cloud returned an incomplete reply. Retry: gpu ls --provider gcp")
}
