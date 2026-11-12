//! Managed `vast-<id>` SSH aliases in `~/.ssh/vst_config`.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::ctx::Env;
use crate::error::{os_error, Error, Result};
use crate::pyfmt::{is_digits, py_str};
use crate::pyjson::{Dict, Value};

const INCLUDE: &[u8] = b"Include ~/.ssh/vst_config\n";
static HOST_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^Host vast-(\d+)\n(?:[ \t]+[^\n]*\n)*").unwrap());
static INCLUDE_LINE: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
    regex::bytes::Regex::new(r"(?i-u)^[ \t]*Include[ \t]+~/.ssh/vst_config[ \t\r]*$").unwrap()
});
static HOST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9.-]+$").unwrap());

fn ssh_dir(env: &Env) -> PathBuf {
    env.home().join(".ssh")
}

fn managed_path(env: &Env) -> PathBuf {
    ssh_dir(env).join("vst_config")
}

/// `bytes.splitlines()`: splits on `\n`, `\r\n` and `\r`.
fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'\n' => {
                lines.push(&data[start..i]);
                start = i + 1;
            }
            b'\r' => {
                lines.push(&data[start..i]);
                if data.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < data.len() {
        lines.push(&data[start..]);
    }
    lines
}

/// `Path.read_text()`, with its universal newlines: `\r\n` and `\r` read as `\n`.
fn read_text(path: &Path) -> Result<String> {
    let text = std::fs::read_to_string(path).map_err(|e| os_error(&e, path))?;
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

fn ensure_include(env: &Env) -> Result<()> {
    let directory = ssh_dir(env);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .map_err(|e| os_error(&e, &directory))?;
    let config = directory.join("config");
    crate::atomic_file::update(&config, |original| {
        if !split_lines(original)
            .iter()
            .any(|line| INCLUDE_LINE.is_match(line))
        {
            // The Include must precede all Host/Match blocks for SSH precedence.
            let mut content = INCLUDE.to_vec();
            content.extend_from_slice(original);
            Ok(content)
        } else {
            Ok(original.to_vec())
        }
    })
}

fn without_entry(content: &str, instance_id: &str) -> String {
    HOST_BLOCK
        .replace_all(content, |caps: &regex::Captures| {
            if &caps[1] == instance_id {
                String::new()
            } else {
                caps[0].to_string()
            }
        })
        .into_owned()
}

/// Why `update_host` did not write an alias.
#[derive(Debug)]
pub enum HostError {
    /// The SSH endpoint is not provisioned yet; `wait_for_ssh` keeps polling.
    NotReady(String),
    Failed(Error),
}

impl From<Error> for HostError {
    fn from(err: Error) -> Self {
        HostError::Failed(err)
    }
}

impl From<HostError> for Error {
    fn from(err: HostError) -> Self {
        match err {
            HostError::NotReady(text) => Error::Msg(text),
            HostError::Failed(err) => err,
        }
    }
}

/// Write the instance's managed alias.
pub fn update_host(env: &Env, instance: &Dict) -> std::result::Result<String, HostError> {
    let instance_id = py_str(instance.get("id").ok_or_else(|| Error::key("id"))?);
    if !is_digits(&instance_id) {
        return Err(Error::msg("Invalid instance ID for SSH host").into());
    }
    let host = match instance.get("ssh_host") {
        Some(Value::Str(host)) if HOST.is_match(host) => host.clone(),
        _ => {
            return Err(HostError::NotReady(format!(
                "Instance {instance_id} has no usable SSH host yet"
            )))
        }
    };
    let port = py_str(instance.get("ssh_port").unwrap_or(&Value::Null));
    if !is_digits(&port) || !port.parse::<u64>().is_ok_and(|p| (1..=65535).contains(&p)) {
        return Err(HostError::NotReady(format!(
            "Instance {instance_id} has no usable SSH port yet"
        )));
    }
    ensure_include(env)?;
    let path = managed_path(env);
    let entry = format!(
        "Host vast-{instance_id}\n  HostName {host}\n  Port {port}\n  User root\n  IdentityFile \
            ~/.ssh/id_ed25519\n  StrictHostKeyChecking accept-new\n  ServerAliveInterval 15\n  ServerAliveCountMax 4\n"
    );
    crate::atomic_file::update(&path, |original| {
        let current = String::from_utf8_lossy(original)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        // Preserve ordering and avoid a rename on every readiness poll.
        if HOST_BLOCK
            .captures_iter(&current)
            .any(|caps| caps[1] == instance_id && caps[0] == entry)
        {
            return Ok(original.to_vec());
        }
        let rest = without_entry(&current, &instance_id);
        Ok(if rest.trim_end_matches('\n').is_empty() {
            entry.clone()
        } else {
            format!("{}\n{entry}", rest.trim_end_matches('\n'))
        }
        .into_bytes())
    })?;
    Ok(format!("vast-{instance_id}"))
}

/// Whether gpu has written a managed stanza for this instance.
pub fn has_host(env: &Env, instance_id: &str) -> Result<bool> {
    let path = managed_path(env);
    if !path.exists() {
        return Ok(false);
    }
    let content = read_text(&path)?;
    Ok(HOST_BLOCK
        .captures_iter(&content)
        .any(|caps| &caps[1] == instance_id))
}

/// Remove only this instance's managed stanza; never edit user SSH config.
pub fn remove_host(env: &Env, instance_id: &str) -> Result<()> {
    let path = managed_path(env);
    if !path.exists() {
        return Ok(());
    }
    crate::atomic_file::update(&path, |original| {
        Ok(without_entry(&String::from_utf8_lossy(original), instance_id).into_bytes())
    })
}
