use crate::sync::MutexExt;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::redact::redact_text;

#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// `stdin=subprocess.DEVNULL`.
    Null,
    /// The terminal, inherited.
    Inherit,
    /// Text written to the child's stdin, then closed.
    Text(String),
    /// Stream a file without loading an archive into memory.
    File(std::path::PathBuf),
}

/// One process to run: argv plus how to connect it.
#[derive(Clone, Debug, PartialEq)]
pub struct Cmd {
    pub argv: Vec<String>,
    pub stdin: Input,
    /// Capture stdout and stderr; otherwise they are inherited.
    pub capture: bool,
    pub timeout: Option<f64>,
    pub normalize_newlines: bool,
    /// Binary stdout destination (archives and byte-exact log chunks).
    pub stdout_file: Option<std::path::PathBuf>,
    /// A new process group: terminal Ctrl-C then reaches only the CLI, which
    /// decides whether to cancel the child.
    pub process_group: bool,
}

impl Cmd {
    pub fn new<S: AsRef<str>>(argv: &[S]) -> Cmd {
        Cmd {
            argv: argv.iter().map(|a| a.as_ref().to_string()).collect(),
            stdin: Input::Null,
            capture: true,
            timeout: None,
            normalize_newlines: true,
            stdout_file: None,
            process_group: false,
        }
    }

    pub fn stdin(mut self, stdin: Input) -> Cmd {
        self.stdin = stdin;
        self
    }

    pub fn capture(mut self, capture: bool) -> Cmd {
        self.capture = capture;
        self
    }

    pub fn timeout(mut self, timeout: Option<f64>) -> Cmd {
        self.timeout = timeout;
        self
    }

    pub fn raw_output(mut self) -> Cmd {
        self.normalize_newlines = false;
        self
    }

    pub fn stdout_file(mut self, path: impl Into<std::path::PathBuf>) -> Cmd {
        self.stdout_file = Some(path.into());
        self
    }

    pub fn process_group(mut self) -> Cmd {
        self.process_group = true;
        self
    }

    pub fn program(&self) -> &str {
        self.argv.first().map(String::as_str).unwrap_or("")
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(stdout: impl Into<String>) -> Output {
        Output {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    pub fn status(code: i32) -> Output {
        Output {
            code,
            ..Output::default()
        }
    }

    pub fn failed(code: i32, stderr: impl Into<String>) -> Output {
        Output {
            code,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RunError {
    NotFound,
    TimedOut,
    Os(String),
    Denied(String),
}

impl RunError {
    /// One line describing why `program` could not complete.
    pub fn describe(&self, program: &str) -> String {
        match self {
            RunError::NotFound => format!("Required command not found: {program}"),
            RunError::TimedOut => format!("Command '{program}' timed out"),
            RunError::Os(text) | RunError::Denied(text) => text.clone(),
        }
    }
}

pub trait Runner: Send + Sync {
    fn run(&self, cmd: &Cmd) -> std::result::Result<Output, RunError>;

    /// Transfers can be interrupted without cancelling subsequent cleanup.
    fn run_cancellable(
        &self,
        cmd: &Cmd,
        _count: &std::sync::atomic::AtomicUsize,
        _threshold: usize,
    ) -> std::result::Result<Output, RunError> {
        self.run(cmd)
    }

    /// Start `cmd` detached from this process with stdout and stderr appended
    /// to `log`, and return its process ID without waiting for it.
    fn spawn_detached(
        &self,
        _cmd: &Cmd,
        _log: &std::path::Path,
    ) -> std::result::Result<u32, RunError> {
        Err(RunError::Os(
            "background processes are not supported".into(),
        ))
    }

    /// Replace the current process (`os.execvp`); returns only on failure.
    fn exec(&self, argv: &[String]) -> String;
}

/// Work that a termination signal must let finish (up to a limit) in a
/// background process, so a stop never lands between two halves of an update.
static CRITICAL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub struct Critical;

/// Mark a critical section until the guard drops.
pub fn critical() -> Critical {
    CRITICAL.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Critical
}

impl Drop for Critical {
    fn drop(&mut self) {
        CRITICAL.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Wait up to `limit` for every critical section to end.
pub fn wait_critical(limit: Duration) {
    let end = Instant::now() + limit;
    while CRITICAL.load(std::sync::atomic::Ordering::SeqCst) > 0 && Instant::now() < end {
        thread::sleep(Duration::from_millis(50));
    }
}

pub struct RealRunner;

fn translate_newlines(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Option<Receiver<Vec<u8>>> {
    pipe.map(|mut pipe| {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let _ = pipe.read_to_end(&mut buffer);
            let _ = sender.send(buffer);
        });
        receiver
    })
}

/// A drained pipe's text; `None` when the deadline passes first.
fn collect(
    receiver: Option<Receiver<Vec<u8>>>,
    deadline: Option<Instant>,
    normalize_newlines: bool,
) -> Option<String> {
    let Some(receiver) = receiver else {
        return Some(String::new());
    };
    let bytes = match deadline {
        None => receiver.recv().unwrap_or_default(),
        Some(deadline) => {
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(bytes) => bytes,
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => Vec::new(),
            }
        }
    };
    Some(if normalize_newlines {
        translate_newlines(&bytes)
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// Children given their own process group only so a timeout can kill their
/// whole tree. A terminal signal no longer reaches them, so the signal
/// handler forwards it with `forward_signal` before exiting.
static TIMEOUT_GROUPS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

struct TimeoutGroup(Option<i32>);

impl TimeoutGroup {
    fn register(pid: Option<i32>) -> TimeoutGroup {
        if let Some(pid) = pid {
            TIMEOUT_GROUPS.lock_recover().push(pid);
        }
        TimeoutGroup(pid)
    }
}

impl Drop for TimeoutGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            TIMEOUT_GROUPS.lock_recover().retain(|p| *p != pid);
        }
    }
}

/// Deliver a terminal signal to every timeout-isolated child group.
pub fn forward_signal(signal: i32) {
    use rustix::process::Signal;
    let signal = match signal {
        signal_hook::consts::SIGINT => Signal::INT,
        signal_hook::consts::SIGHUP => Signal::HUP,
        _ => Signal::TERM,
    };
    for pid in TIMEOUT_GROUPS.lock_recover().iter() {
        if let Some(pid) = rustix::process::Pid::from_raw(*pid) {
            let _ = rustix::process::kill_process_group(pid, signal);
        }
    }
}

fn kill(child: &mut Child, group: bool) {
    if group {
        if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn wait(
    child: &mut Child,
    group: bool,
    deadline: Option<Instant>,
    cancellation: Option<(&std::sync::atomic::AtomicUsize, usize)>,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    if deadline.is_none() && cancellation.is_none() {
        return child.wait().map(Some);
    }
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if deadline.is_some_and(|at| Instant::now() >= at)
            || cancellation.is_some_and(|(count, threshold)| {
                count.load(std::sync::atomic::Ordering::SeqCst) >= threshold
            })
        {
            kill(child, group);
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

impl RealRunner {
    fn execute(
        cmd: &Cmd,
        cancellation: Option<(&std::sync::atomic::AtomicUsize, usize)>,
    ) -> std::result::Result<Output, RunError> {
        use std::os::unix::process::{CommandExt, ExitStatusExt};

        let mut command = Command::new(cmd.program());
        command.args(&cmd.argv[1..]);
        let group = cmd.process_group || cmd.timeout.is_some();
        if group {
            command.process_group(0);
        }
        command.stdin(match cmd.stdin {
            Input::Null => Stdio::null(),
            Input::Inherit => Stdio::inherit(),
            Input::Text(_) => Stdio::piped(),
            Input::File(ref path) => {
                Stdio::from(std::fs::File::open(path).map_err(|err| RunError::Os(err.to_string()))?)
            }
        });
        if cmd.capture {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        if let Some(path) = &cmd.stdout_file {
            command.stdout(Stdio::from(
                std::fs::File::create(path).map_err(|err| RunError::Os(err.to_string()))?,
            ));
        }
        let mut child = command.spawn().map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => RunError::NotFound,
            _ => RunError::Os(crate::error::os_message(&err, None)),
        })?;
        let _registered =
            TimeoutGroup::register((group && !cmd.process_group).then(|| child.id() as i32));
        let deadline = cmd
            .timeout
            .map(|t| Instant::now() + Duration::from_secs_f64(t.max(0.0)));
        if let (Input::Text(text), Some(mut stdin)) = (&cmd.stdin, child.stdin.take()) {
            let text = text.clone();
            thread::spawn(move || {
                let _ = stdin.write_all(text.as_bytes());
            });
        }
        // Read both pipes on threads so a large output cannot deadlock.
        let stdout = drain(child.stdout.take());
        let stderr = drain(child.stderr.take());
        let status = wait(&mut child, group, deadline, cancellation)
            .map_err(|e| RunError::Os(e.to_string()))?
            .ok_or(RunError::TimedOut)?;
        let output = collect(stdout, deadline, cmd.normalize_newlines).zip(collect(
            stderr,
            deadline,
            cmd.normalize_newlines,
        ));
        let Some((stdout, stderr)) = output else {
            kill(&mut child, group);
            return Err(RunError::TimedOut);
        };
        let code = status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1));
        Ok(Output {
            code,
            stdout,
            stderr,
        })
    }
}

impl Runner for RealRunner {
    fn run(&self, cmd: &Cmd) -> std::result::Result<Output, RunError> {
        Self::execute(cmd, None)
    }

    fn run_cancellable(
        &self,
        cmd: &Cmd,
        count: &std::sync::atomic::AtomicUsize,
        threshold: usize,
    ) -> std::result::Result<Output, RunError> {
        Self::execute(cmd, Some((count, threshold)))
    }

    fn spawn_detached(
        &self,
        cmd: &Cmd,
        log: &std::path::Path,
    ) -> std::result::Result<u32, RunError> {
        use std::os::unix::fs::OpenOptionsExt;

        let open = || {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(log)
                .map_err(|err| RunError::Os(crate::error::os_message(&err, Some(log))))
        };
        let child = Command::new(cmd.program())
            .args(&cmd.argv[1..])
            .stdin(Stdio::null())
            .stdout(open()?)
            .stderr(open()?)
            .spawn()
            .map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => RunError::NotFound,
                _ => RunError::Os(crate::error::os_message(&err, None)),
            })?;
        Ok(child.id())
    }

    fn exec(&self, argv: &[String]) -> String {
        use std::os::unix::process::CommandExt;

        let err = Command::new(&argv[0]).args(&argv[1..]).exec();
        crate::error::os_message(&err, None)
    }
}

pub fn run<S: AsRef<str>>(ctx: &Ctx, argv: &[S], timeout: Option<f64>) -> Result<String> {
    run_output(ctx, argv, timeout).map(|output| output.stdout.trim().to_string())
}

/// `run`, keeping a successful command's full output, stderr included.
pub fn run_output<S: AsRef<str>>(ctx: &Ctx, argv: &[S], timeout: Option<f64>) -> Result<Output> {
    let cmd = Cmd::new(argv).timeout(timeout);
    checked(cmd.program(), ctx.run(&cmd))
}

/// Report a command result generically; provider adapters translate their
/// own failure signatures first.
pub(crate) fn checked(
    program: &str,
    result: std::result::Result<Output, RunError>,
) -> Result<Output> {
    match result {
        Ok(output) if output.code == 0 => Ok(output),
        Ok(output) => {
            let detail = failure_detail(&output);
            Err(Error::msg(if detail.is_empty() {
                format!("Command failed: {program}")
            } else {
                detail
            }))
        }
        Err(RunError::TimedOut) => Err(Error::timeout(format!("Timed out running {program}"))),
        Err(error) => Err(Error::msg(error.describe(program))),
    }
}

/// The redacted diagnostic of a failed command: stderr, else stdout.
pub(crate) fn failure_detail(output: &Output) -> String {
    let detail = if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    redact_text(detail.trim())
}

/// Find the first executable `name` on `PATH`.
pub(crate) fn which(env: &crate::ctx::Env, name: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let executable = |path: &std::path::Path| {
        std::fs::metadata(path).is_ok_and(|m| !m.is_dir() && m.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        let path = std::path::PathBuf::from(name);
        return executable(&path).then_some(path);
    }
    let search = env.get("PATH").unwrap_or("/bin:/usr/bin");
    search
        .split(':')
        .map(|dir| std::path::Path::new(dir).join(name))
        .find(|path| executable(path))
}
