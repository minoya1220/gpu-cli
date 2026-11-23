//! Invocation-scoped structured results. Never derive machine data from prose.
use crate::ctx::Ctx;
use crate::error::Result;
use crate::pyjson::{self, Dict, Value};
use crate::sync::MutexExt;

#[derive(Default)]
pub(crate) struct CommandOutput {
    pub json: bool,
    pub command: String,
    pub written: bool,
    pub result: Dict,
    pub ssh_timeout: Option<f64>,
    pub command_timeout: Option<f64>,
    /// `gpu up --dir`: offer the editor once the directory has been copied.
    pub defer_editor_offer: bool,
    /// The picker's answer to the autosync question, once it was asked.
    pub autosync_choice: Option<bool>,
    /// The directory the launch picker chose, as `--dir` would have given.
    pub picked_dir: Option<std::path::PathBuf>,
}

pub(crate) fn envelope(ctx: &Ctx, command: &str, outcome: &Result<()>) -> Value {
    let mut object = Dict::new();
    object.insert("command", Value::str(command));
    object.insert("ok", Value::Bool(outcome.is_ok()));
    object.insert("result", Value::Dict(ctx.result_snapshot()));
    if let Err(error) = outcome {
        let mut detail = Dict::new();
        detail.insert("code", Value::str(error.code_name()));
        detail.insert(
            "message",
            Value::str(crate::redact::redact_text(error.text())),
        );
        detail.insert("exit_code", Value::Int(error.exit_code().into()));
        object.insert("error", Value::Dict(detail));
    }
    Value::Dict(object)
}

pub(crate) fn finish(ctx: &Ctx, command: &str, outcome: &Result<()>) -> Result<()> {
    // Remote stdout/stderr are caller data, not provider API responses. They
    // deliberately pass through unchanged. Provider data is redacted at source.
    let _emission = ctx.emission.lock_recover();
    if ctx.output_written() {
        return Ok(());
    }
    ctx.console
        .println(&pyjson::dumps(&envelope(ctx, command, outcome), None))?;
    ctx.mark_output_written();
    Ok(())
}
