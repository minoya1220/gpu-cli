//! Subprocesses: the `Runner` seam and Python's `run()` helper.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
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
}

/// One process to run: argv plus how to connect it.
#[derive(Clone, Debug, PartialEq)]
pub struct Cmd {
    pub argv: Vec<String>,
    pub stdin: Input,
    /// Capture stdout and stderr; otherwise they are inherited.
    pub capture: bool,
    pub timeout: Option<f64>,
}

impl Cmd {
    pub fn new<S: AsRef<str>>(argv: &[S]) -> Cmd {
        Cmd {
            argv: argv.iter().map(|a| a.as_ref().to_string()).collect(),
            stdin: Input::Null,
            capture: true,
            timeout: None,
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

    pub fn program(&self) -> &str {
        self.argv.first().map(String::as_str).unwrap_or("")
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Output {
    /// Exit status; a signal is reported as its negative number, like Python.
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
}

impl RunError {
    /// The text Python's subprocess exceptions carry when not handled.
    pub fn python_text(&self, program: &str) -> String {
        match self {
            RunError::NotFound => format!(
                "[Errno 2] No such file or directory: {}",
                crate::pyfmt::repr_str(program)
            ),
            RunError::TimedOut => format!("Command '{program}' timed out"),
            RunError::Os(text) => text.clone(),
        }
    }
}

pub trait Runner: Send + Sync {
    fn run(&self, cmd: &Cmd) -> std::result::Result<Output, RunError>;

    /// Replace the current process (`os.execvp`); returns only on failure.
    fn exec(&self, argv: &[String]) -> String;
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
fn collect(receiver: Option<Receiver<Vec<u8>>>, deadline: Option<Instant>) -> Option<String> {
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
    Some(translate_newlines(&bytes))
}

fn wait(
    child: &mut Child,
    deadline: Option<Instant>,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let Some(deadline) = deadline else {
        return child.wait().map(Some);
    };
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

impl Runner for RealRunner {
    fn run(&self, cmd: &Cmd) -> std::result::Result<Output, RunError> {
        use std::os::unix::process::ExitStatusExt;

        let mut command = Command::new(cmd.program());
        command.args(&cmd.argv[1..]);
        command.stdin(match cmd.stdin {
            Input::Null => Stdio::null(),
            Input::Inherit => Stdio::inherit(),
            Input::Text(_) => Stdio::piped(),
        });
        if cmd.capture {
            command.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        let mut child = command.spawn().map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => RunError::NotFound,
            _ => RunError::Os(crate::error::os_message(&err, None)),
        })?;
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
        let status = wait(&mut child, deadline)
            .map_err(|e| RunError::Os(e.to_string()))?
            .ok_or(RunError::TimedOut)?;
        let stdout = collect(stdout, deadline).ok_or(RunError::TimedOut)?;
        let stderr = collect(stderr, deadline).ok_or(RunError::TimedOut)?;
        let code = status
            .code()
            .unwrap_or_else(|| -status.signal().unwrap_or(1));
        Ok(Output {
            code,
            stdout,
            stderr,
        })
    }

    fn exec(&self, argv: &[String]) -> String {
        use std::os::unix::process::CommandExt;

        let err = Command::new(&argv[0]).args(&argv[1..]).exec();
        crate::error::os_message(&err, None)
    }
}

/// Run a CLI and return its stripped stdout. It never reads the terminal,
/// which the picker owns. Failures keep Python's messages, redacted.
pub fn run<S: AsRef<str>>(ctx: &Ctx, argv: &[S], timeout: Option<f64>) -> Result<String> {
    let cmd = Cmd::new(argv).timeout(timeout);
    let program = cmd.program().to_string();
    match ctx.runner.run(&cmd) {
        Err(RunError::TimedOut) => Err(Error::msg(format!("Timed out running {program}"))),
        Err(RunError::NotFound) => {
            Err(Error::msg(format!("Required command not found: {program}")))
        }
        Err(RunError::Os(text)) => Err(Error::msg(text)),
        Ok(output) if output.code != 0 => {
            let detail = if output.stderr.is_empty() {
                &output.stdout
            } else {
                &output.stderr
            };
            let detail = redact_text(detail.trim());
            Err(Error::msg(if detail.is_empty() {
                format!("Command failed: {program}")
            } else {
                detail
            }))
        }
        Ok(output) => Ok(output.stdout.trim().to_string()),
    }
}
