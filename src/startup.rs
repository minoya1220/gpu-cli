//! The generated onstart script for container and VM instances.

use crate::config;
use crate::error::{Error, Result};
use crate::pyfmt::{py_str, shell_quote, shell_quote_value};
use crate::pyjson::Dict;
use crate::repos::{resolve_repo, Repo};

pub fn startup_script(repo: &Repo, mode: &str, cfg: &Dict) -> Result<String> {
    if mode != "container" && mode != "vm" {
        return Err(Error::msg(format!("Unsupported mode: {mode}")));
    }
    let (clone_url, name) = resolve_repo(repo.as_arg(), &py_str(config::get(cfg, "owner")?))?;
    let target = format!("/workspace/{name}");
    let mut lines: Vec<String> = vec![
        "#!/bin/bash".into(),
        "source /etc/environment 2>/dev/null || true".into(),
        "export GH_TOKEN".into(),
    ];
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
        shell_quote_value(config::get(cfg, "git_name")?)?
    ));
    lines.push(format!(
        "git config --global user.email {}",
        shell_quote_value(config::get(cfg, "git_email")?)?
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
    Ok(lines.join("\n") + "\n")
}
