//! Translate known provider failure signatures once, without exposing secrets.
use crate::error::{Error, ExitCode};

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
        Some(Error::status(
            code,
            if detail.is_empty() {
                format!("{summary}. Next: {next}")
            } else {
                format!("{summary}: {detail}\nNext: {next}")
            },
        ))
    };
    if [
        "no capacity",
        "no longer available",
        "offer not available",
        "offer does not exist",
        "no such offer",
        "no such ask",
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
