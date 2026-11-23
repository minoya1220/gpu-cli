//! The picker's event loop and terminal renderer.
//!
//! Like prompt_toolkit's non-fullscreen mode it draws in place below the
//! cursor, grows to at most the terminal height, wraps long body lines, keeps
//! the selection visible with one line of scroll margin, and erases itself
//! when done. Input comes from fd 0, multiplexed with job completions and
//! SIGWINCH through `poll(2)` on stdin plus a wake pipe, so no reader thread
//! outlives the picker and a following prompt gets every keystroke.

use crate::sync::MutexExt;
use std::collections::VecDeque;
use std::io::Write;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::style::{Attribute, Color, SetAttribute, SetForegroundColor};
use crossterm::{cursor, queue, terminal};
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use unicode_width::UnicodeWidthChar;

use super::job::{Runtime, Waker};
use super::keys::Decoder;
use super::{Core, Frame, Outcome, PickValue, Screen, Snapshot, Style};
use crate::error::{os_message, Error, Result};

/// Where the loop reads keys and draws.
pub trait Io {
    fn input(&self) -> BorrowedFd<'_>;
    fn write(&mut self, bytes: &[u8]);
    /// Columns and rows.
    fn size(&self) -> (usize, usize);
    /// Scripted keys and the record of what the picker showed.
    fn script(&mut self) -> Option<&mut Script> {
        None
    }
}

/// Lets a test end a loop that would otherwise wait forever.
#[derive(Default)]
pub struct Abort {
    pub script_done: Mutex<Option<Instant>>,
    pub script_failed: AtomicBool,
}

pub struct LoopOptions {
    /// End of input quits like Ctrl-D (a real terminal or scripted keys).
    pub eof_cancels: bool,
    pub snapshot: Option<Arc<Mutex<Snapshot>>>,
    pub abort: Option<Arc<Abort>>,
    /// Redraw when the terminal is resized.
    pub sigwinch: bool,
}

pub enum LoopEnd<R> {
    Finished(Outcome<R>),
    Aborted,
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let (read, write) = rustix::pipe::pipe()?;
    for fd in [&read, &write] {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
        rustix::fs::fcntl_setfl(fd, rustix::fs::OFlags::NONBLOCK)?;
    }
    Ok((read, write))
}

fn drain(fd: &OwnedFd) {
    let mut buffer = [0u8; 256];
    while matches!(rustix::io::read(fd, &mut buffer), Ok(n) if n > 0) {}
}

/// Abort a test loop 2 s after its script ends, or at once if it failed.
fn aborted(abort: &Option<Arc<Abort>>, now: Instant) -> bool {
    let Some(abort) = abort else { return false };
    if abort.script_failed.load(Ordering::SeqCst) {
        return true;
    }
    let done = *abort.script_done.lock_recover();
    done.is_some_and(|at| now >= at + Duration::from_secs(2))
}

pub fn run_loop<V: PickValue, R: 'static>(
    path: Vec<Screen<V, R>>,
    io: &mut dyn Io,
    options: LoopOptions,
) -> LoopEnd<R> {
    let (wake_read, wake_write) = pipe().expect("create picker wake pipe");
    let winch = if options.sigwinch {
        rustix::io::dup(&wake_write).ok().and_then(|fd| {
            signal_hook::low_level::pipe::register(signal_hook::consts::SIGWINCH, fd).ok()
        })
    } else {
        None
    };
    let runtime = Runtime::new(Arc::new(Waker::new(wake_write)));
    let _entered = runtime.enter();
    let core = Core::new(path);
    let mut decoder = Decoder::new();
    let mut renderer = Renderer::default();
    let mut input_open = true;
    let end = loop {
        runtime.poll();
        {
            let core = core.borrow();
            renderer.draw(io, &core.frame());
            if let Some(snapshot) = &options.snapshot {
                *snapshot.lock_recover() = core.snapshot();
            }
        }
        if let Some(script) = io.script() {
            let core = core.borrow();
            script.shown(core.snapshot());
            script.check_stalled();
            if core.idle() && decoder.deadline().is_none() {
                script.release();
            }
        }
        if let Some(outcome) = core.borrow_mut().take_outcome() {
            break LoopEnd::Finished(outcome);
        }
        let now = Instant::now();
        if aborted(&options.abort, now) {
            break LoopEnd::Aborted;
        }
        if !input_open && options.eof_cancels {
            for key in decoder.flush(now + Duration::from_secs(3600)) {
                core.borrow_mut().handle_key(key);
            }
            if !core.borrow().finished() {
                core.borrow_mut().handle_key(super::keys::Key::CtrlD);
            }
            continue;
        }
        let mut deadline = decoder.deadline();
        if io.script().is_some() {
            // Wake to notice a stalled script instead of waiting forever.
            let tick = now + Duration::from_millis(250);
            deadline = Some(deadline.map_or(tick, |d| d.min(tick)));
        }
        if options.abort.is_some() {
            let tick = now + Duration::from_millis(20);
            deadline = Some(deadline.map_or(tick, |d| d.min(tick)));
        }
        let timeout = deadline.map(|d| {
            let wait = d.saturating_duration_since(now);
            Timespec {
                tv_sec: wait.as_secs() as _,
                tv_nsec: wait.subsec_nanos() as _,
            }
        });
        let input = io.input();
        let mut fds = vec![PollFd::new(&wake_read, PollFlags::IN)];
        if input_open {
            fds.push(PollFd::new(&input, PollFlags::IN));
        }
        let ready = poll(&mut fds, timeout.as_ref()).unwrap_or(0);
        let wake = ready > 0 && !fds[0].revents().is_empty();
        let readable = input_open && ready > 0 && !fds[1].revents().is_empty();
        drop(fds);
        if wake {
            drain(&wake_read);
        }
        let mut keys = Vec::new();
        if readable {
            let mut buffer = [0u8; 4096];
            match rustix::io::read(input, &mut buffer) {
                Ok(0) => input_open = false,
                Ok(n) => keys.extend(decoder.feed(&buffer[..n], Instant::now())),
                Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
                Err(_) => input_open = false,
            }
        }
        keys.extend(decoder.flush(Instant::now()));
        if !keys.is_empty() {
            if let Some(script) = io.script() {
                script.handled();
            }
        }
        for key in keys {
            core.borrow_mut().handle_key(key);
            if core.borrow().finished() {
                break;
            }
        }
    };
    if let Some(id) = winch {
        signal_hook::low_level::unregister(id);
    }
    renderer.erase(io);
    end
}

#[derive(Default)]
struct Renderer {
    rows_drawn: usize,
    scroll: usize,
    last: Vec<u8>,
}

/// prompt_toolkit's display form for control characters: `^@`..`^_`, `^?`
/// and `<80>`..`<9f>`, so nothing reaches the terminal as a control code.
fn shown(line: &[(Style, String)]) -> Vec<(Style, String)> {
    line.iter()
        .map(|(style, text)| {
            let mut out = String::with_capacity(text.len());
            for c in text.chars() {
                match c {
                    '\0'..='\x1f' => {
                        out.push('^');
                        out.push(char::from(c as u8 + 0x40));
                    }
                    '\x7f' => out.push_str("^?"),
                    '\u{80}'..='\u{9f}' => out.push_str(&format!("<{:02x}>", c as u32)),
                    c => out.push(c),
                }
            }
            (*style, out)
        })
        .collect()
}

/// Wrap one styled line to `width` display columns.
fn wrap(line: &[(Style, String)], width: usize) -> Vec<Vec<(Style, String)>> {
    let mut rows = vec![Vec::new()];
    let mut used = 0;
    for (style, text) in line {
        let mut piece = String::new();
        for c in text.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > width && used > 0 {
                rows.last_mut()
                    .unwrap()
                    .push((*style, std::mem::take(&mut piece)));
                rows.push(Vec::new());
                used = 0;
            }
            piece.push(c);
            used += w;
        }
        rows.last_mut().unwrap().push((*style, piece));
    }
    rows
}

/// Cut one styled line at `width` display columns.
fn truncate(line: &[(Style, String)], width: usize) -> Vec<(Style, String)> {
    let mut out = Vec::new();
    let mut used = 0;
    for (style, text) in line {
        let mut piece = String::new();
        for c in text.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > width {
                out.push((*style, piece));
                return out;
            }
            piece.push(c);
            used += w;
        }
        out.push((*style, piece));
    }
    out
}

fn styled(out: &mut Vec<u8>, row: &[(Style, String)]) {
    for (style, text) in row {
        if text.is_empty() {
            continue;
        }
        let _ = match style {
            Style::Plain => Ok(()),
            Style::Title => queue!(out, SetAttribute(Attribute::Bold)),
            Style::Filter => queue!(out, SetForegroundColor(Color::DarkCyan)),
            Style::Selected => queue!(
                out,
                SetAttribute(Attribute::Bold),
                SetAttribute(Attribute::Reverse)
            ),
            Style::Note => queue!(out, SetAttribute(Attribute::Italic)),
            Style::Error => queue!(out, SetForegroundColor(Color::DarkRed)),
        };
        out.extend_from_slice(text.as_bytes());
        if *style != Style::Plain {
            let _ = queue!(out, SetAttribute(Attribute::Reset));
        }
    }
}

/// The first body line to show, as prompt_toolkit's wrapping `Window`
/// scrolls: by whole lines, keeping the cursor line fully visible with one
/// row of margin above and below, never scrolling past the end, and moving
/// no further than needed from `previous`.
fn scroll_position(heights: &[usize], cursor: usize, height: usize, previous: usize) -> usize {
    let (top_margin, bottom_margin) = (1, 1);
    if heights.iter().sum::<usize>() <= height || cursor >= heights.len() {
        return 0;
    }
    if heights[cursor] > height.saturating_sub(top_margin) {
        return cursor;
    }
    let mut min_scroll = 0;
    let (mut used, mut prev) = (0, cursor);
    for line in (0..=cursor).rev() {
        used += heights[line];
        if used > height - bottom_margin {
            min_scroll = prev;
            break;
        }
        prev = line;
    }
    let (mut used, mut max_scroll) = (0, cursor);
    for line in (0..cursor).rev() {
        used += heights[line];
        if used > top_margin {
            break;
        }
        max_scroll = line;
    }
    let (mut used, mut topmost) = (0, heights.len() - 1);
    for line in (0..heights.len()).rev() {
        used += heights[line];
        if used > height {
            break;
        }
        topmost = line;
    }
    previous
        .max(topmost.min(min_scroll))
        .min(max_scroll)
        .min(topmost)
}

impl Renderer {
    fn move_to_top(&self, out: &mut Vec<u8>) {
        out.push(b'\r');
        if self.rows_drawn > 1 {
            let _ = queue!(out, cursor::MoveUp((self.rows_drawn - 1) as u16));
        }
    }

    fn draw(&mut self, io: &mut dyn Io, frame: &Frame) {
        let (width, height) = io.size();
        let (width, height) = (width.max(1), height.max(1));
        let header: Vec<_> = frame
            .header
            .iter()
            .enumerate()
            .flat_map(|(index, line)| {
                if index == 0 && frame.wrap_title {
                    wrap(&shown(line), width)
                } else {
                    vec![truncate(&shown(line), width)]
                }
            })
            .collect();
        let wrapped: Vec<Vec<Vec<(Style, String)>>> = frame
            .body
            .iter()
            .map(|line| wrap(&shown(line), width))
            .collect();
        let heights: Vec<usize> = wrapped.iter().map(Vec::len).collect();
        let body_room = height.saturating_sub(header.len() + 1).max(1);
        self.scroll = scroll_position(
            &heights,
            frame.cursor_line.unwrap_or(0),
            body_room,
            self.scroll,
        );
        let body: Vec<_> = wrapped
            .into_iter()
            .skip(self.scroll)
            .flatten()
            .take(body_room)
            .collect();
        let footer = truncate(&shown(&[(Style::Plain, frame.footer.clone())]), width);
        let mut rows: Vec<Vec<(Style, String)>> = header;
        rows.extend(body);
        rows.push(footer);
        rows.truncate(height);

        let mut content = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                content.extend_from_slice(b"\r\n");
            }
            styled(&mut content, row);
        }
        if content == self.last {
            return;
        }
        let mut out = Vec::new();
        if self.rows_drawn > 0 {
            self.move_to_top(&mut out);
        }
        let _ = queue!(out, terminal::Clear(terminal::ClearType::FromCursorDown));
        out.extend_from_slice(&content);
        io.write(&out);
        self.rows_drawn = rows.len();
        self.last = content;
    }

    fn erase(&mut self, io: &mut dyn Io) {
        if self.rows_drawn == 0 {
            return;
        }
        let mut out = Vec::new();
        self.move_to_top(&mut out);
        let _ = queue!(out, terminal::Clear(terminal::ClearType::FromCursorDown));
        io.write(&out);
        self.rows_drawn = 0;
        self.last.clear();
    }
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
const ENTER: &[u8] = b"\x1b[?25l\x1b[?2004h";
const LEAVE: &[u8] = b"\x1b[?2004l\x1b[?25h";

/// Undo raw mode, bracketed paste and the hidden cursor; safe to call from
/// any thread, any number of times (panic hook, SIGINT handler, normal exit).
pub fn restore_terminal() {
    if !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    // Serialize with TerminalIo::write: finish any in-flight draw, then prevent
    // subsequent draws before restoring the terminal (not after Cancelled).
    let mut stdout = std::io::stdout().lock();
    if ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = stdout.write_all(LEAVE);
        let _ = stdout.flush();
        let _ = terminal::disable_raw_mode();
    }
}

struct TerminalIo;

impl Io for TerminalIo {
    fn input(&self) -> BorrowedFd<'_> {
        rustix::stdio::stdin()
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut stdout = std::io::stdout().lock();
        if !ACTIVE.load(Ordering::SeqCst) {
            return;
        }
        let _ = stdout.write_all(bytes);
        let _ = stdout.flush();
    }

    fn size(&self) -> (usize, usize) {
        // stdout's size, as prompt_toolkit reads it.
        match rustix::termios::tcgetwinsize(rustix::stdio::stdout()) {
            Ok(size) if size.ws_col > 0 && size.ws_row > 0 => {
                (size.ws_col as usize, size.ws_row as usize)
            }
            _ => terminal::size()
                .map(|(c, r)| (c as usize, r as usize))
                .unwrap_or((80, 24)),
        }
    }
}

/// Run on the real terminal (stdin and stdout are TTYs).
pub fn run_terminal<V: PickValue, R: 'static>(path: Vec<Screen<V, R>>) -> Result<Outcome<R>> {
    terminal::enable_raw_mode().map_err(|e| Error::msg(os_message(&e, None)))?;
    ACTIVE.store(true, Ordering::SeqCst);
    let mut io = TerminalIo;
    io.write(ENTER);
    let options = LoopOptions {
        eof_cancels: true,
        snapshot: None,
        abort: None,
        sigwinch: true,
    };
    let end = run_loop(path, &mut io, options);
    restore_terminal();
    match end {
        LoopEnd::Finished(outcome) => Ok(outcome),
        LoopEnd::Aborted => Ok(Outcome::Cancelled(1)),
    }
}

/// Key bytes split at NUL: each chunk after the first is written once the
/// picker has handled the previous one and its screen is no longer loading.
pub struct Script {
    keys: Option<OwnedFd>,
    chunks: VecDeque<Vec<u8>>,
    shown: Vec<Snapshot>,
    /// A chunk was written and the picker has not read it yet.
    unread: bool,
    /// When the picker last changed what it showed or received keys.
    progress: Instant,
}

/// A scripted picker that neither redraws nor gets keys for this long is stuck.
#[cfg(not(test))]
const STALLED: Duration = Duration::from_secs(20);
#[cfg(test)]
const STALLED: Duration = Duration::from_secs(5);

impl Script {
    fn release(&mut self) {
        let Some(keys) = &self.keys else {
            return;
        };
        if self.unread {
            return;
        }
        self.progress = Instant::now();
        if let Some(chunk) = self.chunks.pop_front() {
            self.unread = !chunk.is_empty();
            let mut written = 0;
            while written < chunk.len() {
                match rustix::io::write(keys, &chunk[written..]) {
                    Ok(n) => written += n,
                    Err(_) => break,
                }
            }
        }
        if self.chunks.is_empty() {
            self.keys = None;
        }
    }

    /// The picker read and handled the chunk it was waiting on.
    fn handled(&mut self) {
        self.unread = false;
    }

    fn shown(&mut self, snapshot: Snapshot) {
        if self.shown.last() != Some(&snapshot) {
            self.shown.push(snapshot);
            self.progress = Instant::now();
        }
    }

    /// Fail a stuck script with what it was waiting on, not a CI timeout.
    fn check_stalled(&self) {
        if self.progress.elapsed() > STALLED {
            let last = self.shown.last().cloned().unwrap_or_default();
            panic!(
                "scripted picker stalled for {STALLED:?} on screen {:?} with {} key chunk(s) \
                 unsent and its load unfinished; labels: {:?}",
                last.title,
                self.chunks.len(),
                last.labels
            );
        }
    }
}

struct ScriptIo {
    input: OwnedFd,
    script: Script,
}

impl Io for ScriptIo {
    fn input(&self) -> BorrowedFd<'_> {
        self.input.as_fd()
    }

    fn write(&mut self, _bytes: &[u8]) {}

    fn size(&self) -> (usize, usize) {
        (100, 30)
    }

    fn script(&mut self) -> Option<&mut Script> {
        Some(&mut self.script)
    }
}

/// Run on scripted key bytes; when they run out, the picker quits like
/// Ctrl-D. The outcome, and each distinct state the picker showed.
pub fn run_scripted<V: PickValue, R: 'static>(
    path: Vec<Screen<V, R>>,
    keys: &[u8],
) -> (Outcome<R>, Vec<Snapshot>) {
    let (read, write) = rustix::pipe::pipe().expect("create key pipe");
    let mut script = Script {
        keys: Some(write),
        chunks: keys.split(|b| *b == 0).map(<[u8]>::to_vec).collect(),
        shown: Vec::new(),
        unread: false,
        progress: Instant::now(),
    };
    script.release();
    let mut io = ScriptIo {
        input: read,
        script,
    };
    let options = LoopOptions {
        eof_cancels: true,
        snapshot: None,
        abort: None,
        sigwinch: false,
    };
    let outcome = match run_loop(path, &mut io, options) {
        LoopEnd::Finished(outcome) => outcome,
        LoopEnd::Aborted => Outcome::Cancelled(1),
    };
    (outcome, io.script.shown)
}

#[cfg(test)]
mod tests;
