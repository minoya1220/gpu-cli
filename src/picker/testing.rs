//! Drives a real picker loop with key presses written to a pipe, one key per
//! write, while a script on another thread watches what it shows.

use crate::sync::MutexExt;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::term::{run_loop, Abort, Io, LoopEnd, LoopOptions};
use super::{Outcome, PickValue, Screen, Snapshot};

pub const ENTER: &str = "\r";
pub const BACK: &str = LEFT;
pub const UP: &str = "\x1b[A";
pub const DOWN: &str = "\x1b[B";
pub const RIGHT: &str = "\x1b[C";
pub const LEFT: &str = "\x1b[D";
pub const F2: &str = "\x1b[12~";
pub const ESC: &str = "\x1b";
pub const BACKSPACE: &str = "\x7f";
pub const CTRL_C: &str = "\x03";
pub const CTRL_D: &str = "\x04";

pub struct Keyboard {
    input: OwnedFd,
    snapshot: Arc<Mutex<Snapshot>>,
}

impl Keyboard {
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock_recover().clone()
    }

    pub fn title(&self) -> String {
        self.snapshot().title
    }

    pub fn filter(&self) -> String {
        self.snapshot().filter
    }

    pub fn labels(&self) -> Vec<String> {
        self.snapshot().labels
    }

    pub fn footer(&self) -> String {
        self.snapshot().footer
    }

    fn send(&self, key: &str) {
        rustix::io::write(&self.input, key.as_bytes()).expect("write key");
    }

    /// Send one key, then give the picker a moment.
    pub fn press(&self, key: &str) {
        self.send(key);
        std::thread::sleep(Duration::from_millis(20));
    }

    /// Send one key and wait for `until`; the latency.
    pub fn press_until(&self, key: &str, until: impl Fn(&Snapshot) -> bool) -> Duration {
        let start = Instant::now();
        self.send(key);
        self.wait(until);
        start.elapsed()
    }

    /// Send one key and wait for a condition outside the picker's own state.
    pub fn send_until(&self, key: &str, what: &str, condition: impl Fn() -> bool) {
        self.send(key);
        self.wait_for(what, condition);
    }

    pub fn type_text(&self, text: &str) {
        for c in text.chars() {
            let typed = format!("{}{c}", self.filter());
            self.press_until(&c.to_string(), move |s| s.filter == typed);
        }
    }

    pub fn wait(&self, condition: impl Fn(&Snapshot) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = self.snapshot();
            if condition(&snapshot) {
                return;
            }
            if Instant::now() > deadline {
                panic!("timed out on screen {:?}: {snapshot:#?}", snapshot.title);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Wait for any condition that is not about the picker's own state.
    pub fn wait_for(&self, what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !condition() {
            if Instant::now() > deadline {
                panic!("timed out waiting for {what} on screen {:?}", self.title());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

pub fn on(title: &str) -> impl Fn(&Snapshot) -> bool {
    let title = title.to_string();
    move |s| s.title == title
}

struct PipeIo {
    input: OwnedFd,
}

impl Io for PipeIo {
    fn input(&self) -> BorrowedFd<'_> {
        self.input.as_fd()
    }

    fn write(&mut self, _bytes: &[u8]) {}

    fn size(&self) -> (usize, usize) {
        (100, 30)
    }
}

/// Run a real picker loop on this thread while `script` presses keys on
/// another; the flow's outcome. A failing script fails the test.
pub fn drive<V: PickValue, R: 'static>(
    first: Screen<V, R>,
    script: impl FnOnce(&Keyboard) + Send + 'static,
) -> Outcome<R> {
    let (read, write) = rustix::pipe::pipe().expect("create key pipe");
    let snapshot = Arc::new(Mutex::new(Snapshot::default()));
    let abort = Arc::new(Abort::default());
    let keyboard = Keyboard {
        input: write,
        snapshot: snapshot.clone(),
    };
    let script_abort = abort.clone();
    let handle = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            keyboard.wait(|s| s.drawn);
            script(&keyboard)
        }));
        if result.is_err() {
            script_abort.script_failed.store(true, Ordering::SeqCst);
        }
        *script_abort.script_done.lock_recover() = Some(Instant::now());
        // Keep the pipe open until the picker finishes on its own.
        (result, keyboard)
    });
    let mut io = PipeIo { input: read };
    let options = LoopOptions {
        eof_cancels: false,
        snapshot: Some(snapshot),
        abort: Some(abort),
        sigwinch: false,
    };
    let end = run_loop(vec![first], &mut io, options);
    let (result, _keyboard) = handle.join().expect("script thread");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    match end {
        LoopEnd::Finished(outcome) => outcome,
        LoopEnd::Aborted => panic!("the picker did not finish within 2 s after the script ended"),
    }
}
