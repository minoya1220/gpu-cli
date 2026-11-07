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
        }
    }
}

/// How a command ends other than success.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// A handled error: `gpu: <redacted message>` on stderr, exit 1.
    Msg(String),
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

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Msg(_) => 1,
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
        }
    }

    /// Python's `KeyError` text for a missing key.
    pub fn key(key: &str) -> Error {
        Error::Msg(format!("Required field missing: {}.", repr_str(key)))
    }

    /// Message text for handled errors (empty for the other kinds).
    pub fn text(&self) -> &str {
        match self {
            Error::Msg(text) | Error::Usage(text) | Error::Status(_, text) => text,
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

/// Python's `OSError` text: `[Errno 2] No such file or directory: 'path'`.
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
