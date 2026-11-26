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
const BACK: &str = LEFT;
const LEFT: &str = "\x1b[D";
const RIGHT: &str = "\x1b[C";
const DOWN: &str = "\x1b[B";
const UP: &str = "\x1b[A";
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
    read_chunk_size: usize,
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
            read_chunk_size: 65536,
        }
    }

    /// Exercise partial redraws deterministically instead of depending on the
    /// scheduler to split a frame between its title, body and footer.
    fn read_in_chunks(mut self, bytes: usize) -> Self {
        assert!((1..=65536).contains(&bytes));
        self.read_chunk_size = bytes;
        self
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

    fn pump(&mut self, timeout: Duration) -> bool {
        let wait = Timespec {
            tv_sec: 0,
            tv_nsec: timeout.as_nanos() as _,
        };
        let mut fds = [PollFd::new(&self.master, PollFlags::IN)];
        if poll(&mut fds, Some(&wait)).unwrap_or(0) > 0 {
            let mut buffer = [0u8; 65536];
            match rustix::io::read(&self.master, &mut buffer[..self.read_chunk_size]) {
                Ok(n) if n > 0 => {
                    self.output.extend_from_slice(&buffer[..n]);
                    self.parser.process(&buffer[..n]);
                    true
                }
                // The child closed the terminal.
                _ => {
                    std::thread::sleep(timeout);
                    false
                }
            }
        } else {
            false
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
                // A final redraw/result may span several PTY reads too.
                while self.pump(Duration::from_millis(50)) {}
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

/// A title or input row can arrive before the rest of its redraw. When
/// asserting the whole page, wait for its expected footer as well.
fn with_footer(
    condition: impl Fn(&Terminal) -> bool,
    footer: &'static str,
) -> impl Fn(&Terminal) -> bool {
    move |t| condition(t) && t.last_line() == footer
}

fn assert_aligned(term: &Terminal, token: &str) {
    let rows = term
        .lines()
        .into_iter()
        .filter(|l| l.contains(token))
        .collect::<Vec<_>>();
    assert!(rows.len() >= 2, "need multiple rows for {token}: {rows:?}");
    let columns = rows
        .iter()
        .map(|l| l.split_once(token).unwrap().0.chars().count())
        .collect::<Vec<_>>();
    assert!(
        columns.iter().all(|c| *c == columns[0]),
        "unaligned {token}: {rows:?}"
    );
    assert!(
        term.lines()
            .iter()
            .all(|l| !l.contains('·') && !l.contains('*')),
        "{}",
        term.lines().join("\n")
    );
}

// These assertions count picker jobs, not the fresh pre-launch identity read.
fn identity_read(call: &[String]) -> bool {
    call.starts_with(&["gh".into(), "api".into(), "user".into()])
}

fn tools(sandbox: &Sandbox) -> Vec<String> {
    sandbox
        .calls()
        .into_iter()
        .filter(|call| !identity_read(call))
        .map(|call| call[0].clone())
        .collect()
}

fn sorted_tools(sandbox: &Sandbox) -> Vec<String> {
    let mut tools = tools(sandbox);
    tools.sort();
    tools
}

fn gpu_up(sandbox: &Sandbox) -> Terminal {
    Terminal::start(sandbox, &["up", "--dry-run"])
}

#[test]
fn test_left_from_the_deepest_screen_steps_back_to_the_first_then_quits() {
    let sandbox = Sandbox::new();
    let mut term = gpu_up(&sandbox);
    term.wait(shows("gpu-kernels"));
    assert_eq!(term.title(), "Repository");
    term.press_until(RIGHT, on("Mode"));
    term.press_until(RIGHT, on("GPU"));
    for c in ["4", "0", "9", "0"] {
        term.press(c);
    }
    term.wait(|t| t.line(1) == "Filter: 4090");
    term.press_until(RIGHT, on("Offer: RTX 4090"));
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
        t.last_line().contains("Press \u{2190} again to quit")
    });
    assert!(term.running(), "one Left on the first screen must not quit");
    term.press(BACK);
    assert_eq!(term.exit_status(), 1);
    assert!(term.output_text().contains("Cancelled."));
    assert_eq!(
        sorted_tools(&sandbox),
        ["gh", "vastai", "vastai"],
        "navigation must not rerun gh or vastai"
    );
}

#[test]
fn test_keyboard_dry_run_launch_after_going_back() {
    let sandbox = Sandbox::new();
    let mut term = gpu_up(&sandbox);
    term.wait(shows("other-repo"));
    term.press_until(RIGHT, on("Mode"));
    term.press_until(RIGHT, on("GPU"));
    term.press_until(BACK, on("Mode"));
    term.press_until(BACK, on("Repository"));
    term.press_until("2", on("Mode")); // explicit repository shortcut
    term.press_until("2", on("GPU"));
    for c in ["t", "4"] {
        term.press(c);
    }
    term.press_until(RIGHT, on("Offer: Tesla T4"));
    term.wait(shows("id=100"));
    term.press(RIGHT);
    assert_eq!(term.exit_status(), 0);
    let output = term.output_text();
    assert!(output.contains("DRY RUN: no instance created"));
    assert!(output.contains("\"100\"") && output.contains("/workspace/other-repo"));
    assert!(output.contains("\"docker.io/vastai/kvm\""));
    assert_eq!(sorted_tools(&sandbox), ["gh", "vastai", "vastai", "vastai"]);
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
                term.press_until(RIGHT, on("Mode"));
            }
        }
        term.press_until("1", on("GPU"));
        for c in ["t", "4"] {
            term.press(c);
        }
        term.press_until(RIGHT, on("Offer: Tesla T4"));
        term.wait(shows("id=100"));
        term.press(RIGHT);
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
                identity_read(&call)
                    || call[1..3] == ["repo", "list"]
                    || call[1..3] == ["search", "offers"],
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
    term.wait(shows("Vast.ai       ✓ set up"));
    assert!(shows("Google Cloud  not set up")(&term));
    assert_eq!(tools(&sandbox), ["vastai"]);
    term.press_until("2", on("Google Cloud project"));
    term.wait(shows("test-project"));
    term.press_until("1", shows("Type project ID 'test-project' to confirm:"));
    assert!(!term.output_text().contains("TPU zone (test-project)"));
    assert!(!term.output_text().contains("TPU runtime (Enter ="));
    // Not a single mutation was even attempted; the existing queue is reused.
    term.press(UP);
    term.press(DOWN);
    term.press("test-project");
    term.press(ENTER);
    assert_eq!(term.exit_status(), 0, "{}", term.output_text());
    assert!(!term.output_text().contains("^[["));
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
    term.press(UP);
    term.press(DOWN);
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
            std::fs::write(
                config.join("config.json"),
                common::gcp_config(
                    Some("us-east5-b"),
                    None,
                    Some("123456-compute@developer.gserviceaccount.com"),
                ),
            )
            .unwrap();
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
        let query = if configured { "v6e-1" } else { "tpu" };
        term.press_until(query, |t| t.line(1) == format!("Filter: {query}"));
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
    for repo_key in ["0", "1"] {
        let query = "tpu";
        let sandbox = Sandbox::new();
        // Match the owner's bare invocation, not --no-repo or a flags-only
        // dry run. PATH stubs forbid every real lifecycle operation.
        let mut term = Terminal::start(&sandbox, &["up"]);
        term.wait(shows("gpu-kernels"));
        term.press_until(repo_key, on("Mode"));
        term.press_until("1", |t| on("GPU")(t) && shows("needs gpu setup")(t));
        assert!(shows("needs gpu setup")(&term));
        // No types or prices are offered before setup, even with type filtering.
        term.press_until("v5e", shows("Filter: v5e"));
        term.wait(shows("No matches"));
        term.press_until(BACK, |t| !t.line(1).starts_with("Filter:"));
        term.press_until(query, |t| {
            t.line(1) == format!("Filter: {query}") && shows(". TPU")(t)
        });
        let rows = term
            .lines()
            .into_iter()
            .filter(|l| l.contains(". TPU"))
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(!rows[0].contains('$'));
        assert!(!rows[0].contains("TPU v"));
        term.press_until(
            RIGHT,
            with_footer(
                shows("Google Cloud is not set up"),
                "← return to choices  Ctrl-C quit",
            ),
        );
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
        term.press_until(BACK, |t| {
            t.title() == "GPU"
                && t.line(1) == format!("Filter: {query}")
                && !shows("No TPU created.")(t)
        });
        assert_eq!(term.line(1), format!("Filter: {query}"));
        term.press_until(BACK, |t| !t.line(1).starts_with("Filter:"));
        term.press("4090");
        term.press_until(RIGHT, on("Offer: RTX 4090"));
        term.wait(shows("id=100"));
        term.press("\x03");
        assert_eq!(term.exit_status(), 130);
        assert!(sandbox.calls().iter().all(|call| {
            identity_read(call)
                || call[1..3] == ["repo", "list"]
                || call[1..3] == ["search", "offers"]
        }));
    }
}

#[test]
fn test_tpu_category_and_type_search_continue_into_cloud_dry_run() {
    for query in ["tpu", "v5e"] {
        let sandbox = Sandbox::new();
        let config = sandbox.root.join("config/gpu");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("config.json"),
            common::gcp_config(Some("us-central1-a"), Some("v2-alpha-tpuv5lite"), None),
        )
        .unwrap();
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
        term.press_until("1", |t| on("GPU")(t) && shows("TPU")(t));
        assert!(shows("TPU")(&term));
        term.press_until(query, |t| t.line(1) == format!("Filter: {query}"));
        if query == "tpu" {
            term.press_until(RIGHT, on("TPU generation"));
            term.press_until("v5e", shows("Filter: v5e"));
            term.press_until(RIGHT, on("v5e size"));
        }
        if query == "v5e" {
            term.press_until(RIGHT, on("v5e size"));
        }
        term.press_until(RIGHT, on("TPU capacity"));
        term.press_until("2", on("TPU zone"));
        term.press_until("us-central1-a", shows("Filter: us-central1-a"));
        term.press(RIGHT);
        if query == "v5e" {
            term.wait(with_footer(on("Destroy after (e.g. 2h)"), "← back"));
            assert!(shows("30m, 2h or 1d")(&term));
            term.press("30s");
            term.press_until(RIGHT, shows("Expiry must be 1 minute–29 days"));
            term.press_until(BACK, on("TPU zone"));
            term.press_until(RIGHT, on("Destroy after (e.g. 2h)"));
            term.press("2h");
            term.press(RIGHT);
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
    for _ in 0..3 {
        term.press(DOWN);
    }
    term.press_until(RIGHT, on("Clone from URL"));
    for c in ["o", "/", "r"] {
        term.press(c);
    }
    for key in SPECIAL_KEYS {
        term.press(key);
    }
    term.wait(|t| t.line(1) == "Repository: o/r");
    term.press_until(LEFT, |t| t.line(1) == "Repository:");
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
    term.press_until(RIGHT, on("Mode"));
    term.press_until(RIGHT, on("GPU"));
    for c in ["4", "0", "9", "0"] {
        term.press(c);
    }
    term.press_until(RIGHT, on("Offer: RTX 4090"));
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
    std::fs::write(
        config.join("config.json"),
        common::gcp_config(None, None, None),
    )
    .unwrap();
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
    term.wait(on("TPU generation"));
    term.wait(shows("Spot $2.750/hr"));
    assert_aligned(&term, "Spot ");
    assert!(!shows("from unavailable")(&term));
    term.press_until("v6e", shows("Filter: v6e"));
    term.press_until(RIGHT, on("v6e size"));
    term.wait(shows("Spot $2.750/hr"));
    assert_aligned(&term, "Spot ");
    term.press_until(RIGHT, on("TPU zone"));
    term.wait(shows("us-east     1  zone"));
    assert_aligned(&term, "  zone");
    assert!(
        term.lines()
            .iter()
            .any(|l| l.starts_with("›") && l.contains("us-central")),
        "{}",
        term.lines().join("\n")
    );
    term.press_until(RIGHT, on("us-central zones"));
    term.wait(shows("us-central1-a  $2.750/hr"));
    assert!(term.lines().iter().all(|line| !line.contains('\u{2014}')));
    term.press_until(BACK, on("TPU zone"));
    term.press("\x1b[A");
    term.press_until(RIGHT, on("us-east zones"));
    term.wait(shows("us-east5-b  $0.750/hr"));
    term.press_until(BACK, on("TPU zone"));
    term.press_until("us-", shows("Filter: us-"));
    term.wait(|t| t.lines().iter().filter(|l| l.contains("/hr")).count() == 2);
    assert_aligned(&term, "$");
    term.press("\x03");
    assert_eq!(term.exit_status(), 130);
}

#[test]
fn test_vast_confirmation_left_restores_offer_and_only_yes_enter_creates() {
    let sandbox = Sandbox::new();
    sandbox.stub("vastai", &common::FAKE_CLI.replace(
        "case \"$tool $1 $2\" in",
        "case \"$tool $1 $2\" in\n    \"vastai create instance\") echo '{\"success\":true,\"new_contract\":123,\"instance_api_key\":\"private-test-key\"}' ;;",
    ));
    let mut term = Terminal::start(
        &sandbox,
        &[
            "up",
            "--repo",
            "example/gpu-cli",
            "--mode",
            "container",
            "--gpu",
            "RTX 4090",
        ],
    )
    .read_in_chunks(16);
    term.wait(on("Offer: RTX 4090"));
    term.wait(shows("id=100"));
    term.press_until(
        RIGHT,
        with_footer(shows("Type 'yes' to confirm:"), "← back"),
    );
    assert_eq!(
        term.title(),
        "Launch RTX 4090 container for gpu-cli at $0.300/hr?"
    );
    assert_eq!(term.last_line(), "← back");
    assert_eq!(
        term.lines().len(),
        3,
        "question, input and back footer only"
    );
    assert!(
        !term.output_text().contains("Enter confirms")
            && !term.output_text().contains("Backspace edit")
    );
    let reads = sandbox.calls().len();
    term.press("ye");
    term.press_until(LEFT, |t| on("Offer: RTX 4090")(t) && shows("id=100")(t));
    assert!(shows("id=100")(&term));
    assert_eq!(
        sandbox.calls().len(),
        reads,
        "Back restores the loaded offer page"
    );
    assert!(!sandbox
        .calls()
        .iter()
        .any(|a| a.iter().any(|s| s == "create")));
    term.press_until(
        ENTER,
        with_footer(shows("Type 'yes' to confirm:"), "← back"),
    );
    assert!(!shows("Type 'yes' to confirm: ye")(&term));
    term.press("yes");
    term.press(RIGHT);
    term.pump(Duration::from_millis(100));
    assert!(!sandbox
        .calls()
        .iter()
        .any(|a| a.iter().any(|s| s == "create")));
    term.press_until(ENTER, shows("Open workspace"));
    term.press("2");
    assert_eq!(term.exit_status(), 0, "{}", term.output_text());
    assert_eq!(
        sandbox
            .calls()
            .iter()
            .filter(|a| a.iter().any(|s| s == "create"))
            .count(),
        1
    );
    let output = term.output_text();
    assert!(
        !output.contains("^[[")
            && !output.contains("this paid instance")
            && !output.contains("private-test-key"),
        "{output:?}"
    );
}

#[test]
fn test_tpu_confirmation_left_restores_filtered_zone_in_direct_and_shared_flows() {
    for direct in [false, true] {
        let sandbox = Sandbox::new();
        sandbox.gcp_lifecycle();
        let args = if direct {
            vec![
                "up",
                "--tpu",
                "v6e-1",
                "--no-repo",
                "--spot",
                "--expire",
                "2h",
            ]
        } else {
            vec!["up", "--no-repo", "--expire", "2h"]
        };
        let mut term = Terminal::start(&sandbox, &args).read_in_chunks(16);
        if !direct {
            term.wait(on("Mode"));
            term.press_until(RIGHT, on("GPU"));
            term.press_until("v6e-1", shows("Filter: v6e-1"));
            term.press_until(RIGHT, on("TPU capacity"));
            term.press_until("2", on("TPU zone"));
        } else {
            term.wait(on("TPU zone"));
        }
        term.press_until("us-east5-b", shows("Filter: us-east5-b"));
        term.press_until(
            ENTER,
            with_footer(shows("Type 'yes' to confirm:"), "← back"),
        );
        assert!(term
            .title()
            .starts_with("Launch v6e-1 Spot in us-east5-b for $"));
        assert_eq!(term.last_line(), "← back");
        assert!(!term
            .lines()
            .iter()
            .any(|line| line.contains("Enter confirms") || line.contains("Backspace edit")));
        term.press("ye");
        term.press_until(LEFT, |t| {
            on("TPU zone")(t) && shows("Filter: us-east5-b")(t)
        });
        assert!(shows("Filter: us-east5-b")(&term));
        assert!(!sandbox
            .calls()
            .iter()
            .any(|a| a.iter().any(|s| s == "create" || s == "create-http-task")));
        term.press_until(
            RIGHT,
            with_footer(shows("Type 'yes' to confirm:"), "← back"),
        );
        assert!(!shows("Type 'yes' to confirm: ye")(&term));
        term.press_until("yes\r", shows("Open workspace"));
        term.press("2");
        assert_eq!(term.exit_status(), 0, "{}", term.output_text());
        assert_eq!(
            sandbox
                .calls()
                .iter()
                .filter(|a| a.get(4).is_some_and(|s| s == "create"))
                .count(),
            1
        );
        assert!(sandbox.root.join("gcp-task.json").exists());
        assert!(
            !term.output_text().contains("^[["),
            "{}",
            term.output_text()
        );
    }
}

#[test]
fn test_gpu_picker_measures_vast_and_tpu_rows_in_the_same_four_columns() {
    let sandbox = Sandbox::new();
    sandbox.gcp_lifecycle();
    sandbox.stub(
        "vastai",
        &common::FAKE_CLI
            .replace("offer 100 \"RTX 4090\" 0.3", "offer 100 \"H100 NVL\" 0.441")
            .replace("offer 101 \"Tesla T4\" 0.4", "offer 101 \"Titan V\" 2.0"),
    );
    let history = sandbox.history_path();
    std::fs::create_dir_all(history.parent().unwrap()).unwrap();
    std::fs::write(
        &history,
        r#"{"gpu":["H100 NVL","Titan V"],"gcp_zone:test-project":["us-east5-b"]}"#,
    )
    .unwrap();
    let mut term = Terminal::start(
        &sandbox,
        &["up", "--no-repo", "--expire", "2h", "--dry-run"],
    );
    term.wait(on("Mode"));
    term.press_until(RIGHT, on("GPU"));
    term.wait(shows("$0.441/hr"));
    term.wait(shows("Spot $0.750/hr"));
    let assert_grid = |term: &Terminal| {
        let labels = term
            .lines()
            .into_iter()
            .filter_map(|line| line.split_once(". ").map(|(_, label)| label.to_string()))
            .collect::<Vec<_>>();
        assert!(labels.len() > 2, "{labels:?}");
        let gaps = regex::Regex::new(r" {2,}").unwrap();
        let columns = labels
            .iter()
            .map(|label| {
                assert_eq!(gaps.split(label).count(), 4, "{label:?}");
                [
                    label[..label.find("Spot ").unwrap()].chars().count(),
                    label[..label.rfind("  ").unwrap() + 2].chars().count(),
                ]
            })
            .collect::<Vec<_>>();
        assert!(columns.iter().all(|col| *col == columns[0]), "{labels:?}");
        assert_aligned(term, "Spot ");
        // Rates are right-aligned: a two-digit dollar amount starts one cell
        // earlier, but its decimal places and /hr suffix share the same columns.
        assert_aligned(term, "/hr");
    };
    assert!(shows("Recent: H100 NVL")(&term));
    assert_grid(&term);
    term.press_until("v", shows("Filter: v"));
    term.wait(shows("TPU v6e"));
    assert!(shows("H100 NVL")(&term));
    assert!(shows("Titan V")(&term));
    assert_grid(&term);
    assert!(term.lines().iter().any(|line| line.contains("H100 NVL")
        && line.contains("Spot -")
        && line.contains("1 offer")));
    term.press("\x03");
    assert_eq!(term.exit_status(), 130);
    assert!(!sandbox.calls().iter().any(|call| call
        .iter()
        .any(|a| a == "create" || a == "create-http-task" || a == "--interruptible")));
}

#[test]
fn test_ls_is_one_compact_aligned_table_in_a_real_terminal() {
    let sandbox = Sandbox::new();
    sandbox.mixed_listing();
    let mut term = Terminal::start(&sandbox, &["ls"]);
    assert_eq!(term.exit_status(), 0, "{}", term.output_text());
    assert_eq!(
        term.lines(),
        [
            "ID          Provider  Type      Status    Price      Uptime  Owner",
            "30306428    vast      RTX 4090  created   $0.102/hr  5m",
            "gpu-uwhcp7  gcp       v5e-1     deleting  $0.359/hr  7m",
        ]
    );
    assert_aligned(&term, "$");
    assert_eq!(sandbox.calls().len(), 4, "no extra price/catalog reads");
    let mut ssh = Terminal::start(&sandbox, &["ssh", "gpu-uwhcp7"]);
    assert_eq!(ssh.exit_status(), 0, "{}", ssh.output_text());
    assert!(ssh.output_text().contains("ssh gpu-1794014955-uwhcp7"));
    assert_eq!(
        sandbox.calls().len(),
        6,
        "short ID lookup needs only token + node inventory"
    );
}

#[test]
fn test_tpu_size_and_capacity_prices_numbers_and_enter_in_real_terminal() {
    for size_key in [ENTER, "2"] {
        let sandbox = Sandbox::new();
        sandbox.gcp_lifecycle();
        let curl = std::fs::read_to_string(sandbox.bin.join("curl")).unwrap();
        sandbox.stub(
            "curl",
            &curl.replace(
                r#"{"type":"v6e-1"}"#,
                r#"{"type":"v6e-1"},{"type":"v6e-4"}"#,
            ),
        );
        let history = sandbox.history_path();
        std::fs::create_dir_all(history.parent().unwrap()).unwrap();
        std::fs::write(&history, r#"{"gcp_zone:test-project":["us-east5-b"]}"#).unwrap();
        let mut term = Terminal::start(
            &sandbox,
            &[
                "up",
                "--provider",
                "gcp",
                "--tpu",
                "v6e",
                "--no-repo",
                "--expire",
                "2h",
                "--dry-run",
            ],
        );
        term.wait(on("v6e size"));
        term.wait(with_footer(
            shows("Spot $3.000/hr"),
            "↑/↓ move  → select  ← quit (twice)  1-9 select",
        ));
        if size_key == ENTER {
            term.press(DOWN);
        }
        term.press_until(size_key, on("TPU capacity"));
        term.wait(shows("On-demand  $11.000/hr"));
        term.wait(with_footer(
            shows("Spot       $3.000/hr"),
            "↑/↓ move  → select  ← back  1-9 select",
        ));
        assert_aligned(&term, "$");
        assert!(shows("(may be interrupted)")(&term));
        assert_eq!(term.last_line(), "↑/↓ move  → select  ← back  1-9 select");
        term.press("`");
        term.press_until(DOWN, |t| {
            t.lines()
                .iter()
                .any(|l| l.starts_with('›') && l.contains("Spot"))
        });
        term.press_until(ENTER, on("TPU zone"));
        term.press_until("us-east5-b", shows("Filter: us-east5-b"));
        term.press(ENTER);
        assert_eq!(term.exit_status(), 0, "{}", term.output_text());
        assert!(term.output_text().contains("--accelerator-type=v6e-4"));
        assert!(term.output_text().contains("--spot"));
        assert!(!sandbox
            .calls()
            .iter()
            .any(|a| a.iter().any(|v| v == "create" || v == "create-http-task")));
    }
}

#[test]
fn test_tpu_capacity_keeps_moving_while_prices_load_then_redraws_without_keys() {
    let sandbox = Sandbox::new();
    sandbox.gcp_lifecycle();
    let hold = sandbox.root.join("hold-prices");
    std::fs::write(&hold, "").unwrap();
    let curl = std::fs::read_to_string(sandbox.bin.join("curl")).unwrap();
    sandbox.stub(
        "curl",
        &curl.replace(
            "    status=200",
            "    while [ -f \"$root/hold-prices\" ]; do sleep 0.05; done\n    status=200",
        ),
    );
    let mut term = Terminal::start(
        &sandbox,
        &[
            "up",
            "--tpu",
            "v6e-4",
            "--zone",
            "us-east5-b",
            "--no-repo",
            "--expire",
            "2h",
            "--dry-run",
        ],
    );
    term.wait(|t| on("TPU capacity")(t) && shows("On-demand  -")(t) && shows("Spot       -")(t));
    assert!(shows("On-demand  -")(&term));
    assert!(shows("Spot       -")(&term));
    let latency = term.press_until(DOWN, |t| {
        t.lines()
            .iter()
            .any(|l| l.starts_with('›') && l.contains("Spot"))
    });
    assert!(
        latency < Duration::from_millis(250),
        "blocked on prices: {latency:?}"
    );
    std::fs::remove_file(hold).unwrap();
    term.wait(shows("On-demand  $11.000/hr"));
    term.wait(shows("Spot       $3.000/hr"));
    assert_aligned(&term, "$");
    term.press(ENTER);
    assert_eq!(term.exit_status(), 0, "{}", term.output_text());
    assert!(term.output_text().contains("--accelerator-type=v6e-4"));
    assert!(term.output_text().contains("--spot"));
}

#[test]
fn test_tpu_retry_manual_zone_confirmation_keeps_back_navigation() {
    let sandbox = Sandbox::new();
    sandbox.gcp_lifecycle();
    std::fs::write(
        sandbox.root.join("gcp-reject-zones.json"),
        r#"["us-east5-b"]"#,
    )
    .unwrap();
    let mut term = Terminal::start(
        &sandbox,
        &[
            "up",
            "--tpu",
            "v5e-1",
            "--zone",
            "us-east5-b",
            "--spot",
            "--no-repo",
            "--expire",
            "2h",
        ],
    );
    term.wait(shows("Type 'yes' to confirm:"));
    term.press("yes\r");
    term.wait(shows("Retry v5e-1 Spot"));
    term.press_until("2", shows("TPU zone"));
    term.press_until("us-central1-a", shows("Filter: us-central1-a"));
    term.press_until(ENTER, shows("Type 'yes' to confirm:"));
    term.press("ye");
    term.press_until(LEFT, shows("TPU zone"));
    assert!(shows("Filter: us-central1-a")(&term));
    assert_eq!(
        sandbox
            .calls()
            .iter()
            .filter(|a| a.get(4).is_some_and(|s| s == "create"))
            .count(),
        1
    );
    assert!(
        !sandbox.root.join("gcp-task.json").exists(),
        "back must not arm another paid attempt"
    );
    term.press_until(ENTER, shows("Type 'yes' to confirm:"));
    term.press_until("yes\r", shows("Open workspace"));
    term.press("2");
    assert_eq!(term.exit_status(), 0, "{}", term.output_text());
    assert_eq!(
        sandbox
            .calls()
            .iter()
            .filter(|a| a.get(4).is_some_and(|s| s == "create"))
            .count(),
        2
    );
    assert!(!term.output_text().contains("^[["));
}

#[test]
fn test_tpu_wait_timer_counts_in_place_and_clears_for_retry_and_results() {
    let sandbox = Sandbox::new();
    sandbox.gcp_lifecycle();
    std::fs::write(sandbox.root.join("gcp-create-delay.json"), "2").unwrap();
    std::fs::write(
        sandbox.root.join("gcp-reject-zones.json"),
        r#"["us-east5-b"]"#,
    )
    .unwrap();
    let mut term = Terminal::start(
        &sandbox,
        &[
            "up",
            "--tpu",
            "v5e-1",
            "--zone",
            "us-east5-b",
            "--spot",
            "--no-repo",
            "--expire",
            "2h",
        ],
    );
    term.wait(shows("Type 'yes' to confirm:"));
    let confirmation = term
        .lines()
        .into_iter()
        .find(|l| l.starts_with("Launch "))
        .unwrap();
    assert!(
        confirmation.starts_with("Launch v5e-1 Spot in us-east5-b for $"),
        "{confirmation}"
    );
    assert!(!term.output_text().contains("cached"));
    assert!(!term.output_text().contains("Paid;"));
    let plain = regex::Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]")
        .unwrap()
        .replace_all(&term.output_text(), "")
        .replace(['\r', '\n'], "");
    assert!(
        plain.contains("(storage and networking charges apply)"),
        "{plain}"
    );
    term.press("yes\r");
    for attempt in 0..2 {
        term.wait(shows("Waiting for Google Cloud to create the TPU... 0:00"));
        term.wait(shows("Waiting for Google Cloud to create the TPU... 0:01"));
        assert_eq!(
            term.lines()
                .iter()
                .filter(|l| l.contains("Waiting for Google Cloud"))
                .count(),
            1
        );
        if attempt == 0 {
            term.wait(shows("Retry v5e-1 Spot"));
            assert!(!shows("Waiting for Google Cloud")(&term));
            assert!(!term.lines().iter().any(|l| l.contains(';')));
            term.press(RIGHT);
        }
    }
    term.wait(shows("Open workspace"));
    assert!(!shows("Waiting for Google Cloud")(&term));
    term.press("2");
    assert_eq!(term.exit_status(), 0, "{}", term.output_text());
    let output = term.output_text();
    assert!(output.contains("\r\x1b[2KCreated TPU"), "{output:?}");
    assert!(!output
        .rsplit_once("Created TPU")
        .unwrap()
        .1
        .contains("Waiting for Google Cloud"));

    let nodes =
        gpu::pyjson::loads(&std::fs::read_to_string(sandbox.root.join("gcp-nodes.json")).unwrap())
            .unwrap();
    let name = nodes
        .as_dict()
        .unwrap()
        .get("nodes")
        .unwrap()
        .as_list()
        .unwrap()[0]
        .as_dict()
        .unwrap()
        .get("name")
        .unwrap()
        .as_str()
        .unwrap();
    let parts = name.split('/').collect::<Vec<_>>();
    let id = format!("gcp:{}:{}:{}", parts[1], parts[3], parts[5]);
    for (action, result, message) in [
        (
            "stop",
            "Stopped",
            "Waiting for Google Cloud to stop the TPU",
        ),
        (
            "delete",
            "Destroyed TPU",
            "Waiting for Google Cloud to delete the TPU",
        ),
    ] {
        std::fs::write(sandbox.root.join(format!("gcp-{action}-delay.json")), "2").unwrap();
        let args = if action == "delete" {
            vec!["down", &id, "--yes"]
        } else {
            vec!["stop", &id]
        };
        let mut lifecycle = Terminal::start(&sandbox, &args);
        lifecycle.wait(|t| {
            t.lines()
                .iter()
                .any(|l| l.contains(&format!("{message}... 0:00")))
        });
        lifecycle.wait(|t| {
            t.lines()
                .iter()
                .any(|l| l.contains(&format!("{message}... 0:01")))
        });
        assert_eq!(lifecycle.exit_status(), 0, "{}", lifecycle.output_text());
        assert!(!shows("Waiting for Google Cloud")(&lifecycle));
        assert!(lifecycle
            .output_text()
            .contains(&format!("\r\x1b[2K{result}")));
    }
    assert!(!sandbox.root.join("gcp-task.json").exists());
}

#[test]
fn test_tpu_wait_timer_clears_before_a_create_error() {
    let sandbox = Sandbox::new();
    sandbox.gcp_lifecycle();
    // Fail after the waiting line starts, but before any paid create request.
    sandbox.stub("gcloud", &include_str!("common/gcp_native.py").replace(
        "if args[1:3] == ['queues', 'describe']:",
        "if args[1:3] == ['queues', 'describe']:\n        time.sleep(2)\n        sys.exit('PERMISSION_DENIED')",
    ));
    let mut term = Terminal::start(
        &sandbox,
        &[
            "up",
            "--tpu",
            "v6e-1",
            "--zone",
            "us-east5-b",
            "--no-repo",
            "--expire",
            "2h",
            "--yes",
            "--on-demand",
        ],
    );
    term.wait(shows("Waiting for Google Cloud to create the TPU... 0:01"));
    assert_eq!(term.exit_status(), 1);
    assert!(term
        .output_text()
        .contains("\r\x1b[2Kgpu: Google Cloud denied access"));
    assert!(!shows("Waiting for Google Cloud")(&term));
    assert!(!sandbox
        .calls()
        .iter()
        .any(|a| a.iter().any(|v| v == "create")));
}

#[test]
fn test_tpu_post_launch_editor_then_edit_uses_instance_picker() {
    let sandbox = Sandbox::new();
    sandbox.gcp_lifecycle();
    let history = sandbox.history_path();
    std::fs::create_dir_all(history.parent().unwrap()).unwrap();
    std::fs::write(&history, r#"{"editor":["cursor"]}"#).unwrap();
    let mut term = Terminal::start(&sandbox, &["up", "--no-repo", "--expire", "2h"]);
    term.wait(on("Mode"));
    term.press_until("1", on("GPU"));
    term.press_until("tpu", shows("Filter: tpu"));
    term.press_until(RIGHT, on("TPU generation"));
    term.press_until("v6e", shows("Filter: v6e"));
    term.press_until(RIGHT, on("v6e size"));
    term.press_until(RIGHT, on("TPU capacity"));
    term.press_until("2", on("TPU zone"));
    term.press_until("us-east5-b", shows("Filter: us-east5-b"));
    term.press_until(RIGHT, shows("Type 'yes' to confirm:"));
    term.press_until("yes\r", |t| {
        shows("Open workspace")(t) && shows("Don't open")(t)
    });
    assert!(shows("Open editor")(&term));
    assert!(shows("Select editor...")(&term));
    assert!(shows("Don't open")(&term));
    term.press(DOWN);
    term.press_until(RIGHT, shows("Editor"));
    term.press(DOWN);
    term.press(RIGHT);
    assert_eq!(term.exit_status(), 0, "{}", term.output_text());
    assert!(term.output_text().contains("CREATING"));
    assert!(term.output_text().contains("Opened VS Code"));
    {
        let (command, tool) = ("edit", "code");
        if command == "edit" {
            let nodes = sandbox.root.join("gcp-nodes.json");
            let data = std::fs::read_to_string(&nodes).unwrap();
            std::fs::write(nodes, data.replace("\"READY\"", "\"CREATING\"")).unwrap();
            std::fs::write(sandbox.root.join("gcp-creating-reads.json"), "1").unwrap();
        }
        let mut edit = Terminal::start(&sandbox, &[command]);
        edit.wait(|t| shows("Google Cloud")(t) && shows("v6e-1")(t));
        assert!(shows("v6e-1")(&edit));
        edit.press(RIGHT);
        assert_eq!(edit.exit_status(), 0, "{}", edit.output_text());
        if command == "edit" {
            assert!(edit.output_text().contains("CREATING"));
        }
        assert!(sandbox.calls().iter().any(|a| a[0] == tool
            && a.iter()
                .any(|s| s.starts_with("ssh-remote+gcp_test-project_us-east5-b_gpu-"))
            && a.iter().any(|s| s == "/workspace")));
    }
    let config = std::fs::read_to_string(sandbox.home.join(".ssh/gpu_gcp_config")).unwrap();
    assert!(config.contains("Host gcp_test-project_us-east5-b_gpu-"));
    assert!(config.contains("User \"developer\""));
    assert!(config.contains("google_compute_engine"));
    assert!(!config.contains("test-token"));
    // A blank destruction confirmation intentionally cancels. Selecting an
    // instance must not leak input into the following confirmation.
    for approve in [false, true] {
        let mut down = Terminal::start(&sandbox, &["down"]);
        down.wait(shows("Google Cloud"));
        down.press_until(RIGHT, shows("Destroy TPU"));
        down.press(UP);
        down.press(DOWN);
        down.press(if approve { "yes\r" } else { "\r" });
        assert_eq!(down.exit_status(), 0, "{}", down.output_text());
        assert!(!down.output_text().contains("^[["));
        assert!(down.output_text().contains(if approve {
            "Destroyed TPU"
        } else {
            "Cancelled."
        }));
        assert_eq!(sandbox.root.join("gcp-task.json").exists(), !approve);
    }
    let calls = sandbox.calls();
    let delete_node = calls
        .iter()
        .position(|a| a.get(4).is_some_and(|s| s == "delete"))
        .unwrap();
    let delete_task = calls
        .iter()
        .position(|a| {
            a.get(1).is_some_and(|s| s == "tasks") && a.get(2).is_some_and(|s| s == "delete")
        })
        .unwrap();
    assert!(delete_node < delete_task);
}

#[test]
fn test_list_enter_selects_and_backtick_does_nothing_in_real_terminal() {
    let sandbox = Sandbox::new();
    let mut term = Terminal::start(&sandbox, &["up", "--no-repo", "--dry-run"]);
    term.wait(on("Mode"));
    assert!(!term.last_line().contains("Enter") && !term.last_line().contains('`'));
    term.press("`");
    term.press_until(DOWN, |t| {
        t.lines()
            .iter()
            .any(|l| l.starts_with('›') && l.contains("vm"))
    });
    assert_eq!(term.title(), "Mode");
    assert!(!shows("Filter:")(&term));
    term.press_until(ENTER, on("GPU"));
    term.press_until("t4", shows("Filter: t4"));
    term.press("`");
    term.press(UP);
    term.wait(|t| t.title() == "GPU" && t.line(1) == "Filter: t4");
    term.press_until(ENTER, on("Offer: Tesla T4"));
    term.wait(with_footer(
        shows("id=100"),
        "↑/↓ move  → select  ← back  1-9 select",
    ));
    assert_eq!(term.last_line(), "↑/↓ move  → select  ← back  1-9 select");
    term.press_until(LEFT, on("GPU"));
    term.press_until(LEFT, |t| !t.line(1).starts_with("Filter:"));
    term.press_until(LEFT, on("Mode"));
    term.press_until(LEFT, |t| t.last_line().contains("again to quit"));
    term.press(LEFT);
    assert_eq!(term.exit_status(), 1);
}

#[test]
fn test_enter_submits_repository_url_and_tpu_duration_text_boxes() {
    let sandbox = Sandbox::new();
    let mut repo = Terminal::start(&sandbox, &["up", "--dry-run"]).read_in_chunks(16);
    repo.wait(shows("Clone from URL..."));
    repo.press_until("9", with_footer(on("Clone from URL"), "← back"));
    assert_eq!(repo.last_line(), "← back");
    assert!(!repo
        .lines()
        .iter()
        .any(|line| line.contains("Backspace edit") || line.contains("then Enter")));
    repo.press_until(LEFT, on("Repository"));
    repo.press_until("9", with_footer(on("Clone from URL"), "← back"));
    repo.press(UP);
    repo.press(DOWN);
    repo.press("https://github.com/owner/demo");
    repo.press_until(ENTER, on("Mode"));
    repo.press("\x03");
    assert_eq!(repo.exit_status(), 130);
    assert!(!repo.output_text().contains("^[["));
    let sandbox = Sandbox::new();
    sandbox.gcp_lifecycle();
    let mut duration = Terminal::start(
        &sandbox,
        &[
            "up",
            "--provider",
            "gcp",
            "--tpu",
            "v6e-1",
            "--zone",
            "us-east5-b",
            "--no-repo",
            "--spot",
            "--dry-run",
        ],
    )
    .read_in_chunks(16);
    duration.wait(with_footer(on("Destroy after (e.g. 2h)"), "← back"));
    assert!(shows("30m, 2h or 1d")(&duration));
    assert!(!duration
        .lines()
        .iter()
        .any(|line| line.contains("Enter/→") || line.contains("Backspace edit")));
    duration.press("2h");
    duration.press(UP);
    duration.press(DOWN);
    duration.press(ENTER);
    assert_eq!(duration.exit_status(), 0, "{}", duration.output_text());
    assert!(duration
        .output_text()
        .contains("DRY RUN: no TPU or expiry task created"));
    assert!(!duration.output_text().contains("^[["));
    assert!(sandbox
        .calls()
        .iter()
        .all(|a| identity_read(a) || a[0] == "curl" || (a[0] == "gcloud" && a[1] == "auth")));
}

#[test]
fn test_cloud_generation_and_size_left_restores_gpu_page() {
    {
        let back = LEFT;
        let sandbox = Sandbox::new();
        let config = sandbox.root.join("config/gpu");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("config.json"),
            common::gcp_config(None, None, None),
        )
        .unwrap();
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
            &["up", "--no-repo", "--expire", "2h", "--dry-run"],
        );
        term.wait(on("Mode"));
        term.press_until("1", on("GPU"));
        term.press_until("tpu", shows("Filter: tpu"));
        assert!(
            term.lines().iter().filter(|l| l.contains("TPU v")).count() <= 6,
            "root must show generations, not every size"
        );
        term.press_until(RIGHT, on("TPU generation"));
        assert!(!term.last_line().contains("quit (twice)"));
        term.press_until(RIGHT, |t| on("v5e size")(t) && shows("chip")(t));
        assert!(shows("chip")(&term));
        assert!(!term.last_line().contains("quit (twice)"));
        term.press_until(back, on("TPU generation"));
        term.press_until(back, |t| on("GPU")(t) && t.line(1) == "Filter: tpu");
        assert_eq!(term.line(1), "Filter: tpu");
        term.press("\x03");
        assert_eq!(term.exit_status(), 130);
        assert!(!sandbox
            .calls()
            .iter()
            .any(|c| c.iter().any(|a| a == "create" || a == "create-http-task")));
    }
}

#[test]
fn test_run_without_expire_asks_for_it_in_a_terminal_then_opens_the_gpu_picker() {
    const BACKSPACE: &str = "\x7f";
    let sandbox = Sandbox::new();
    let mut term = Terminal::start(&sandbox, &["run", "--dry-run", "--", "true"]);
    term.wait(with_footer(
        |t| t.title().starts_with("Run expiry"),
        "\u{2190} back",
    ));
    for c in ["b", "a", "d"] {
        term.press(c);
    }
    term.press_until(ENTER, shows("Use a duration"));
    for _ in 0..3 {
        term.press(BACKSPACE);
    }
    for c in ["1", "0", "m"] {
        term.press(c);
    }
    term.press_until(ENTER, shows("reserve time for downloading results"));
    assert!(
        term.running(),
        "an invalid expiry must keep the prompt open"
    );
    for _ in 0..3 {
        term.press(BACKSPACE);
    }
    for c in ["3", "0", "m"] {
        term.press(c);
    }
    term.press_until(ENTER, on("GPU"));
    term.wait(shows("Consumer RTX 40 series        from $0.300/hr"));
    for c in "modal t4".chars() {
        term.press(&c.to_string());
    }
    term.wait(shows(
        "Modal helper failed. Retry gpu setup --provider modal.",
    ));
    assert!(shows("Modal T4")(&term));
    assert!(
        !term.output_text().contains("unexpected call"),
        "helper stderr must not be written over the picker"
    );
    term.press("\x03");
    term.press("\x03");
    assert_eq!(term.exit_status(), 130);
    assert!(!sandbox
        .calls()
        .iter()
        .any(|call| call.iter().any(|a| a == "create")));

    // Without a terminal the explicit usage error remains.
    let out = sandbox
        .command(&["run", "--gpu", "RTX 4090", "--dry-run", "--", "true"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr)
        .contains("gpu run requires --expire DURATION or default_expire in config.json."));
}

#[test]
fn test_explicit_vast_live_run_picker_never_starts_the_modal_helper() {
    let sandbox = Sandbox::new();
    let mut term = Terminal::start(
        &sandbox,
        &["run", "--provider", "vast", "--expire", "30m", "--", "true"],
    );
    term.wait(on("GPU"));
    term.wait(shows("Consumer RTX 40 series        from $0.300/hr"));
    assert!(!shows("Modal")(&term));
    term.press("\x03");
    term.press("\x03");
    assert_eq!(term.exit_status(), 130);
    assert!(!sandbox.calls().iter().any(|call| call[0] == "uv"));
}

#[test]
fn test_run_with_modal_provider_and_no_gpu_opens_a_modal_only_picker() {
    let sandbox = Sandbox::new();
    let mut term = Terminal::start(
        &sandbox,
        &[
            "run",
            "--provider",
            "modal",
            "--expire",
            "30m",
            "--",
            "true",
        ],
    );
    term.wait(|t| on("GPU")(t) && shows("Modal helper failed")(t));
    assert!(shows("Modal T4")(&term));
    assert!(!shows("Consumer RTX")(&term));
    assert!(!shows("TPU")(&term));
    term.press("\x03");
    term.press("\x03");
    assert_eq!(term.exit_status(), 130);
    assert!(sandbox.calls().iter().all(|call| call[0] == "uv"));
}

#[test]
fn test_local_directory_choice_dry_run_reports_the_upload_in_terminal() {
    let sandbox = Sandbox::new();
    let proj = sandbox.home.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("train.py"), "print('hi')\n").unwrap();
    let mut term = gpu_up(&sandbox);
    term.wait(shows("No repository"));
    let repository = term.lines();
    assert!(
        repository
            .iter()
            .any(|l| l.contains("7. From local directory...")),
        "{}",
        repository.join("\n")
    );
    println!("--- Repository ---\n{}", repository.join("\n"));
    term.press_until("7", on("From local directory"));
    for c in ["~", "/", "p", "r", "o", "j"] {
        term.press(c);
    }
    term.wait(shows("~/proj"));
    println!("--- From local directory ---\n{}", term.lines().join("\n"));
    term.press_until(RIGHT, on("Mode"));
    term.press_until("1", on("GPU"));
    for c in ["t", "4"] {
        term.press(c);
    }
    term.press_until(RIGHT, on("Offer: Tesla T4"));
    term.wait(shows("id=100"));
    term.press(RIGHT);
    assert_eq!(term.exit_status(), 0);
    let output = term.output_text();
    println!("--- Final screen ---\n{}", term.lines().join("\n"));
    assert!(output.contains("DRY RUN: no instance created"), "{output}");
    assert!(output.contains("\"no-repo\""), "{output}");
    assert!(output.contains("DRY RUN: would copy 1 file"), "{output}");
    for call in sandbox.calls() {
        assert!(
            identity_read(&call)
                || call[1..3] == ["repo", "list"]
                || call[1..3] == ["search", "offers"],
            "{call:?}"
        );
    }
}

#[test]
fn test_gone_offer_shows_the_error_in_the_offer_list_and_a_second_pick_launches() {
    let sandbox = Sandbox::new();
    let gone = r#"{"error": true, "status_code": 404, "msg": "error 404/3603: no_such_ask Instance type by id 56377751 is not available."}"#;
    let created = r#"{"success":true,"new_contract":123,"instance_api_key":"private-test-key"}"#;
    let attempts = sandbox.root.join("creates");
    sandbox.stub("vastai", &common::FAKE_CLI.replace(
        "case \"$tool $1 $2\" in",
        &format!(
            "case \"$tool $1 $2\" in\n    \"vastai create instance\")\n        if [ -e '{marker}' ]; then echo '{created}'; else touch '{marker}'; echo '{gone}' >&2; fi ;;",
            marker = attempts.display(),
        ),
    ));
    let mut term = Terminal::start(
        &sandbox,
        &[
            "up",
            "--repo",
            "example/gpu-cli",
            "--mode",
            "container",
            "--gpu",
            "RTX 4090",
        ],
    );
    term.wait(on("Offer: RTX 4090"));
    term.wait(shows("id=100"));
    term.press_until(
        RIGHT,
        with_footer(shows("Type 'yes' to confirm:"), "← back"),
    );
    term.press("yes");
    let notice = "Vast has no capacity for this offer: error 404/3603: no_such_ask Instance type by id 56377751 is not available.";
    term.press_until(ENTER, |t| {
        on("Offer: RTX 4090")(t) && shows("id=100")(t) && shows("no_such_ask")(t)
    });
    let after_failure = term.lines();
    println!(
        "--- Offer list after the failed create ---\n{}",
        after_failure.join("\n")
    );
    assert!(term.running(), "the failed create must not exit gpu up");
    // The notice is wider than the terminal, so it spans two rows.
    assert!(
        after_failure.concat().contains(notice),
        "{}",
        after_failure.join("\n")
    );
    assert!(!term.output_text().contains("Next: gpu up with another"));
    term.press_until(
        RIGHT,
        with_footer(shows("Type 'yes' to confirm:"), "← back"),
    );
    term.press("yes");
    term.press_until(ENTER, shows("Open workspace"));
    println!("--- After the second pick ---\n{}", term.lines().join("\n"));
    term.press("2");
    assert_eq!(term.exit_status(), 0, "{}", term.output_text());
    let creates = sandbox
        .calls()
        .iter()
        .filter(|a| a.iter().any(|s| s == "create"))
        .count();
    assert_eq!(creates, 2);
}
