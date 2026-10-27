//! Test doubles for the `Ctx` seams: a scripted runner that records every
//! command, a fake clock that advances on sleep, and isolated temp HOME and
//! XDG directories (Python's `isolate_user_directories` fixture).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use jiff::Timestamp;

use crate::ctx::{Clock, Console, Ctx, Env, Ui};
use crate::process::{Cmd, Output, RunError, Runner};
use crate::pyjson::{self, Dict, Value};

type Handler = Box<dyn Fn(&Cmd) -> Result<Output, RunError> + Send + Sync>;

/// Answers commands with a handler and records them. A handler panics on a
/// command it does not expect, which fails the test.
pub struct FakeRunner {
    handler: Handler,
    calls: Mutex<Vec<Cmd>>,
}

impl FakeRunner {
    pub fn new(
        handler: impl Fn(&Cmd) -> Result<Output, RunError> + Send + Sync + 'static,
    ) -> Arc<FakeRunner> {
        Arc::new(FakeRunner {
            handler: Box::new(handler),
            calls: Mutex::new(Vec::new()),
        })
    }

    /// Fails the test on any command.
    pub fn forbid() -> Arc<FakeRunner> {
        FakeRunner::new(|cmd| panic!("unexpected command: {:?}", cmd.argv))
    }

    pub fn calls(&self) -> Vec<Cmd> {
        self.calls.lock().unwrap().clone()
    }

    pub fn argvs(&self) -> Vec<Vec<String>> {
        self.calls().into_iter().map(|c| c.argv).collect()
    }
}

impl Runner for FakeRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output, RunError> {
        self.calls.lock().unwrap().push(cmd.clone());
        (self.handler)(cmd)
    }

    fn exec(&self, argv: &[String]) -> String {
        self.calls.lock().unwrap().push(Cmd::new(argv));
        "exec is not available in tests".into()
    }
}

/// Monotonic time that only moves when something sleeps.
pub struct FakeClock {
    elapsed: Mutex<f64>,
    start: Timestamp,
}

impl FakeClock {
    pub fn at(start: Timestamp) -> Arc<FakeClock> {
        Arc::new(FakeClock {
            elapsed: Mutex::new(0.0),
            start,
        })
    }

    pub fn now_real() -> Arc<FakeClock> {
        FakeClock::at(Timestamp::now())
    }

    pub fn elapsed(&self) -> f64 {
        *self.elapsed.lock().unwrap()
    }
}

impl Clock for FakeClock {
    fn monotonic(&self) -> f64 {
        self.elapsed()
    }

    fn sleep(&self, seconds: f64) {
        *self.elapsed.lock().unwrap() += seconds;
    }

    fn now(&self) -> Timestamp {
        let nanos = (self.elapsed() * 1e9) as i64;
        self.start
            .checked_add(jiff::Span::new().nanoseconds(nanos))
            .unwrap()
    }
}

/// Fresh HOME, XDG directories and an empty PATH directory per test.
pub struct TestEnv {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
    pub cache: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub bin: PathBuf,
}

impl TestEnv {
    pub fn new() -> TestEnv {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let [home, cache, config, state, bin] =
            ["home", "cache", "config", "state", "bin"].map(|d| {
                let path = root.join(d);
                std::fs::create_dir(&path).unwrap();
                path
            });
        TestEnv {
            _dir: dir,
            root,
            home,
            cache,
            config,
            state,
            bin,
        }
    }

    pub fn env(&self) -> Env {
        Env::default()
            .with("HOME", self.home.to_string_lossy())
            .with("XDG_CACHE_HOME", self.cache.to_string_lossy())
            .with("XDG_CONFIG_HOME", self.config.to_string_lossy())
            .with("XDG_STATE_HOME", self.state.to_string_lossy())
            .with("PATH", self.bin.to_string_lossy())
            .with("TZ", "UTC")
    }

    /// An executable stub on this test's PATH.
    pub fn executable(&self, name: &str, body: &str) -> PathBuf {
        let path = self.bin.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    pub fn history_path(&self) -> PathBuf {
        self.state.join("gpu").join("history.json")
    }

    pub fn ctx(&self) -> CtxBuilder {
        CtxBuilder {
            env: self.env(),
            runner: FakeRunner::forbid(),
            clock: FakeClock::now_real(),
            input: None,
            interactive: false,
            ui: Vec::new(),
        }
    }
}

pub struct CtxBuilder {
    pub env: Env,
    runner: Arc<FakeRunner>,
    clock: Arc<FakeClock>,
    input: Option<Vec<String>>,
    interactive: bool,
    ui: Vec<Vec<u8>>,
}

impl CtxBuilder {
    pub fn runner(mut self, runner: &Arc<FakeRunner>) -> Self {
        self.runner = runner.clone();
        self
    }

    pub fn clock(mut self, clock: &Arc<FakeClock>) -> Self {
        self.clock = clock.clone();
        self
    }

    /// Lines answering prompts; without this, any prompt fails the test.
    pub fn input(mut self, lines: &[&str]) -> Self {
        self.input = Some(lines.iter().map(|s| s.to_string()).collect());
        self
    }

    pub fn interactive(mut self, on: bool) -> Self {
        self.interactive = on;
        self
    }

    /// Key bytes for each picker the command opens, in order.
    pub fn pickers(mut self, keys: &[&str]) -> Self {
        self.ui = keys.iter().map(|k| k.as_bytes().to_vec()).collect();
        self
    }

    pub fn env(mut self, env: Env) -> Self {
        self.env = env;
        self
    }

    pub fn build(self) -> Ctx {
        let input = self
            .input
            .as_ref()
            .map(|lines| lines.iter().map(String::as_str).collect());
        Ctx {
            env: Arc::new(self.env),
            runner: self.runner,
            clock: self.clock,
            console: Arc::new(Console::buffer(input, self.interactive)),
            ui: Arc::new(Ui::scripted(&self.ui)),
        }
    }
}

pub fn out(ctx: &Ctx) -> String {
    ctx.console.take_out()
}

pub fn err(ctx: &Ctx) -> String {
    ctx.console.take_err()
}

pub fn argv(parts: &[&str]) -> Vec<String> {
    std::iter::once("gpu")
        .chain(parts.iter().copied())
        .map(String::from)
        .collect()
}

/// Run the CLI; the exit status.
pub fn main(ctx: &Ctx, parts: &[&str]) -> i32 {
    crate::main_with(&argv(parts), ctx)
}

pub fn json(text: &str) -> Value {
    pyjson::loads(text).unwrap_or_else(|e| panic!("invalid JSON in test: {e}: {text}"))
}

pub fn dict(text: &str) -> Dict {
    match json(text) {
        Value::Dict(d) => d,
        other => panic!("not an object: {other:?}"),
    }
}

pub fn dicts(text: &str) -> Vec<Dict> {
    match json(text) {
        Value::List(items) => items
            .into_iter()
            .map(|i| i.as_dict().cloned().unwrap())
            .collect(),
        other => panic!("not a list: {other:?}"),
    }
}

pub fn ok(stdout: &str) -> Result<Output, RunError> {
    Ok(Output::ok(stdout))
}

pub fn strs(argv: &[String]) -> Vec<&str> {
    argv.iter().map(String::as_str).collect()
}

pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}
