#[cfg(test)]
mod tests;
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

/// Error text plus the facts callers act on, so no caller parses prose.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Message {
    text: String,
    /// Byte length of the text before an appended next step.
    summary: usize,
    /// The command this error recommends running next.
    next: Option<String>,
    /// The provider refused the request outright, so nothing was created.
    rejected: bool,
}

impl Message {
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl From<String> for Message {
    fn from(text: String) -> Self {
        Self {
            summary: text.len(),
            text,
            next: None,
            rejected: false,
        }
    }
}

impl From<&str> for Message {
    fn from(text: &str) -> Self {
        text.to_string().into()
    }
}

impl std::ops::Deref for Message {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// How a command ends other than success.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// A handled error: `gpu: <redacted message>` on stderr, exit 1.
    Msg(Message),
    /// A provider read can be retried without changing paid resources.
    ProviderUnavailable(Message),
    /// A subprocess deadline elapsed; mutations may have completed remotely.
    Timeout(Message),
    Interrupted(i32),
    Status(ExitCode, Message),
    /// An SSH/scp process completed; do not replace its diagnostics or exit code.
    Remote(i32),
    /// The user quit; `Cancelled.` on stdout and this exit status.
    Cancelled(i32),
    /// Actionable missing-input text or clap-rendered usage; stderr, exit 2.
    Usage(Message),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn msg(text: impl Into<String>) -> Error {
        Error::Msg(text.into().into())
    }

    pub fn status(code: ExitCode, text: impl Into<String>) -> Error {
        Error::Status(code, text.into().into())
    }

    pub fn usage(text: impl Into<String>) -> Error {
        Error::Usage(text.into().into())
    }

    pub fn unavailable(text: impl Into<String>) -> Error {
        Error::ProviderUnavailable(text.into().into())
    }

    pub fn timeout(text: impl Into<String>) -> Error {
        Error::Timeout(text.into().into())
    }

    fn message_mut(&mut self) -> Option<&mut Message> {
        match self {
            Error::Msg(message)
            | Error::Usage(message)
            | Error::Status(_, message)
            | Error::ProviderUnavailable(message)
            | Error::Timeout(message) => Some(message),
            Error::Interrupted(_) | Error::Cancelled(_) | Error::Remote(_) => None,
        }
    }

    fn message(&self) -> Option<&Message> {
        match self {
            Error::Msg(message)
            | Error::Usage(message)
            | Error::Status(_, message)
            | Error::ProviderUnavailable(message)
            | Error::Timeout(message) => Some(message),
            Error::Interrupted(_) | Error::Cancelled(_) | Error::Remote(_) => None,
        }
    }

    fn append_next(mut self, separator: &str, command: String) -> Error {
        if let Some(message) = self.message_mut() {
            message.text = format!("{}{separator}{command}", &message.text[..message.summary]);
            message.next = Some(command);
        }
        self
    }

    /// Append `. Next: COMMAND` and record COMMAND as the recovery step.
    pub fn next(self, command: impl Into<String>) -> Error {
        self.append_next(". Next: ", command.into())
    }

    /// Append `\nNext: COMMAND` and record COMMAND as the recovery step.
    pub fn next_line(self, command: impl Into<String>) -> Error {
        self.append_next("\nNext: ", command.into())
    }

    /// Record the recovery command that the message text already names.
    pub fn recovers_with(mut self, command: impl Into<String>) -> Error {
        if let Some(message) = self.message_mut() {
            message.next = Some(command.into());
        }
        self
    }

    /// Mark a definitive provider refusal: the request created nothing.
    pub fn rejected(mut self) -> Error {
        if let Some(message) = self.message_mut() {
            message.rejected = true;
        }
        self
    }

    /// Replace the text with fresh context, keeping a stable exit status;
    /// every other kind becomes a handled error.
    pub fn with_text(self, text: impl Into<String>) -> Error {
        match self {
            Error::Status(code, _) => Error::status(code, text),
            _ => Error::msg(text),
        }
    }

    /// The text without its appended next step.
    pub fn summary(&self) -> &str {
        self.message()
            .map_or("", |message| &message.text[..message.summary])
    }

    /// The command this error recommends running next.
    pub fn next_step(&self) -> Option<&str> {
        self.message().and_then(|message| message.next.as_deref())
    }

    pub fn is_rejection(&self) -> bool {
        self.message().is_some_and(|message| message.rejected)
    }

    pub fn not_found(id: &str) -> Error {
        Self::status(
            ExitCode::NotFound,
            format!("Instance {id} not found in the account"),
        )
        .next("gpu ls")
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Msg(_) | Self::ProviderUnavailable(_) | Self::Timeout(_) => 1,
            Self::Interrupted(code) => *code,
            Self::Usage(_) => 2,
            Self::Status(code, _) => *code as i32,
            Self::Cancelled(code) | Self::Remote(code) => *code,
        }
    }

    pub fn code_name(&self) -> &'static str {
        match self {
            Self::Status(code, _) => code.name(),
            Self::Usage(_) => "usage",
            Self::Cancelled(_) => "cancelled",
            Self::Interrupted(_) => "interrupted",
            Self::Timeout(_) => "timeout",
            Self::ProviderUnavailable(_) => "provider_unavailable",
            Self::Remote(_) => "remote_exit",
            Self::Msg(_) => "error",
        }
    }

    pub fn retryable(&self) -> bool {
        matches!(self, Self::ProviderUnavailable(_) | Self::Timeout(_))
    }

    pub fn key(key: &str) -> Error {
        Error::msg(format!("Required field missing: {}.", repr_str(key)))
    }

    /// Message text for handled errors (empty for the other kinds).
    pub fn text(&self) -> &str {
        match self {
            Error::Interrupted(_) => {
                "Command interrupted; inspect the partial result and gpu ls before retrying."
            }
            error => error.message().map_or("", Message::as_str),
        }
    }
}

impl From<String> for Error {
    fn from(text: String) -> Self {
        Error::msg(text)
    }
}

impl From<&str> for Error {
    fn from(text: &str) -> Self {
        Error::msg(text)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::msg(os_message(&err, None))
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
    Error::msg(os_message(err, Some(path)))
}
