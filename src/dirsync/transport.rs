//! Everything that crosses SSH for directory sync: the remote worker, the
//! initial archive upload and the rsync pull. All of it goes through the
//! `Runner`, so tests substitute a fake instance.
use std::collections::BTreeMap;
use std::path::Path;

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::process::{Cmd, Input, Output, RunError};
use crate::pyfmt::{shell_join, shell_quote};

/// The remote worker, sent over SSH stdin and run with the instance's bash.
pub(crate) const WORKER: &str = include_str!("../../remote/sync_remote.sh");

/// A file as the instance lists it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RemoteFile {
    pub size: u64,
    pub mtime: String,
}

pub(crate) type Manifest = BTreeMap<String, RemoteFile>;

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Listing {
    pub sig: String,
    pub files: Manifest,
}

#[derive(Debug, PartialEq)]
pub(crate) struct Prepared {
    pub existing: u64,
    pub inotify: bool,
}

pub(crate) struct Remote<'a> {
    ctx: &'a Ctx,
    pub alias: String,
}

fn ssh_argv(alias: &str, command: &str) -> Vec<String> {
    let mut argv = vec!["ssh".to_string(), "-T".into()];
    argv.extend(crate::ssh::options(true));
    argv.extend([
        "-o".into(),
        "ControlMaster=no".into(),
        "-o".into(),
        "LogLevel=ERROR".into(),
        alias.into(),
        command.into(),
    ]);
    argv
}

/// The `-e` value rsync runs for its remote shell.
fn rsync_shell() -> String {
    let mut words = vec!["ssh".to_string()];
    words.extend(crate::ssh::options(true));
    words.extend(
        ["-o", "ControlMaster=no", "-o", "LogLevel=ERROR"]
            .into_iter()
            .map(String::from),
    );
    words.join(" ")
}

/// The first `key=value` line for `key`.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

fn parse_listing(text: &str) -> Listing {
    let mut listing = Listing {
        sig: field(text, "sig").unwrap_or_default().to_string(),
        files: Manifest::new(),
    };
    for line in text.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(size), Some(mtime), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if let (Ok(size), true) = (size.parse::<u64>(), super::store::safe_relative(path)) {
            listing.files.insert(
                path.to_string(),
                RemoteFile {
                    size,
                    mtime: mtime.to_string(),
                },
            );
        }
    }
    listing
}

impl<'a> Remote<'a> {
    pub fn new(ctx: &'a Ctx, alias: impl Into<String>) -> Self {
        Remote {
            ctx,
            alias: alias.into(),
        }
    }

    fn describe(&self, error: RunError, what: &str) -> Error {
        match error {
            RunError::TimedOut => Error::timeout(format!("{what} timed out")),
            other => Error::msg(other.describe("ssh")),
        }
    }

    fn worker(&self, action: &str, args: &[&str], timeout: f64) -> Result<Output> {
        let mut words = vec!["bash", "-s", "--", action];
        words.extend_from_slice(args);
        let cmd = Cmd::new(&ssh_argv(&self.alias, &shell_join(&words)))
            .stdin(Input::Text(WORKER.to_string()))
            .timeout(Some(timeout));
        let output = self
            .ctx
            .run(&cmd)
            .map_err(|e| self.describe(e, "Listing the instance's files"))?;
        if let Some(error) = field(&output.stdout, "error") {
            return Err(Error::msg(format!(
                "The instance cannot be synced ({error}); check the directory exists and is writable."
            )));
        }
        if output.code != 0 {
            let reason = if output.code == 255 {
                "SSH could not connect".to_string()
            } else {
                format!("the worker exited with status {}", output.code)
            };
            return Err(Error::unavailable(format!(
                "Cannot read the instance's files: {reason}. {}",
                crate::process::failure_detail(&output)
            )));
        }
        Ok(output)
    }

    /// Create the directory and make sure rsync is there.
    pub fn prepare(&self, dir: &str) -> Result<Prepared> {
        let output = self.worker("prepare", &[dir], 180.0)?;
        if field(&output.stdout, "rsync") != Some("ok") {
            return Err(Error::msg(
                "rsync is not installed on the instance and could not be installed. Install rsync there (apt-get install rsync) and retry.",
            ));
        }
        Ok(Prepared {
            existing: field(&output.stdout, "existing")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
            inotify: field(&output.stdout, "watcher") == Some("inotify"),
        })
    }

    pub fn manifest(&self, dir: &str) -> Result<Listing> {
        let output = self.worker("manifest", &[dir], 120.0)?;
        Ok(parse_listing(&output.stdout))
    }

    /// Block up to `seconds` for a change since `sig`; `None` when nothing changed.
    pub fn wait(&self, dir: &str, seconds: u32, sig: &str) -> Result<Option<Listing>> {
        let output = self.worker(
            "wait",
            &[dir, &seconds.to_string(), sig],
            f64::from(seconds) + 90.0,
        )?;
        Ok((field(&output.stdout, "status") == Some("changed"))
            .then(|| parse_listing(&output.stdout)))
    }

    /// Extract the archive into `dir` on the instance.
    pub fn upload(&self, dir: &str, archive: &Path) -> Result<()> {
        let command = format!(
            "mkdir -p {dir} && tar -xzf - --no-same-owner -C {dir}",
            dir = shell_quote(dir)
        );
        let cmd = Cmd::new(&ssh_argv(&self.alias, &command))
            .stdin(Input::File(archive.into()))
            .timeout(Some(3600.0));
        let output = self
            .ctx
            .run(&cmd)
            .map_err(|e| self.describe(e, "The upload"))?;
        if output.code != 0 {
            return Err(Error::msg(format!(
                "Uploading the directory failed (status {}): {}",
                output.code,
                crate::process::failure_detail(&output)
            )));
        }
        Ok(())
    }

    /// Copy `list` (relative paths, one per line) from the instance's `dir`
    /// into `staging`, keeping the relative layout.
    pub fn pull(&self, dir: &str, list: &Path, staging: &Path) -> Result<()> {
        let argv = vec![
            "rsync".to_string(),
            "-t".into(),
            "-p".into(),
            format!("--files-from={}", list.display()),
            "-e".into(),
            rsync_shell(),
            format!("{}:{}/", self.alias, dir),
            format!("{}/", staging.display()),
        ];
        let output = self
            .ctx
            .run(&Cmd::new(&argv).timeout(Some(7200.0)))
            .map_err(|e| match e {
                RunError::NotFound => Error::msg(
                    "rsync is not installed on this computer. Install it (macOS: brew install rsync; Debian: apt install rsync) and retry.",
                ),
                other => self.describe(other, "The pull"),
            })?;
        // 24: files vanished while copying, normal on a live tree. 23: some could
        // not be read; the caller reports whichever files did not arrive.
        if !matches!(output.code, 0 | 23 | 24) {
            return Err(Error::unavailable(format!(
                "rsync failed (status {}): {}",
                output.code,
                crate::process::failure_detail(&output)
            )));
        }
        Ok(())
    }
}
