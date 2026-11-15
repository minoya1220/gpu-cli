//! Runs the real `gpu` binary under a temp HOME and XDG directories with
//! stub executables first on PATH. Every stub logs its argv and fails on an
//! unexpected call, so a missing fake can never fall through to a real
//! `vastai`, `gcloud`, `gh`, `ssh`, `rsync` or editor installed on the machine.

#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const FAKE_CLI: &str = r#"#!/bin/sh
set -f
tool=$(basename "$0")
entry=$tool; for arg in "$@"; do entry="$entry	$arg"; done
printf '%s\n' "$entry" >> "$FAKE_CLI_LOG"
sleep "${FAKE_CLI_DELAY:-0.3}"
case "$tool $1 $2" in
    "gh api user") echo 'sampleuser' ;;
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
entry=$tool; for arg in "$@"; do entry="$entry	$arg"; done
printf '%s\n' "$entry" >> "$FAKE_CLI_LOG"
echo "unexpected call: $tool $*" >&2
exit 97
"#;

pub fn gcp_config(zone: Option<&str>, runtime: Option<&str>, account: Option<&str>) -> String {
    use gpu::pyjson::{self, Value};
    let mut config = pyjson::loads(include_str!("gcp_setup.json"))
        .unwrap()
        .as_dict()
        .unwrap()
        .clone();
    let mut gcp = config.get("gcp").unwrap().as_dict().unwrap().clone();
    for (key, value) in [("zone", zone), ("runtime", runtime)] {
        if let Some(value) = value {
            gcp.insert(key, Value::str(value));
        }
    }
    if let Some(account) = account {
        let mut expiry = gcp.get("expiry").unwrap().as_dict().unwrap().clone();
        expiry.insert("service_account", Value::str(account));
        gcp.insert("expiry", Value::Dict(expiry));
    }
    config.insert("gcp", Value::Dict(gcp));
    pyjson::dumps(&Value::Dict(config), None)
}

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
        sandbox.stub(
            "git",
            "#!/bin/sh\n[ \"$1\" = config ] && exit 1\nexec /usr/bin/git \"$@\"\n",
        );
        for tool in [
            "ssh",
            "scp",
            "rsync",
            "cursor",
            "code",
            "zed",
            "curl",
            "gcloud",
            "uv",
            "runpodctl",
            "doctl",
        ] {
            sandbox.stub(tool, FORBIDDEN);
        }
        sandbox
    }

    pub fn stub(&self, name: &str, script: &str) {
        let path = self.bin.join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    pub fn gcp_reads(&self, nodes: &str, prices: bool) {
        std::fs::write(
            self.root.join("gcp-nodes.json"),
            format!("{{\"nodes\":{nodes}}}"),
        )
        .unwrap();
        if prices {
            std::fs::write(self.root.join("gcp-prices"), "").unwrap();
        }
        let data = gpu::pyjson::loads(include_str!("tpu_facts.json")).unwrap();
        let cache = self.root.join("cache/gpu");
        std::fs::create_dir_all(&cache).unwrap();
        let mut facts = data.as_dict().unwrap().clone();
        facts.insert(
            "at",
            gpu::pyjson::Value::Int(jiff::Timestamp::now().as_second()),
        );
        for file in ["tpu-catalog-test-project.json", "tpu-sku-facts.json"] {
            std::fs::write(
                cache.join(file),
                gpu::pyjson::dumps(&gpu::pyjson::Value::Dict(facts.clone()), None),
            )
            .unwrap();
        }
        let spot=data.as_dict().unwrap().get("skus").unwrap().as_list().unwrap().iter().filter_map(gpu::pyjson::Value::as_dict).find(|d|matches!(d.get("description"),Some(gpu::pyjson::Value::Str(s)) if s=="TpuV6e attached to Spot Preemptible VMs running in Columbus")).unwrap();
        let gpu::pyjson::Value::Str(id) = spot.get("id").unwrap() else {
            panic!("SKU id")
        };
        self.stub(
            "curl",
            &include_str!("gcp_reads.sh").replace("@SPOT_ID@", id),
        );
    }

    pub fn gcp_lifecycle(&self) {
        let config = self.root.join("config/gpu");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("config.json"), gcp_config(None, None, None)).unwrap();
        self.gcp_reads("[]", true);
        self.stub("gcloud", include_str!("gcp_native.py"));
        self.stub(
            "vastai",
            &FAKE_CLI.replace(
                "case \"$tool $1 $2\" in",
                "case \"$tool $1 $2\" in\n    \"vastai show instances\") echo '[]' ;;",
            ),
        );
        for editor in ["cursor", "code", "zed"] {
            self.stub(editor,r#"#!/bin/sh
{ printf '%s' "$(basename "$0")"; for arg in "$@"; do printf '\t%s' "$arg"; done; printf '\n'; } >> "$FAKE_CLI_LOG"
"#);
        }
    }

    /// Representative mixed listing, including a local price snapshot. Every
    /// request still goes to the existing forbidden-by-default PATH stubs.
    pub fn mixed_listing(&self) {
        self.gcp_lifecycle();
        let now = jiff::Timestamp::now();
        let created = now.checked_sub(jiff::Span::new().minutes(7)).unwrap();
        self.gcp_reads(&format!(r#"[{{"name":"projects/test-project/locations/us-east5-b/nodes/gpu-1794014955-uwhcp7","acceleratorType":"v5litepod-1","state":"DELETING","createTime":"{created}","schedulingConfig":{{"spot":true}}}}]"#), true);
        let cache = self.root.join("cache/gpu");
        std::fs::create_dir_all(&cache).unwrap();
        let at = now.as_second() - 1;
        std::fs::write(cache.join("tpu-prices-test-project.json"), format!(r#"{{"at":{at},"entries":[{{"id":"test-v5e-spot","at":{at},"sku":{{"description":"TPU v5 Lite Pod chip Spot","category":{{"usageType":"Preemptible"}},"serviceRegions":["us-east5"],"pricingInfo":[{{"effectiveTime":"2026-01-01T00:00:00Z","pricingExpression":{{"usageUnit":"chip.h","tieredRates":[{{"unitPrice":{{"currencyCode":"USD","nanos":359000000}}}}]}}}}]}}}}]}}"#)).unwrap();
        self.stub("vastai", &FAKE_CLI.replace(
            "case \"$tool $1 $2\" in",
            "case \"$tool $1 $2\" in\n    \"vastai show instances\") echo '[{\"id\":30306428,\"gpu_name\":\"RTX 4090\",\"actual_status\":\"created\",\"dph_total\":0.10188888888888889,\"duration\":300}]' ;;",
        ));
    }

    pub fn env(&self) -> Vec<(String, String)> {
        let path = |p: &Path| p.to_string_lossy().into_owned();
        let mut env = vec![
            ("HOME".into(), path(&self.home)),
            ("XDG_CACHE_HOME".into(), path(&self.root.join("cache"))),
            ("XDG_CONFIG_HOME".into(), path(&self.root.join("config"))),
            ("XDG_STATE_HOME".into(), path(&self.root.join("state"))),
            ("PATH".into(), format!("{}:/usr/bin:/bin", path(&self.bin))),
            ("TZ".into(), "UTC".into()),
            ("TERM".into(), "xterm-256color".into()),
            ("FAKE_CLI_LOG".into(), path(&self.log)),
        ];
        // Keep instrumented subprocess coverage in cargo-llvm-cov's collector,
        // without inheriting any other host environment (especially credentials).
        if let Ok(profile) = std::env::var("LLVM_PROFILE_FILE") {
            env.push(("LLVM_PROFILE_FILE".into(), profile));
        }
        env
    }

    /// The binary with the sandbox environment and optional coverage output.
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
