//! End to end: the real `gpu up --dry-run` in a pseudo-terminal, keys typed
//! one at a time. Fake `gh` and `vastai` executables on PATH log each call
//! and answer slowly, like the real network-bound CLIs. The screen is read
//! through a terminal emulator, as a person would see it.

mod common;

use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use common::Sandbox;
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};

const ENTER: &str = "\r";
const BACK: &str = "`";
const LEFT: &str = "\x1b[D";
const RIGHT: &str = "\x1b[C";
const DOWN: &str = "\x1b[B";
// Home, End, PageUp, PageDown, Delete, F5, Shift-Left, Shift-Right, Ctrl-Left.
const SPECIAL_KEYS: [&str; 9] = [
    "\x1b[H",
    "\x1b[F",
    "\x1b[5~",
    "\x1b[6~",
    "\x1b[3~",
    "\x1b[15~",
    "\x1b[1;2D",
    "\x1b[1;2C",
    "\x1b[1;5D",
];
const ROWS: u16 = 30;
const COLUMNS: u16 = 100;

struct Terminal {
    master: OwnedFd,
    child: Child,
    parser: vt100::Parser,
    output: Vec<u8>,
    slave_path: std::path::PathBuf,
}

impl Terminal {
    fn start(sandbox: &Sandbox, args: &[&str]) -> Terminal {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let slave_path =
            std::path::PathBuf::from(ptsname(&master, Vec::new()).unwrap().into_string().unwrap());
        let slave = File::options()
            .read(true)
            .write(true)
            .open(&slave_path)
            .unwrap();
        let size = rustix::termios::Winsize {
            ws_row: ROWS,
            ws_col: COLUMNS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        rustix::termios::tcsetwinsize(&slave, size).unwrap();
        let mut command = sandbox.command(args);
        command
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // A new session without a controlling terminal, like Python's start_new_session.
        unsafe {
            command.pre_exec(|| {
                rustix::process::setsid().map_err(std::io::Error::from)?;
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        Terminal {
            master,
            child,
            parser: vt100::Parser::new(
                std::num::NonZeroU16::new(ROWS).unwrap(),
                std::num::NonZeroU16::new(COLUMNS).unwrap(),
                0,
            ),
            output: Vec::new(),
            slave_path,
        }
    }

    fn lines(&self) -> Vec<String> {
        self.parser
            .screen()
            .rows(0, COLUMNS)
            .map(|row| row.trim_end().to_string())
            .filter(|row| !row.trim().is_empty())
            .collect()
    }

    fn title(&self) -> String {
        self.lines().first().cloned().unwrap_or_default()
    }

    fn line(&self, index: usize) -> String {
        self.lines().get(index).cloned().unwrap_or_default()
    }

    fn last_line(&self) -> String {
        self.lines().last().cloned().unwrap_or_default()
    }

    fn pump(&mut self, timeout: Duration) {
        let wait = Timespec {
            tv_sec: 0,
            tv_nsec: timeout.as_nanos() as _,
        };
        let mut fds = [PollFd::new(&self.master, PollFlags::IN)];
        if poll(&mut fds, Some(&wait)).unwrap_or(0) > 0 {
            let mut buffer = [0u8; 65536];
            match rustix::io::read(&self.master, &mut buffer) {
                Ok(n) if n > 0 => {
                    self.output.extend_from_slice(&buffer[..n]);
                    self.parser.process(&buffer[..n]);
                }
                // The child closed the terminal.
                _ => std::thread::sleep(timeout),
            }
        }
    }

    fn wait(&mut self, condition: impl Fn(&Terminal) -> bool) -> Duration {
        let start = Instant::now();
        while !condition(self) {
            if start.elapsed() > Duration::from_secs(5) {
                panic!("timed out; screen:\n{}", self.lines().join("\n"));
            }
            self.pump(Duration::from_millis(5));
        }
        start.elapsed()
    }

    /// Type one key at a person's pace, so every key arrives in its own read.
    fn press(&mut self, key: &str) -> Instant {
        std::thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        rustix::io::write(&self.master, key.as_bytes()).unwrap();
        start
    }

    fn press_until(&mut self, key: &str, until: impl Fn(&Terminal) -> bool) -> Duration {
        let start = self.press(key);
        self.wait(until);
        start.elapsed()
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    fn exit_status(&mut self) -> i32 {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.pump(Duration::from_millis(50));
                return status.code().unwrap_or(-1);
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "still running; screen:\n{}",
                self.lines().join("\n")
            );
            self.pump(Duration::from_millis(5));
        }
    }

    fn output_text(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn on(title: &'static str) -> impl Fn(&Terminal) -> bool {
    move |t| t.title() == title
}

fn shows(text: &'static str) -> impl Fn(&Terminal) -> bool {
    move |t| t.lines().iter().any(|line| line.contains(text))
}

fn tools(sandbox: &Sandbox) -> Vec<String> {
    sandbox
        .calls()
        .into_iter()
        .map(|call| call[0].clone())
        .collect()
}

fn gpu_up(sandbox: &Sandbox) -> Terminal {
    Terminal::start(sandbox, &["up", "--dry-run"])
}

#[test]
fn test_backtick_from_the_deepest_screen_steps_back_to_the_first_then_quits() {
    let sandbox = Sandbox::new();
    let mut term = gpu_up(&sandbox);
    term.wait(shows("gpu-kernels"));
    assert_eq!(term.title(), "Repository");
    term.press_until(ENTER, on("Mode"));
    term.press_until(ENTER, on("GPU"));
    for c in ["4", "0", "9", "0"] {
        term.press(c);
    }
    term.wait(|t| t.line(1) == "Filter: 4090");
    term.press_until(ENTER, on("Offer: RTX 4090"));
    term.wait(shows("id=100"));

    type Shown = Box<dyn Fn(&Terminal) -> bool>;
    let steps: [(&str, Shown); 4] = [
        (
            "GPU",
            Box::new(|t: &Terminal| t.title() == "GPU" && t.line(1) == "Filter: 4090"),
        ),
        (
            "GPU",
            Box::new(|t: &Terminal| t.title() == "GPU" && !t.line(1).starts_with("Filter:")),
        ),
        ("Mode", Box::new(on("Mode"))),
        ("Repository", Box::new(on("Repository"))),
    ];
    for (title, shown) in steps {
        let latency = term.press_until(BACK, shown);
        assert!(term.running(), "quit instead of going back to {title}");
        assert!(
            latency < Duration::from_secs(1),
            "back to {title} took {latency:?}"
        );
    }
    term.press_until(BACK, |t| {
        t.last_line().contains("Press `/\u{2190} again to quit")
    });
    assert!(term.running(), "one ` on the first screen must not quit");
    term.press(BACK);
    assert_eq!(term.exit_status(), 1);
    assert!(term.output_text().contains("Cancelled."));
    assert_eq!(
        tools(&sandbox),
        ["gh", "vastai", "vastai"],
        "navigation must not rerun gh or vastai"
    );
}

#[test]
fn test_keyboard_dry_run_launch_after_going_back() {
    let sandbox = Sandbox::new();
    let mut term = gpu_up(&sandbox);
    term.wait(shows("other-repo"));
    term.press_until(ENTER, on("Mode"));
    term.press_until(ENTER, on("GPU"));
    term.press_until(BACK, on("Mode"));
    term.press_until(BACK, on("Repository"));
    term.press_until("2", on("Mode")); // explicit repository shortcut
    term.press_until("2", on("GPU"));
    for c in ["t", "4"] {
        term.press(c);
    }
    term.press_until(ENTER, on("Offer: Tesla T4"));
    term.wait(shows("id=100"));
    term.press(ENTER);
    assert_eq!(term.exit_status(), 0);
    let output = term.output_text();
    assert!(output.contains("DRY RUN: no instance created"));
    assert!(output.contains("\"100\"") && output.contains("/workspace/other-repo"));
    assert!(output.contains("\"docker.io/vastai/kvm\""));
    assert_eq!(tools(&sandbox), ["gh", "vastai", "vastai", "vastai"]);
}

#[test]
fn test_new_repository_choices_dry_run_in_terminal() {
    for repo in [None, Some("https://github.com/sgl-project/sglang.git")] {
        let sandbox = Sandbox::new();
        let mut term = gpu_up(&sandbox);
        term.wait(shows("No repository"));
        match repo {
            None => {
                term.press_until("0", on("Mode"));
            }
            Some(repo) => {
                term.press_until("9", on("Clone from URL"));
                term.press(&format!("\x1b[200~{repo}\x1b[201~"));
                term.press_until(ENTER, on("Mode"));
            }
        }
        term.press_until("1", on("GPU"));
        for c in ["t", "4"] {
            term.press(c);
        }
        term.press_until(ENTER, on("Offer: Tesla T4"));
        term.wait(shows("id=100"));
        term.press(ENTER);
        assert_eq!(term.exit_status(), 0);
        let output = term.output_text();
        assert!(output.contains("DRY RUN: no instance created"));
        match repo {
            None => {
                assert!(!output.contains("git clone") && !output.contains("uv sync"));
                assert!(output.contains("\"no-repo\""));
            }
            Some(repo) => {
                assert!(output.contains(&format!("git clone {repo} /workspace/sglang")));
                assert!(output.contains("\"sglang\""));
            }
        }
        for call in sandbox.calls() {
            assert!(
                call[1..3] == ["repo", "list"] || call[1..3] == ["search", "offers"],
                "{call:?}"
            );
        }
    }
}

#[test]
fn test_setup_with_disabled_api_needs_no_tpu_zone_or_catalog() {
    let sandbox = Sandbox::new();
    sandbox.stub("gcloud",r#"#!/bin/sh
entry=gcloud; for arg in "$@"; do entry="$entry	$arg"; done
printf '%s\n' "$entry" >> "$FAKE_CLI_LOG"
case "$1 $2" in
 'projects list') printf '[{"projectId":"test-project","projectNumber":"123456","name":"sandbox"}]\n' ;;
 'services list') printf '[]\n' ;;
 *) exit 97 ;;
esac
"#);
    let mut term = Terminal::start(
        &sandbox,
        &[
            "setup",
            "--provider",
            "gcp",
            "--project",
            "test-project",
            "--dry-run",
        ],
    );
    assert_eq!(term.exit_status(), 0);
    let text = term.output_text();
    assert!(text.contains("Choose TPU type/zone at launch"));
    assert!(!text.contains("TPU zone ("));
    assert!(!sandbox.root.join("config/gpu/config.json").exists());
    assert_eq!(sandbox.calls().len(), 2);
}

#[test]
fn test_setup_project_picker_confirmation_status_and_remove_with_only_cloud_stubs() {
    let sandbox = Sandbox::new();
    sandbox.gcp_reads("[]", false);
    sandbox.stub(
        "vastai",
        r#"#!/bin/sh
{ printf 'vastai'; for arg in "$@"; do printf '\t%s' "$arg"; done; printf '\n'; } >> "$FAKE_CLI_LOG"
[ "$*" = 'show instances --raw' ] || exit 97
printf '[]\n'
"#,
    );
    sandbox.stub("gcloud", r#"#!/bin/sh
entry=gcloud; for arg in "$@"; do entry="$entry	$arg"; done
printf '%s\n' "$entry" >> "$FAKE_CLI_LOG"
case "$*" in
  'auth print-access-token --project=test-project --quiet') printf 'private-test-token\n' ;;
  'projects list --filter=lifecycleState:ACTIVE --quiet --format=json')
    printf '[{"projectId":"test-project","projectNumber":"123456","name":"sandbox"}]\n' ;;
  'services list --enabled --project=test-project --quiet --format=json')
    printf '[{"config":{"name":"compute.googleapis.com"}},{"config":{"name":"tpu.googleapis.com"}},{"config":{"name":"cloudtasks.googleapis.com"}},{"config":{"name":"cloudbilling.googleapis.com"}}]\n' ;;
  'tasks queues list --location=us-central1 --project=test-project --quiet --format=json')
    printf '[{"name":"projects/test-project/locations/us-central1/queues/gpu-expiry"}]\n' ;;
  'tasks queues describe gpu-expiry --project=test-project --location=us-central1 --quiet --format=json')
    printf '{"state":"RUNNING","retryConfig":{"maxAttempts":-1,"maxBackoff":"60s"}}\n' ;;
  'iam service-accounts describe 123456-compute@developer.gserviceaccount.com --project=test-project --quiet --format=json')
    printf '{"email":"123456-compute@developer.gserviceaccount.com"}\n' ;;
  'projects get-iam-policy test-project --project=test-project --quiet --format=json')
    printf '{"bindings":[{"role":"roles/editor","members":["serviceAccount:123456-compute@developer.gserviceaccount.com"]}]}\n' ;;
  'compute tpus tpu-vm accelerator-types list --project=test-project --quiet --zone=us-east5-b --format=json' | 'compute tpus tpu-vm accelerator-types list --project=test-project --quiet --zone=us-central1-a --format=json' | 'compute tpus tpu-vm accelerator-types list --project=test-project --quiet --zone=europe-west4-a --format=json')
    printf '[{"type":"v6e-1"}]\n' ;;
  'compute tpus locations list --project=test-project --quiet --format=json')
    printf '[{"locationId":"us-central1-a"},{"locationId":"europe-west4-a"},{"locationId":"us-east5-b"}]\n' ;;
  'compute tpus tpu-vm list --project=test-project --zone=us-east5-b --quiet --format=json' | 'compute tpus tpu-vm list --project=test-project --zone=us-central1-a --quiet --format=json' | 'compute tpus tpu-vm list --project=test-project --zone=europe-west4-a --quiet --format=json' | 'tasks list --location=us-central1 --queue=gpu-expiry --project=test-project --quiet --format=json')
    printf '[]\n' ;;
  *) echo 'unexpected cloud command: all mutations forbidden in this test' >&2; exit 97 ;;
esac
"#);
    let mut term = Terminal::start(&sandbox, &["setup"]);
    term.wait(shows("Vast.ai  ✓ set up"));
    assert!(shows("Google Cloud  not set up")(&term));
    assert_eq!(tools(&sandbox), ["vastai"]);
    term.press_until("2", on("Google Cloud project"));
    term.wait(shows("test-project"));
    term.press_until("1", shows("Type project ID 'test-project' to confirm:"));
    assert!(!term.output_text().contains("TPU zone (test-project)"));
    assert!(!term.output_text().contains("TPU runtime (Enter ="));
    // Not a single mutation was even attempted; the existing queue is reused.
    term.press("test-project");
    term.press(ENTER);
    assert_eq!(term.exit_status(), 0, "{}", term.output_text());
    let path = sandbox.root.join("config/gpu/config.json");
    let config = std::fs::read_to_string(&path).unwrap();
    assert!(config.contains("123456-compute@developer.gserviceaccount.com"));
    assert!(config.contains("\"created_queue\": false"));
    assert!(!config.contains("\"zone\""));
    assert!(!config.contains("\"runtime\""));
    let before = sandbox.calls().len();
    let mut term = Terminal::start(&sandbox, &["setup"]);
    term.wait(shows("Google Cloud  ✓ set up"));
    assert!(sandbox.calls()[before..]
        .iter()
        .all(|call| call[0] == "vastai"));
    term.press_until("2", on("Google Cloud setup"));
    term.press_until("2", shows("Type project ID 'test-project' to confirm:"));
    term.press("test-project");
    term.press(ENTER);
    assert_eq!(term.exit_status(), 0, "{}", term.output_text());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "{}\n");
    let before = sandbox.calls().len();
    let output = sandbox
        .command(&["ls", "--provider", "gcp"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Next: gpu setup"));
    assert_eq!(sandbox.calls().len(), before);
}

#[test]
fn test_auto_tpu_prices_use_stubs_and_without_setup_explain_the_gate() {
    for configured in [false, true] {
        let sandbox = Sandbox::new();
        if configured {
            let config = sandbox.root.join("config/gpu");
            std::fs::create_dir_all(&config).unwrap();
            std::fs::write(config.join("config.json"),r#"{"gcp":{"setup":{"version":1,"project":"test-project"},"project":"test-project","zone":"us-east5-b","expiry":{"queue":"gpu-expiry","location":"us-central1","service_account":"123456-compute@developer.gserviceaccount.com"}}}"#).unwrap();
            sandbox.gcp_reads("[]", true);
            sandbox.stub(
                "gcloud",
                r#"#!/bin/sh
printf 'gcloud\n' >> "$FAKE_CLI_LOG"
[ "$*" = 'auth print-access-token --project=test-project --quiet' ] || exit 97
printf 'private-test-token\n'
"#,
            );
        }
        let mut term = Terminal::start(
            &sandbox,
            &["up", "--no-repo", "--mode", "container", "--dry-run"],
        );
        term.wait(on("GPU"));
        term.press_until("v6e-1", shows("Filter: v6e-1"));
        assert!(!term.last_line().contains("F2"));
        if configured {
            term.wait(shows("Spot $0.750/hr"));
            assert!(term.output_text().contains("$2.750/hr"));
            assert!(!term.output_text().contains("private-test-token"));
            assert_eq!(
                tools(&sandbox)
                    .iter()
                    .filter(|t| t.as_str() == "gcloud")
                    .count(),
                1
            );
        } else {
            term.wait(shows("needs gpu setup"));
            assert!(tools(&sandbox).iter().all(|t| t == "vastai"));
        }
        term.press("\x03");
        assert_eq!(term.exit_status(), 130);
    }
}

#[test]
fn test_tpu_without_config_keeps_setup_visible_and_returns_to_gpu_choices() {
    for (repo_key, query) in [("0", "tpu"), ("1", "v5e")] {
        let sandbox = Sandbox::new();
        // Match the owner's bare invocation, not --no-repo or a flags-only
        // dry run. PATH stubs forbid every real lifecycle operation.
        let mut term = Terminal::start(&sandbox, &["up"]);
        term.wait(shows("gpu-kernels"));
        term.press_until(repo_key, on("Mode"));
        term.press_until("1", on("GPU"));
        assert!(shows("TPU (Google Cloud)")(&term));
        term.press_until(query, |t| t.line(1) == format!("Filter: {query}"));
        term.press_until(ENTER, shows("Google Cloud is not set up"));
        assert!(term.running(), "setup must remain visible until dismissed");
        let screen = term.lines().join("\n");
        assert!(screen.contains("No TPU created."), "{screen}");
        assert!(screen.contains("Next: gpu setup"), "{screen}");
        let config = sandbox.root.join("config/gpu/config.json");
        assert!(
            term.lines()
                .join("")
                .contains(&config.display().to_string()),
            "{screen}"
        );
        assert_eq!(screen.matches("Next:").count(), 1);
        assert!(tools(&sandbox)
            .iter()
            .all(|tool| ["gh", "vastai"].contains(&tool.as_str())));
        // Dismiss the notice without losing the previous filter/selection.
        term.press_until(BACK, |t| t.title() == "GPU" && !shows("No TPU created.")(t));
        assert_eq!(term.line(1), format!("Filter: {query}"));
        term.press_until(BACK, |t| !t.line(1).starts_with("Filter:"));
        term.press("4090");
        term.press_until(ENTER, on("Offer: RTX 4090"));
        term.wait(shows("id=100"));
        term.press("\x03");
        assert_eq!(term.exit_status(), 130);
        assert!(sandbox
            .calls()
            .iter()
            .all(|call| { call[1..3] == ["repo", "list"] || call[1..3] == ["search", "offers"] }));
    }
}

#[test]
fn test_tpu_category_and_type_search_continue_into_cloud_dry_run() {
    for query in ["tpu", "v5e"] {
        let sandbox = Sandbox::new();
        let config = sandbox.root.join("config/gpu");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("config.json"), r#"{"gcp":{"setup":{"version":1,"project":"test-project"},"project":"test-project","zone":"us-central1-a","runtime":"v2-alpha-tpuv5lite","expiry":{"queue":"gpu-expiry","location":"us-central1","service_account":"reaper@test-project.iam.gserviceaccount.com"}}}"#).unwrap();
        sandbox.gcp_reads("[]", false);
        // The only cloud calls allowed are read-only catalog requests. All
        // lifecycle, IAM and task operations fail if this dry run reaches them.
        sandbox.stub("gcloud", r#"#!/bin/sh
entry=gcloud; for arg in "$@"; do entry="$entry	$arg"; done
printf '%s\n' "$entry" >> "$FAKE_CLI_LOG"
case "$*" in
  'auth print-access-token --project=test-project --quiet') printf 'private-test-token\n' ;;
  'compute tpus locations list --project=test-project --quiet --format=json') printf '[{"locationId":"us-central1-a"},{"locationId":"us-east5-b"}]\n' ;;
  'compute tpus tpu-vm accelerator-types list --project=test-project --quiet --zone=us-central1-a --format=json') printf '[{"type":"v5litepod-1"}]\n' ;;
  'compute tpus tpu-vm accelerator-types list --project=test-project --quiet --zone=us-east5-b --format=json') printf '[{"type":"v6e-1"}]\n' ;;
  *) echo 'unexpected cloud call' >&2; exit 97 ;;
esac
"#);
        // Test repository carry-over, and both a flag deadline and a mandatory
        // interactive deadline after the capacity choice (not before it).
        let mut args = vec!["up", "--repo", "example/demo", "--dry-run"];
        if query == "tpu" {
            args.extend(["--expire", "2h"]);
        }
        let mut term = Terminal::start(&sandbox, &args);
        term.wait(on("Mode")); // no provider picker, even with GCP configured
        term.press_until("1", on("GPU"));
        assert!(shows("TPU (Google Cloud)")(&term));
        term.press_until(query, |t| t.line(1) == format!("Filter: {query}"));
        if query == "tpu" {
            term.press_until(ENTER, on("TPU type"));
            term.press_until("v5e-1", shows("Filter: v5e-1"));
        }
        term.press_until(ENTER, on("TPU capacity"));
        term.press_until("2", on("TPU zone"));
        term.press_until("us-central1-a", shows("Filter: us-central1-a"));
        term.press(ENTER);
        if query == "v5e" {
            term.wait(on("Destroy after (e.g. 2h)"));
            assert!(shows("30m, 2h or 1d")(&term));
            term.press("30s");
            term.press_until(ENTER, shows("Choose 1 minute to 29 days"));
            term.press_until(BACK, on("TPU zone"));
            term.press_until(ENTER, on("Destroy after (e.g. 2h)"));
            term.press("2h");
            term.press(ENTER);
        }
        assert_eq!(term.exit_status(), 0);
        let output = term.output_text();
        assert!(
            output.contains("DRY RUN: no TPU or expiry task created"),
            "{output}"
        );
        assert!(output.contains("--accelerator-type=v5litepod-1"));
        assert!(output.contains("--version=v2-alpha-tpuv5-lite"));
        assert!(output.contains("--spot"));
        assert!(output.contains("--method=DELETE"));
        assert!(output.contains("https://github.com/example/demo /workspace/demo"));
        assert!(
            tools(&sandbox)
                .iter()
                .all(|tool| ["gcloud", "curl", "vastai"].contains(&tool.as_str())),
            "{:?}",
            sandbox.calls()
        );
        assert!(sandbox
            .calls()
            .iter()
            .all(|a| a[0] != "gcloud" || a.get(1).is_some_and(|s| s == "auth")));
        assert!(!sandbox.history_path().exists());
    }
}

#[test]
fn test_arrow_keys_navigate_and_special_keys_never_reach_the_filter() {
    let sandbox = Sandbox::new();
    let mut term = gpu_up(&sandbox);
    term.wait(shows("gpu-kernels"));
    assert!(!term.last_line().contains("Ctrl-C"));
    for c in ["o", "t", "h"] {
        term.press(c);
    }
    for key in SPECIAL_KEYS.iter().chain([&LEFT]) {
        term.press(key);
    }
    // Left cleared the filter; nothing else was typed into it.
    term.wait(|t| t.line(1) == "\u{203a} 1. gpu-kernels");
    assert!(!term.lines()[..2].iter().any(|line| line.contains('[')));
    let latency = term.press_until(RIGHT, on("Mode"));
    assert!(latency < Duration::from_secs(1), "Right took {latency:?}");
    for key in SPECIAL_KEYS {
        term.press(key);
    }
    term.press_until(LEFT, on("Repository"));
    term.press(DOWN);
    term.press(DOWN);
    term.press_until(RIGHT, on("Clone from URL"));
    for c in ["o", "/", "r"] {
        term.press(c);
    }
    for key in SPECIAL_KEYS {
        term.press(key);
    }
    term.wait(|t| t.line(1) == "Repository: o/r");
    term.press_until(LEFT, |t| !t.line(1).starts_with("Repository:"));
    term.press_until(LEFT, on("Repository"));

    term.press_until(LEFT, |t| t.last_line().contains("again to quit"));
    term.press_until(DOWN, |t| !t.last_line().contains("again to quit"));
    term.press_until(LEFT, |t| t.last_line().contains("again to quit"));
    assert!(term.running(), "one Left on the first screen must not quit");
    let start = term.press(LEFT);
    assert_eq!(term.exit_status(), 1);
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(!term.output_text().contains("DRY RUN"));
}

#[test]
fn test_ctrl_c_and_ctrl_d_quit_from_the_first_screen() {
    for (key, status) in [("\x03", 130), ("\x04", 1)] {
        let sandbox = Sandbox::new();
        let mut term = gpu_up(&sandbox);
        term.wait(shows("gpu-kernels"));
        term.press(key);
        assert_eq!(term.exit_status(), status);
    }
}

#[test]
fn test_ctrl_c_quits_while_an_offer_search_is_still_running() {
    let sandbox = Sandbox::new();
    let mut term = gpu_up(&sandbox);
    term.wait(shows("gpu-kernels"));
    term.press_until(ENTER, on("Mode"));
    term.press_until(ENTER, on("GPU"));
    for c in ["4", "0", "9", "0"] {
        term.press(c);
    }
    term.press_until(ENTER, on("Offer: RTX 4090"));
    let start = term.press("\x03");
    assert_eq!(term.exit_status(), 130);
    assert!(start.elapsed() < Duration::from_secs(1));
}

/// SIGINT while the picker owns the terminal takes the same restore path as
/// a panic: raw mode, bracketed paste and the hidden cursor are undone.
#[test]
fn terminal_is_restored_when_interrupted_in_the_picker() {
    let sandbox = Sandbox::new();
    let mut term = gpu_up(&sandbox);
    term.wait(shows("gpu-kernels"));
    let slave = File::options()
        .read(true)
        .write(true)
        .open(&term.slave_path)
        .unwrap();
    let raw = rustix::termios::tcgetattr(slave.as_fd()).unwrap();
    assert!(!raw
        .local_modes
        .contains(rustix::termios::LocalModes::ICANON));
    let pid = rustix::process::Pid::from_raw(term.child.id() as i32).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::INT).unwrap();
    assert_eq!(term.exit_status(), 130);
    let restored = rustix::termios::tcgetattr(slave.as_fd()).unwrap();
    assert!(restored
        .local_modes
        .contains(rustix::termios::LocalModes::ICANON));
    assert!(restored
        .local_modes
        .contains(rustix::termios::LocalModes::ECHO));
    let output = term.output_text();
    assert!(output.contains("\x1b[?2004l\x1b[?25h"), "{output:?}");
    assert!(output.ends_with("Cancelled.\r\n"), "{output:?}");
}

#[test]
fn test_type_first_priced_zones_and_last_zone_highlight_in_terminal() {
    let sandbox = Sandbox::new();
    let config = sandbox.root.join("config/gpu");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.json"),r#"{"gcp":{"setup":{"version":1,"project":"test-project"},"project":"test-project","expiry":{"queue":"gpu-expiry","location":"us-central1","service_account":"reaper@test-project.iam.gserviceaccount.com"}}}"#).unwrap();
    let history = sandbox.history_path();
    std::fs::create_dir_all(history.parent().unwrap()).unwrap();
    std::fs::write(&history, r#"{"gcp_zone:test-project":["us-central1-a"]}"#).unwrap();
    sandbox.gcp_reads("[]", true);
    sandbox.stub(
        "gcloud",
        r#"#!/bin/sh
printf 'gcloud\n' >> "$FAKE_CLI_LOG"
[ "$*" = 'auth print-access-token --project=test-project --quiet' ] || exit 97
printf 'test-token\n'
"#,
    );
    let mut term = Terminal::start(
        &sandbox,
        &[
            "up",
            "--provider",
            "gcp",
            "--no-repo",
            "--spot",
            "--expire",
            "2h",
            "--dry-run",
        ],
    );
    term.wait(on("TPU type"));
    term.press_until("v6e-1", shows("Filter: v6e-1"));
    term.wait(shows("Spot $2.750/hr"));
    term.press_until(ENTER, on("TPU zone"));
    term.wait(shows("us-east (1 zone)"));
    assert!(
        term.lines()
            .iter()
            .any(|l| l.starts_with("›") && l.contains("us-central")),
        "{}",
        term.lines().join("\n")
    );
    term.press_until(RIGHT, on("us-central zones"));
    term.wait(shows("us-central1-a · $2.750/hr"));
    assert!(!shows(" — v6e")(&term));
    term.press_until(BACK, on("TPU zone"));
    term.press("\x1b[A");
    term.press_until(RIGHT, on("us-east zones"));
    term.wait(shows("us-east5-b · $0.750/hr"));
    term.press("\x03");
    assert_eq!(term.exit_status(), 130);
}
