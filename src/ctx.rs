//! The seams that replace Python's monkeypatching: environment snapshot,
//! process runner, clock, console and picker UI. Nothing reads process-wide
//! state after `Ctx::real()` builds the snapshot, so tests run in parallel.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;
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
        match &mut *self.lock() {
            ConsoleIo::Real => {
                let mut stdout = io::stdout().lock();
                stdout.write_all(text.as_bytes())?;
                stdout.flush()?;
            }
            ConsoleIo::Buffer { out, .. } => out.extend_from_slice(text.as_bytes()),
        }
        Ok(())
    }

    /// `print(text)`.
    pub fn println(&self, text: &str) -> Result<()> {
        self.write_out(&format!("{text}\n"))
    }

    /// `print(text, file=sys.stderr)`.
    pub fn eprintln(&self, text: &str) -> Result<()> {
        let line = format!("{text}\n");
        match &mut *self.lock() {
            ConsoleIo::Real => {
                let mut stderr = io::stderr().lock();
                stderr.write_all(line.as_bytes())?;
                stderr.flush()?;
            }
            ConsoleIo::Buffer { err, .. } => err.extend_from_slice(line.as_bytes()),
        }
        Ok(())
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
    pub clock: Arc<dyn Clock>,
    pub console: Arc<Console>,
    pub ui: Arc<Ui>,
}

impl Ctx {
    pub fn real() -> Ctx {
        Ctx {
            env: Arc::new(Env::from_process()),
            runner: Arc::new(RealRunner),
            gcp_setup_session: false,
            clock: Arc::new(RealClock {
                start: Instant::now(),
            }),
            console: Arc::new(Console::real()),
            ui: Arc::new(Ui::Terminal),
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
        }
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
            Err(Error::msg(crate::setup::GCP_NOT_SETUP))
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
        self.console.println(text)
    }

    pub fn eprintln(&self, text: &str) -> Result<()> {
        self.console.eprintln(text)
    }

    pub fn interactive(&self) -> bool {
        self.console.interactive()
    }

    /// `input(prompt)`; end of input becomes `Err(on_eof)`.
    pub fn input_or(&self, prompt: &str, on_eof: Error) -> Result<String> {
        self.console.input(prompt)?.ok_or(on_eof)
    }
}
