//! Translate known provider failure signatures once, without exposing secrets.
use crate::error::{Error, ExitCode};

pub(crate) fn vast(detail: &str) -> Option<Error> {
    let lower = detail.to_ascii_lowercase();
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
        return Some(Error::status(
            ExitCode::NotSetup,
            "Vast is not set up. Next: gpu setup --provider vast",
        ));
    }
    if [
        "unauthorized",
        "invalid api key",
        "invalid api-key",
        "invalid_api_key",
        "authentication",
        "login",
        "401",
        "token expired",
    ]
    .iter()
    .any(|s| lower.contains(s))
    {
        return Some(Error::status(
            ExitCode::AuthExpired,
            "Vast authentication failed or expired. Next: vastai login",
        ));
    }
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
        return Some(Error::status(ExitCode::NoCapacity,
            "Vast has no capacity for this offer. Next: gpu up with another --gpu model or --offer ID"));
    }
    None
}
