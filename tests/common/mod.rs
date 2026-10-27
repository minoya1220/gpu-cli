//! Runs the real `gpu` binary under a temp HOME and XDG directories with
//! stub executables first on PATH. Every stub logs its argv and fails on an
//! unexpected call, so a missing fake can never fall through to a real
//! `vastai`, `gh`, `ssh`, `rsync` or editor installed on the machine.

#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Answers like the Python tests' FAKE_CLI: logs the call, waits 0.3 s like a
/// network-bound CLI, and knows only the repository list and offer search.
pub const FAKE_CLI: &str = r#"#!/bin/sh
set -f
tool=$(basename "$0")
{ printf '%s' "$tool"; for arg in "$@"; do printf '\t%s' "$arg"; done; printf '\n'; } >> "$FAKE_CLI_LOG"
sleep "${FAKE_CLI_DELAY:-0.3}"
case "$tool $1 $2" in
    "gh repo list")
        echo '[{"name": "gpu-kernels"}, {"name": "other-repo"}]'
        ;;
    "vastai search offers")
        gpu=
        for term in $3; do
            case $term in gpu_name=*) gpu=$(printf '%s' "${term#gpu_name=}" | tr _ ' ') ;; esac
        done
        offer() {
            printf '{"id": %s, "gpu_name": "%s", "num_gpus": 1, "dph_total": %s, "reliability": 0.99, "geolocation": "Test, XX", "verification": "verified"}' "$1" "$2" "$3"
        }
        if [ -n "$gpu" ]; then
            printf '[%s]\n' "$(offer 100 "$gpu" 0.3)"
        else
            printf '[%s, %s]\n' "$(offer 100 "RTX 4090" 0.3)" "$(offer 101 "Tesla T4" 0.4)"
        fi
        ;;
    *)
        echo "unexpected call: $tool $*" >&2
        exit 97
        ;;
esac
"#;

/// Logs the call and fails: for tools a test must never reach.
pub const FORBIDDEN: &str = r#"#!/bin/sh
tool=$(basename "$0")
{ printf '%s' "$tool"; for arg in "$@"; do printf '\t%s' "$arg"; done; printf '\n'; } >> "$FAKE_CLI_LOG"
echo "unexpected call: $tool $*" >&2
exit 97
"#;

pub struct Sandbox {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
    pub bin: PathBuf,
    pub log: PathBuf,
}

impl Sandbox {
    pub fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        for sub in ["home", "cache", "config", "state", "bin"] {
            std::fs::create_dir(root.join(sub)).unwrap();
        }
        let log = root.join("calls.log");
        std::fs::write(&log, "").unwrap();
        let sandbox = Sandbox {
            home: root.join("home"),
            bin: root.join("bin"),
            log,
            root,
            _dir: dir,
        };
        for tool in ["gh", "vastai"] {
            sandbox.stub(tool, FAKE_CLI);
        }
        for tool in ["ssh", "rsync", "cursor", "code", "zed", "curl"] {
            sandbox.stub(tool, FORBIDDEN);
        }
        sandbox
    }

    pub fn stub(&self, name: &str, script: &str) {
        let path = self.bin.join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub fn env(&self) -> Vec<(String, String)> {
        let path = |p: &Path| p.to_string_lossy().into_owned();
        vec![
            ("HOME".into(), path(&self.home)),
            ("XDG_CACHE_HOME".into(), path(&self.root.join("cache"))),
            ("XDG_CONFIG_HOME".into(), path(&self.root.join("config"))),
            ("XDG_STATE_HOME".into(), path(&self.root.join("state"))),
            ("PATH".into(), format!("{}:/usr/bin:/bin", path(&self.bin))),
            ("TZ".into(), "UTC".into()),
            ("TERM".into(), "xterm-256color".into()),
            ("FAKE_CLI_LOG".into(), path(&self.log)),
        ]
    }

    /// The binary with only this sandbox's environment.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gpu"));
        command.args(args).env_clear().envs(self.env());
        command
    }

    /// Logged calls, one argv per line.
    pub fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.log)
            .unwrap()
            .lines()
            .map(|line| line.split('\t').map(String::from).collect())
            .collect()
    }

    pub fn history_path(&self) -> PathBuf {
        self.root.join("state").join("gpu").join("history.json")
    }
}
