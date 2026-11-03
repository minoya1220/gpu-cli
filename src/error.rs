use std::io;
use std::path::Path;

use crate::pyfmt::repr_str;

/// How a command ends other than success.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// A handled error: `gpu: <redacted message>` on stderr, exit 1.
    Msg(String),
    /// The user quit; `Cancelled.` on stdout and this exit status.
    Cancelled(i32),
    /// A usage error, already rendered like clap's; stderr, exit 2.
    Usage(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn msg(text: impl Into<String>) -> Error {
        Error::Msg(text.into())
    }

    /// Python's `KeyError` text for a missing key.
    pub fn key(key: &str) -> Error {
        Error::Msg(format!("Required field missing: {}.", repr_str(key)))
    }

    /// Message text for handled errors (empty for the other kinds).
    pub fn text(&self) -> &str {
        match self {
            Error::Msg(text) | Error::Usage(text) => text,
            Error::Cancelled(_) => "",
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
