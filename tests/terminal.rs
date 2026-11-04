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
        ["gh", "vastai"],
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
    assert_eq!(tools(&sandbox), ["gh", "vastai"]);
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
        term.press_until(ENTER, shows("Set up Google Cloud before renting TPUs"));
        assert!(term.running(), "setup must remain visible until dismissed");
        let screen = term.lines().join("\n");
        assert!(screen.contains("No TPU created."), "{screen}");
        assert!(
            screen.contains("Next: gpu up --provider gcp --help"),
            "{screen}"
        );
        let config = sandbox.root.join("config/gpu/config.json");
        assert!(
            term.lines()
                .join("")
                .contains(&config.display().to_string()),
            "{screen}"
        );
        assert_eq!(screen.matches("Next:").count(), 1);
        assert!(tools(&sandbox).iter().all(|tool| tool == "gh"));
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
        std::fs::write(config.join("config.json"), r#"{"gcp":{"project":"test-project","zone":"us-central1-a","runtime":"v2-alpha-tpuv5lite","expiry":{"queue":"gpu-expiry","location":"us-central1","service_account":"reaper@test-project.iam.gserviceaccount.com"}}}"#).unwrap();
        // The only cloud call allowed is a read-only catalog request. All
        // lifecycle, IAM and task operations fail if this dry run reaches them.
        sandbox.stub("gcloud", r#"#!/bin/sh
{ printf 'gcloud'; for arg in "$@"; do printf '\t%s' "$arg"; done; printf '\n'; } >> "$FAKE_CLI_LOG"
if [ "$*" != 'compute tpus tpu-vm accelerator-types list --project=test-project --quiet --zone=us-central1-a --format=json' ]; then
    echo 'unexpected cloud call' >&2
    exit 97
fi
printf '[{"type":"v5litepod-1"}]\n'
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
            term.wait(shows("v5litepod-1"));
        }
        term.press_until(ENTER, on("TPU capacity"));
        term.press("2"); // Spot
        if query == "v5e" {
            term.wait(on("Destroy after (e.g. 2h)"));
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
        assert!(output.contains("--spot"));
        assert!(output.contains("--method=DELETE"));
        assert!(output.contains("https://github.com/example/demo /workspace/demo"));
        assert_eq!(
            tools(&sandbox),
            if query == "tpu" {
                vec!["gcloud"]
            } else {
                vec![]
            }
        );
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
