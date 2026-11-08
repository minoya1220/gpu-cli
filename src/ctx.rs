//! The seams that replace Python's monkeypatching: environment snapshot,
//! process runner, clock, console and picker UI. Nothing reads process-wide
//! state after `Ctx::real()` builds the snapshot, so tests run in parallel.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::process::{Cmd, Output, RealRunner, RunError, Runner};

const ENV_KEYS: [&str; 6] = [
    "HOME",
    "PATH",
    "TZ",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
];

/// The environment variables the tool reads, captured once.
#[derive(Clone, Debug, Default)]
pub struct Env {
    vars: BTreeMap<String, String>,
}

impl Env {
    pub fn from_process() -> Env {
        let mut vars = BTreeMap::new();
        for key in ENV_KEYS {
            if let Some(value) = std::env::var_os(key) {
                vars.insert(key.to_string(), value.to_string_lossy().into_owned());
            }
        }
        if !vars.contains_key("HOME") {
            #[allow(deprecated)]
            if let Some(home) = std::env::home_dir() {
                vars.insert("HOME".into(), home.to_string_lossy().into_owned());
            }
        }
        // --env NAME is resolved from this snapshot, never re-read mid-run.
        vars.extend(
            std::env::vars_os().filter_map(|(key, value)| {
                Some((key.into_string().ok()?, value.into_string().ok()?))
            }),
        );
        Env { vars }
    }

    pub fn with(mut self, key: &str, value: impl Into<String>) -> Env {
        self.vars.insert(key.to_string(), value.into());
        self
    }

    pub fn without(mut self, key: &str) -> Env {
        self.vars.remove(key);
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    pub fn home(&self) -> PathBuf {
        PathBuf::from(self.get("HOME").unwrap_or("/"))
    }

    /// An XDG base directory; empty counts as unset, per the XDG spec.
    pub fn xdg(&self, var: &str, default_under_home: &str) -> PathBuf {
        match self.get(var) {
            Some(value) if !value.is_empty() => PathBuf::from(value),
            _ => self.home().join(default_under_home),
        }
    }
}

/// Monotonic time, sleeping, and the wall clock.
pub trait Clock: Send + Sync {
    fn monotonic(&self) -> f64;
    fn sleep(&self, seconds: f64);
    fn now(&self) -> jiff::Timestamp;
}

pub struct RealClock {
    start: Instant,
}

impl Clock for RealClock {
    fn monotonic(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn sleep(&self, seconds: f64) {
        if seconds > 0.0 {
            std::thread::sleep(Duration::from_secs_f64(seconds));
        }
    }

    fn now(&self) -> jiff::Timestamp {
        jiff::Timestamp::now()
    }
}

enum ConsoleIo {
    Real,
    Buffer {
        /// `None` means any read is a test failure.
        input: Option<VecDeque<String>>,
        out: Vec<u8>,
        err: Vec<u8>,
    },
}

/// stdout, stderr and line input (`print` and `input()`).
pub struct Console {
    io: Mutex<ConsoleIo>,
    interactive: bool,
}

impl Console {
    pub fn real() -> Console {
        let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
        Console {
            io: Mutex::new(ConsoleIo::Real),
            interactive,
        }
    }

    /// A captured console; `input` lines answer prompts, `None` forbids them.
    pub fn buffer(input: Option<Vec<&str>>, interactive: bool) -> Console {
        let input = input.map(|lines| lines.into_iter().map(String::from).collect());
        Console {
            io: Mutex::new(ConsoleIo::Buffer {
                input,
                out: Vec::new(),
                err: Vec::new(),
            }),
            interactive,
        }
    }

    /// stdin and stdout are both terminals.
    pub fn interactive(&self) -> bool {
        self.interactive
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ConsoleIo> {
        self.io
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn write_out(&self, text: &str) -> Result<()> {
        self.write_out_bytes(text.as_bytes())
    }

    pub(crate) fn write_out_bytes(&self, text: &[u8]) -> Result<()> {
        match &mut *self.lock() {
            ConsoleIo::Real => {
                let mut stdout = io::stdout().lock();
                stdout.write_all(text)?;
                stdout.flush()?;
            }
            ConsoleIo::Buffer { out, .. } => out.extend_from_slice(text),
        }
        Ok(())
    }

    /// `print(text)`.
    pub fn println(&self, text: &str) -> Result<()> {
        self.write_out(&format!("{text}\n"))
    }

    pub(crate) fn stderr_terminal(&self) -> bool {
        match &*self.lock() {
            ConsoleIo::Real => io::stderr().is_terminal(),
            ConsoleIo::Buffer { .. } => self.interactive,
        }
    }

    pub(crate) fn write_err(&self, text: &str) -> Result<()> {
        self.write_err_bytes(text.as_bytes())
    }

    pub(crate) fn write_err_bytes(&self, text: &[u8]) -> Result<()> {
        match &mut *self.lock() {
            ConsoleIo::Real => {
                let mut stderr = io::stderr().lock();
                stderr.write_all(text)?;
                stderr.flush()?;
            }
            ConsoleIo::Buffer { err, .. } => err.extend_from_slice(text),
        }
        Ok(())
    }

    /// `print(text, file=sys.stderr)`.
    pub fn eprintln(&self, text: &str) -> Result<()> {
        self.write_err(&format!("{text}\n"))
    }

    /// `input(prompt)`: `Ok(None)` at end of input.
    pub fn input(&self, prompt: &str) -> Result<Option<String>> {
        self.write_out(prompt)?;
        match &mut *self.lock() {
            ConsoleIo::Real => {
                let mut line = String::new();
                if io::stdin().lock().read_line(&mut line)? == 0 {
                    return Ok(None);
                }
                if line.ends_with('\n') {
                    line.pop();
                }
                Ok(Some(line))
            }
            ConsoleIo::Buffer { input, .. } => match input {
                Some(lines) => Ok(lines.pop_front()),
                None => panic!("unexpected prompt: {prompt}"),
            },
        }
    }

    pub fn take_out(&self) -> String {
        match &mut *self.lock() {
            ConsoleIo::Real => String::new(),
            ConsoleIo::Buffer { out, .. } => {
                String::from_utf8_lossy(&std::mem::take(out)).into_owned()
            }
        }
    }

    pub fn take_err(&self) -> String {
        match &mut *self.lock() {
            ConsoleIo::Real => String::new(),
            ConsoleIo::Buffer { err, .. } => {
                String::from_utf8_lossy(&std::mem::take(err)).into_owned()
            }
        }
    }

    /// Replace the remaining scripted input lines.
    pub fn set_input(&self, lines: Option<Vec<&str>>) {
        if let ConsoleIo::Buffer { input, .. } = &mut *self.lock() {
            *input = lines.map(|lines| lines.into_iter().map(String::from).collect());
        }
    }
}

/// Where pickers run: the real terminal, or scripted key bytes per picker.
pub enum Ui {
    Terminal,
    Scripted(Mutex<VecDeque<Vec<u8>>>),
}

impl Ui {
    pub fn scripted<K: AsRef<[u8]>>(pickers: &[K]) -> Ui {
        Ui::Scripted(Mutex::new(
            pickers.iter().map(|k| k.as_ref().to_vec()).collect(),
        ))
    }

    /// Keys for the next scripted picker; panics when none was expected.
    pub fn next_script(&self) -> Option<Vec<u8>> {
        match self {
            Ui::Terminal => None,
            Ui::Scripted(queue) => {
                let next = queue.lock().unwrap_or_else(|p| p.into_inner()).pop_front();
                Some(next.unwrap_or_else(|| panic!("unexpected picker")))
            }
        }
    }

    pub fn remaining(&self) -> usize {
        match self {
            Ui::Terminal => 0,
            Ui::Scripted(queue) => queue.lock().unwrap_or_else(|p| p.into_inner()).len(),
        }
    }
}

#[derive(Clone)]
pub struct Ctx {
    pub env: Arc<Env>,
    runner: Arc<dyn Runner>,
    gcp_setup_session: bool,
    pub(crate) gcp_tokens: Arc<Mutex<BTreeMap<String, String>>>,
    pub(crate) gcp_price_lock: Arc<Mutex<()>>,
    pub(crate) gcp_catalog_lock: Arc<Mutex<()>>,
    pub(crate) gcp_sku_refreshed: Arc<Mutex<bool>>,
    pub clock: Arc<dyn Clock>,
    pub console: Arc<Console>,
    pub ui: Arc<Ui>,
    output: Arc<Mutex<crate::output::CommandOutput>>,
    interrupt_notice: Arc<Mutex<Option<String>>>,
    cooperative: Arc<AtomicBool>,
    interrupts: Arc<AtomicUsize>,
}

impl Ctx {
    pub fn real() -> Ctx {
        Ctx {
            env: Arc::new(Env::from_process()),
            runner: Arc::new(RealRunner),
            gcp_setup_session: false,
            gcp_tokens: Arc::default(),
            gcp_price_lock: Arc::default(),
            gcp_catalog_lock: Arc::default(),
            gcp_sku_refreshed: Arc::default(),
            clock: Arc::new(RealClock {
                start: Instant::now(),
            }),
            console: Arc::new(Console::real()),
            ui: Arc::new(Ui::Terminal),
            output: Arc::default(),
            interrupt_notice: Arc::default(),
            cooperative: Arc::default(),
            interrupts: Arc::default(),
        }
    }

    pub fn new(
        env: Arc<Env>,
        runner: Arc<dyn Runner>,
        clock: Arc<dyn Clock>,
        console: Arc<Console>,
        ui: Arc<Ui>,
    ) -> Self {
        Self {
            env,
            runner,
            clock,
            console,
            ui,
            gcp_setup_session: false,
            gcp_tokens: Arc::default(),
            gcp_price_lock: Arc::default(),
            gcp_catalog_lock: Arc::default(),
            gcp_sku_refreshed: Arc::default(),
            output: Arc::default(),
            interrupt_notice: Arc::default(),
            cooperative: Arc::default(),
            interrupts: Arc::default(),
        }
    }

    /// Fresh output state for one invocation, shared by its worker Jobs.
    pub(crate) fn for_command(&self, json: bool) -> Self {
        Self {
            output: Arc::new(Mutex::new(crate::output::CommandOutput {
                json,
                ..Default::default()
            })),
            ..self.clone()
        }
    }

    pub fn json(&self) -> bool {
        self.output.lock().unwrap_or_else(|p| p.into_inner()).json
    }

    pub(crate) fn result(&self, key: &str, value: crate::pyjson::Value) {
        self.output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .result
            .insert(key, value);
    }

    /// What an interrupted command must still tell the user, such as a
    /// created instance that keeps running. Shared by every copy of this
    /// `Ctx`, including the one the SIGINT handler holds.
    pub(crate) fn set_interrupt_notice(&self, notice: Option<String>) {
        *self
            .interrupt_notice
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = notice;
    }

    pub fn cooperative(&self) -> bool {
        self.cooperative.load(Ordering::SeqCst)
    }

    pub(crate) fn begin_run(&self) {
        self.interrupts.store(0, Ordering::SeqCst);
        self.cooperative.store(true, Ordering::SeqCst);
    }

    pub(crate) fn end_run(&self) {
        self.cooperative.store(false, Ordering::SeqCst);
    }

    pub fn interrupt(&self) -> usize {
        self.interrupts.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn interrupts(&self) -> usize {
        self.interrupts.load(Ordering::SeqCst)
    }

    pub(crate) fn check_cancelled(&self) -> Result<()> {
        if self.cooperative() && self.interrupts() > 0 {
            Err(Error::Cancelled(130))
        } else {
            Ok(())
        }
    }

    pub fn interrupt_notice(&self) -> Option<String> {
        self.interrupt_notice
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub(crate) fn set_ssh_timeout(&self, seconds: f64) {
        self.output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .ssh_timeout = Some(seconds);
    }

    pub(crate) fn ssh_timeout(&self) -> f64 {
        self.output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .ssh_timeout
            .unwrap_or(120.0)
    }

    pub(crate) fn result_snapshot(&self) -> crate::pyjson::Dict {
        self.output
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .result
            .clone()
    }

    /// Only setup, after explicit provider selection, may use this scoped copy.
    pub(crate) fn for_gcp_setup(&self) -> Self {
        Self {
            gcp_setup_session: true,
            ..self.clone()
        }
    }

    pub fn require_gcloud(&self) -> Result<()> {
        if self.gcp_setup_session
            || crate::config::load_config(&self.env).is_ok_and(|cfg| crate::setup::gcp_ready(&cfg))
        {
            Ok(())
        } else {
            Err(Error::status(
                crate::error::ExitCode::NotSetup,
                crate::setup::GCP_NOT_SETUP,
            ))
        }
    }

    /// The single process boundary: the underlying runner is private so no
    /// cloud adapter, catalog Job, or exec path can bypass the opt-in gate.
    fn authorize(&self, program: &str) -> std::result::Result<(), RunError> {
        if std::path::Path::new(program)
            .file_name()
            .is_some_and(|n| n == "gcloud")
        {
            self.require_gcloud()
                .map_err(|e| RunError::Denied(e.text().into()))?;
        }
        Ok(())
    }

    pub fn run(&self, cmd: &Cmd) -> std::result::Result<Output, RunError> {
        self.authorize(cmd.program())?;
        self.runner.run(cmd)
    }

    pub(crate) fn transfer(
        &self,
        cmd: &Cmd,
        threshold: usize,
    ) -> std::result::Result<Output, RunError> {
        self.authorize(cmd.program())?;
        self.runner
            .run_cancellable(cmd, &self.interrupts, threshold)
    }

    pub fn exec(&self, argv: &[String]) -> String {
        if let Err(error) = self.authorize(argv.first().map(String::as_str).unwrap_or("")) {
            return error.python_text("gcloud");
        }
        self.runner.exec(argv)
    }

    #[cfg(test)]
    pub(crate) fn with_runner(mut self, runner: Arc<dyn Runner>) -> Self {
        self.runner = runner;
        self
    }

    pub fn println(&self, text: &str) -> Result<()> {
        if self.json() {
            self.progress(text)
        } else {
            self.console.println(text)
        }
    }

    pub fn eprintln(&self, text: &str) -> Result<()> {
        self.console.eprintln(text)
    }

    /// GPU-owned progress is useful in a terminal, but must not mix with a
    /// remote command's stderr or a non-terminal command's machine output.
    pub(crate) fn progress(&self, text: &str) -> Result<()> {
        if self.interactive() {
            self.eprintln(text)
        } else {
            Ok(())
        }
    }

    pub fn interactive(&self) -> bool {
        self.console.interactive() && !self.json()
    }

    /// `input(prompt)`; end of input becomes `Err(on_eof)`.
    pub fn input_or(&self, prompt: &str, on_eof: Error) -> Result<String> {
        if !self.interactive() {
            return Err(crate::output::usage(
                "Cannot prompt without a terminal; pass the missing command flag (see --help).",
            ));
        }
        self.console.input(prompt)?.ok_or(on_eof)
    }
}
