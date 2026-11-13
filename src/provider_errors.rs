//! Translate known provider failure signatures once, without exposing secrets.
#[cfg(test)]
mod tests;
use crate::error::{Error, ExitCode, Result};
use crate::process::{Output, RunError};

/// Translate a gcloud result. Never relay its output: diagnostics can
/// contain credentials or URLs. `next` is the recovery command to suggest.
pub(crate) fn gcp(result: std::result::Result<Output, RunError>, next: &str) -> Result<String> {
    let output = match result {
        Ok(output) if output.code == 0 => return Ok(output.stdout.trim().into()),
        Ok(output) => output,
        Err(RunError::Denied(message)) => return Err(Error::status(ExitCode::NotSetup, message)),
        Err(RunError::NotFound) => {
            return Err(
                Error::status(ExitCode::NotSetup, "Install the Google Cloud CLI")
                    .next("gpu setup --provider gcp"),
            )
        }
        Err(RunError::TimedOut) => {
            return Err(Error::timeout("Google Cloud timed out. Completion unverified").next(next))
        }
        Err(_) => return Err(Error::msg("Could not run Google Cloud CLI").next(next)),
    };
    let detail = format!("{} {}", output.stdout, output.stderr).to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| detail.contains(word));
    let error = if has(&["quota"]) {
        Error::msg("Google Cloud quota is exhausted").rejected()
    } else if has(&["capacity", "resource_exhausted", "no more"]) {
        Error::status(ExitCode::NoCapacity, "No TPU capacity in this zone").rejected()
    } else if has(&["permission", "forbidden"]) {
        Error::msg("Google Cloud denied access. Check project IAM and enabled APIs").rejected()
    } else if has(&[
        "login",
        "unauthenticated",
        "invalid_grant",
        "credential",
        "reauth",
    ]) {
        return Err(
            Error::status(ExitCode::AuthExpired, "Sign in to Google Cloud")
                .next("gcloud auth login"),
        );
    } else if has(&["not_found", "not found"]) {
        Error::status(ExitCode::NotFound, "Google Cloud resource was not found").rejected()
    } else {
        Error::unavailable(
            "Google Cloud request failed; provider may be temporarily unavailable. Completion unverified",
        )
    };
    Err(error.next(next))
}

/// Modal's private helper reports exception kinds, not human message signatures.
pub(crate) fn modal(kind: &str, detail: &str) -> Error {
    let detail = crate::redact::redact_text(detail);
    let code = match kind {
        "MissingToken" => ExitCode::NotSetup,
        "AuthError" => ExitCode::AuthExpired,
        // A rejected GPU/image request or account billing prerequisite is not
        // expired authentication or scarce capacity. Keep the helper's fixed,
        // non-secret guidance and classify this as a usage/request rejection.
        "InvalidError" => return Error::usage(detail),
        "ResourceExhaustedError" | "SchedulingTimeout" => ExitCode::NoCapacity,
        "NotFoundError" => ExitCode::NotFound,
        "InstanceLost" => ExitCode::InstanceLost,
        "CleanupUnverified" => ExitCode::CleanupUnverified,
        "CommandTimeout" => ExitCode::CommandTimeout,
        _ => return Error::msg(detail),
    };
    Error::status(code, detail)
}

/// Whether `text` contains HTTP status `code` as its own token right after
/// a status word (`error 401`, `status: 401`), so instance
/// and offer IDs that merely contain the digits never match.
fn http_status(text: &str, code: &str) -> bool {
    let words: Vec<&str> = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    words
        .windows(2)
        .any(|pair| pair[1] == code && ["error", "status", "http", "code"].contains(&pair[0]))
}

pub(crate) fn vast(detail: &str) -> Option<Error> {
    let lower = detail.to_ascii_lowercase();
    let detail = crate::redact::redact_text(detail.trim());
    let with_detail = |code, summary: &str, next: &str| {
        Some(if detail.is_empty() {
            Error::status(code, summary).next(next)
        } else {
            Error::status(code, format!("{summary}: {detail}")).next_line(next)
        })
    };
    if [
        "no capacity",
        "no longer available",
        "offer not available",
        "offer does not exist",
        "no such offer",
        "no such ask",
        // Vast's 410 slug when `--cancel-unavail` finds the host cannot start it.
        "no_such_ask",
        "not rentable",
    ]
    .iter()
    .any(|s| lower.contains(s))
    {
        return with_detail(
            ExitCode::NoCapacity,
            "Vast has no capacity for this offer",
            "gpu up with another --gpu model or --offer ID",
        );
    }
    if [
        "no api key",
        "no api-key",
        "api key not found",
        "api-key not found",
    ]
    .iter()
    .any(|s| lower.contains(s))
        || ((lower.contains("api key") || lower.contains("api-key"))
            && (lower.contains("not set") || lower.contains("missing")))
    {
        return with_detail(
            ExitCode::NotSetup,
            "Vast is not set up",
            "gpu setup --provider vast",
        );
    }
    if [
        "unauthorized",
        "invalid api key",
        "invalid api-key",
        "invalid_api_key",
        "authentication failed",
        "not authenticated",
        "token expired",
    ]
    .iter()
    .any(|s| lower.contains(s))
        || http_status(&lower, "401")
    {
        return with_detail(
            ExitCode::AuthExpired,
            "Vast authentication failed or expired",
            "vastai login",
        );
    }
    if [
        "no such instance",
        "instance not found",
        "instance does not exist",
    ]
    .iter()
    .any(|s| lower.contains(s))
    {
        return with_detail(ExitCode::NotFound, "Vast instance was not found", "gpu ls");
    }
    None
}
