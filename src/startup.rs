//! The generated onstart script for container and VM instances.

use std::io::Write;

use jiff::Timestamp;

use crate::config;
use crate::error::{Error, Result};
use crate::expiry::{deadline_seconds, GUARD_SOURCE};
use crate::pyfmt::{py_str, shell_quote, shell_quote_value};
use crate::pyjson::Dict;
use crate::repos::{resolve_repo, Repo};

/// Vast's limit on the onstart field. The API reference gives 4048
/// characters ("Key Quirks": "`onstart` field is limited to 4048 characters
/// -- gzip+base64 for longer scripts"); the CLI guide says 16KB. vastai 1.8.2
/// checks nothing locally, so the smaller documented limit is enforced here.
pub const ONSTART_LIMIT: usize = 4048;

/// Onstart lines that install `remote/expiry_guard.sh` and start it with the
/// absolute `deadline`, so the instance destroys itself then even if the CLI
/// never reaches SSH. Onstart runs again when Vast starts the instance, and
/// the guard then resumes the latest deadline. The guard travels gzip+base64
/// without its comment lines, as Vast documents for long onstart scripts.
pub fn expiry_guard(deadline: Timestamp) -> Vec<String> {
    let code: String = GUARD_SOURCE
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .map(|line| format!("{line}\n"))
        .collect();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    gzip.write_all(code.as_bytes())
        .expect("writing to memory cannot fail");
    let packed = base64(&gzip.finish().expect("writing to memory cannot fail"));
    vec![
        "GPU_EXPIRE=${HOME:-/root}/.local/state/gpu-expire".into(),
        format!(
            "mkdir -p \"$GPU_EXPIRE\" && chmod 700 \"$GPU_EXPIRE\" && echo {packed} | base64 -d | gunzip >\"$GPU_EXPIRE/guard.sh\" && (setsid bash \"$GPU_EXPIRE/guard.sh\" boot {} </dev/null >/dev/null 2>&1 &)",
            deadline_seconds(deadline)
        ),
    ]
}

/// Standard base64 with padding, on one line.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            text.push(if i <= chunk.len() {
                char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize])
            } else {
                '='
            });
        }
    }
    text
}

/// Minimal job image startup: no GitHub auth, cloning, or apt work.
pub fn run_script(deadline: Timestamp) -> Result<String> {
    let mut lines = vec!["#!/bin/bash".to_string()];
    lines.extend(expiry_guard(deadline));
    lines.extend(
        [
            "touch /root/.no_auto_tmux",
            "mkdir -p /workspace",
            "export PATH=\"/root/.local/bin:$PATH\"",
            "command -v uv >/dev/null 2>&1 || (curl -LsSf https://astral.sh/uv/install.sh | sh)",
            "touch /root/.works",
            "entrypoint.sh",
        ]
        .map(String::from),
    );
    let script = lines.join("\n") + "\n";
    if script.len() > ONSTART_LIMIT {
        return Err(Error::msg("Job startup exceeds Vast's onstart limit."));
    }
    Ok(script)
}

fn git_identity(cfg: &Dict, key: &str) -> Result<String> {
    let value = config::get(cfg, key)?;
    shell_quote_value(value).map_err(|_| {
        Error::msg(format!(
            "Config '{key}' must be a string, got {}.",
            value.kind()
        ))
    })
}

/// The onstart script; with a `deadline`, the expiry guard starts first.
pub fn startup_script(
    repo: &Repo,
    mode: &str,
    cfg: &Dict,
    deadline: Option<Timestamp>,
) -> Result<String> {
    if mode != "container" && mode != "vm" {
        return Err(Error::msg(format!("Unsupported mode: {mode}")));
    }
    let (clone_url, name) = resolve_repo(repo.as_arg(), &py_str(config::get(cfg, "owner")?))?;
    let target = format!("/workspace/{name}");
    let mut lines: Vec<String> = vec!["#!/bin/bash".into()];
    lines.extend(deadline.map(expiry_guard).unwrap_or_default());
    lines.extend([
        "source /etc/environment 2>/dev/null || true".into(),
        "export GH_TOKEN".into(),
    ]);
    if mode == "container" {
        lines.extend(
            [
                "apt update && apt install -y gh",
                "touch /root/.no_auto_tmux",
                "rm -rf /venv",
            ]
            .map(String::from),
        );
    } else {
        lines.extend([
            "if ! command -v ncu >/dev/null 2>&1; then cd /tmp; wget -q https://developer.download.nvidia.com/compute/cuda/repos/ubuntu2204/x86_64/cuda-keyring_1.1-1_all.deb; dpkg -i cuda-keyring_1.1-1_all.deb; apt update; apt install -y gh git curl build-essential nsight-compute-2025.4.0 cuda-nvcc-12-8; fi".to_string(),
            "grep -q nsight-compute /root/.bashrc || cat >> /root/.bashrc <<'EOF'\nexport PATH=$PATH:/opt/nvidia/nsight-compute/2025.4.0:/usr/local/cuda-12.8/bin\nexport LD_LIBRARY_PATH=${LD_LIBRARY_PATH:-}:/usr/local/cuda-12.8/lib64\nexport PATH=\"/root/.local/bin:$PATH\"\nEOF".to_string(),
            "export PATH=$PATH:/opt/nvidia/nsight-compute/2025.4.0:/usr/local/cuda-12.8/bin".to_string(),
            "command -v uv >/dev/null 2>&1 || (curl -LsSf https://astral.sh/uv/install.sh | sh)".to_string(),
        ]);
    }
    lines.push("mkdir -p /workspace".into());
    lines.push(format!(
        "git config --global user.name {}",
        git_identity(cfg, "git_name")?
    ));
    lines.push(format!(
        "git config --global user.email {}",
        git_identity(cfg, "git_email")?
    ));
    lines.push(r#"if [ -n "${GH_TOKEN:-}" ]; then gh auth setup-git; else echo "WARNING: GH_TOKEN is not set in Vast account environment; private GitHub repos may not clone." >&2; fi"#.into());
    if *repo != Repo::NoRepo {
        lines.push(format!(
            "[ -d {} ] || git clone {} {}",
            shell_quote(&target),
            shell_quote(&clone_url),
            shell_quote(&target)
        ));
        lines.push(format!(
            "if [ -f {} ]; then uv sync --directory {}; fi",
            shell_quote(&format!("{target}/pyproject.toml")),
            shell_quote(&target)
        ));
    }
    if mode == "container" {
        lines.push(r#"sed -i 's|^cd \${WORKSPACE}$|[ "$PWD" = "$HOME" ] \&\& cd "${WORKSPACE:-/workspace}"|' /root/.bashrc"#.into());
        lines.push("touch /root/.works".into());
        lines.push("entrypoint.sh".into());
    } else {
        lines.push("touch /root/.works".into());
    }
    let script = lines.join("\n") + "\n";
    if script.len() > ONSTART_LIMIT {
        return Err(Error::msg(format!(
            "Startup script is {} characters; Vast accepts at most {ONSTART_LIMIT}. Use a shorter repository URL or git identity.",
            script.len()
        )));
    }
    Ok(script)
}
