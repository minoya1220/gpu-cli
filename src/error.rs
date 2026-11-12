use std::io;
use std::path::Path;

use crate::pyfmt::repr_str;

/// Stable CLI failure statuses (remote commands preserve their own exit status).
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(i32)]
pub enum ExitCode {
    NoCapacity = 10,
    OverMaxPrice = 11,
    NotSetup = 12,
    AuthExpired = 13,
    WaitTimeout = 14,
    SpendLimit = 15,
    NotFound = 16,
    JobTimeout = 17,
    InstanceLost = 18,
    CleanupUnverified = 19,
    CommandTimeout = 20,
}

impl ExitCode {
    pub fn name(self) -> &'static str {
        match self {
            Self::NoCapacity => "no_capacity",
            Self::OverMaxPrice => "over_max_price",
            Self::NotSetup => "not_setup",
            Self::AuthExpired => "auth_expired",
            Self::WaitTimeout => "wait_timeout",
            Self::SpendLimit => "spend_limit",
            Self::NotFound => "not_found",
            Self::JobTimeout => "job_timeout",
            Self::InstanceLost => "instance_lost",
            Self::CleanupUnverified => "cleanup_unverified",
            Self::CommandTimeout => "command_timeout",
        }
    }
}

/// Render command help from the same status definitions used by failures.
pub(crate) fn help_codes(run: bool) -> String {
    use ExitCode::*;
    let codes = [
        NoCapacity,
        OverMaxPrice,
        NotSetup,
        AuthExpired,
        WaitTimeout,
        SpendLimit,
        NotFound,
        JobTimeout,
        InstanceLost,
        CleanupUnverified,
        CommandTimeout,
    ];
    codes
        .into_iter()
        .filter(|code| run || !matches!(code, JobTimeout | InstanceLost | CleanupUnverified))
        .map(|code| {
            let label = match code {
                NoCapacity => "capacity",
                OverMaxPrice => "price",
                NotSetup => "setup",
                AuthExpired => "auth",
                WaitTimeout => "readiness timeout",
                SpendLimit => "spend cap",
                NotFound => "not found",
                JobTimeout => "job timeout",
                InstanceLost => "instance lost",
                CleanupUnverified => "cleanup unverified",
                CommandTimeout if run => "helper timeout",
                CommandTimeout => "command timeout",
            };
            format!("{} {label}", code as i32)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// How a command ends other than success.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// A handled error: `gpu: <redacted message>` on stderr, exit 1.
    Msg(String),
    /// A provider read can be retried without changing paid resources.
    ProviderUnavailable(String),
    /// A subprocess deadline elapsed; mutations may have completed remotely.
    Timeout(String),
    Interrupted(i32),
    Status(ExitCode, String),
    /// An SSH/scp process completed; do not replace its diagnostics or exit code.
    Remote(i32),
    /// The user quit; `Cancelled.` on stdout and this exit status.
    Cancelled(i32),
    /// Actionable missing-input text or clap-rendered usage; stderr, exit 2.
    Usage(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn msg(text: impl Into<String>) -> Error {
        Error::Msg(text.into())
    }

    pub fn status(code: ExitCode, text: impl Into<String>) -> Error {
        Error::Status(code, text.into())
    }

    pub fn not_found(id: &str) -> Error {
        Self::status(
            ExitCode::NotFound,
            format!("Instance {id} not found in the account. Next: gpu ls"),
        )
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Msg(_) | Self::ProviderUnavailable(_) | Self::Timeout(_) => 1,
            Self::Interrupted(code) => *code,
            Self::Usage(_) => 2,
            Self::Cancelled(code) | Self::Remote(code) => *code,
            Self::Status(code, _) => *code as i32,
        }
    }

    pub fn code_name(&self) -> &'static str {
        match self {
            Self::Status(code, _) => code.name(),
            Self::Usage(_) => "usage",
            Self::Cancelled(_) => "cancelled",
            Self::Remote(_) => "remote_exit",
            Self::Msg(_) => "error",
            Self::ProviderUnavailable(_) => "provider_unavailable",
            Self::Timeout(_) => "timeout",
            Self::Interrupted(_) => "interrupted",
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, Self::ProviderUnavailable(_) | Self::Timeout(_))
    }

    pub fn key(key: &str) -> Error {
        Error::Msg(format!("Required field missing: {}.", repr_str(key)))
    }

    /// Message text for handled errors (empty for the other kinds).
    pub fn text(&self) -> &str {
        match self {
            Error::Msg(text)
            | Error::Usage(text)
            | Error::Status(_, text)
            | Error::ProviderUnavailable(text)
            | Error::Timeout(text) => text,
            Error::Interrupted(_) => {
                "Command interrupted; inspect the partial result and gpu ls before retrying."
            }
            Error::Cancelled(_) | Error::Remote(_) => "",
        }
    }
}

impl From<String> for Error {
    fn from(text: String) -> Self {
        Error::Msg(text)
    }
}

impl From<&str> for Error {
    fn from(text: &str) -> Self {
        Error::Msg(text.to_string())
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Msg(os_message(&err, None))
    }
}

pub fn os_message(err: &io::Error, path: Option<&Path>) -> String {
    let Some(code) = err.raw_os_error() else {
        return err.to_string();
    };
    let text = io::Error::from_raw_os_error(code).to_string();
    let suffix = format!(" (os error {code})");
    let text = text.strip_suffix(&suffix).unwrap_or(&text);
    match path {
        Some(path) => format!(
            "[Errno {code}] {text}: {}",
            repr_str(&path.to_string_lossy())
        ),
        None => format!("[Errno {code}] {text}"),
    }
}

pub fn os_error(err: &io::Error, path: &Path) -> Error {
    Error::Msg(os_message(err, Some(path)))
}
