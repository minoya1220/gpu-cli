//! The picker's event loop and terminal renderer.
//!
//! Like prompt_toolkit's non-fullscreen mode it draws in place below the
//! cursor, grows to at most the terminal height, wraps long body lines, keeps
//! the selection visible with one line of scroll margin, and erases itself
//! when done. Input comes from fd 0, multiplexed with job completions and
//! SIGWINCH through `poll(2)` on stdin plus a wake pipe, so no reader thread
//! outlives the picker and a following prompt gets every keystroke.

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
    let done = *abort.script_done.lock().unwrap_or_else(|p| p.into_inner());
    done.is_some_and(|at| now >= at + Duration::from_secs(2))
}

pub fn run_loop<V: PickValue, R: 'static>(
    first: Screen<V, R>,
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
    let core = Core::new(first);
    core.borrow_mut().open(false);
    let mut decoder = Decoder::new();
    let mut renderer = Renderer::default();
    let mut input_open = true;
    let end = loop {
        runtime.poll();
        {
            let core = core.borrow();
            renderer.draw(io, &core.frame());
            if let Some(snapshot) = &options.snapshot {
                *snapshot.lock().unwrap_or_else(|p| p.into_inner()) = core.snapshot();
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
            .map(|l| truncate(&shown(l), width))
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
    if ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = rustix::io::write(rustix::stdio::stdout(), LEAVE);
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
pub fn run_terminal<V: PickValue, R: 'static>(first: Screen<V, R>) -> Result<Outcome<R>> {
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
    let end = run_loop(first, &mut io, options);
    restore_terminal();
    match end {
        LoopEnd::Finished(outcome) => Ok(outcome),
        LoopEnd::Aborted => Ok(Outcome::Cancelled(1)),
    }
}

struct ScriptIo {
    input: OwnedFd,
}

impl Io for ScriptIo {
    fn input(&self) -> BorrowedFd<'_> {
        self.input.as_fd()
    }

    fn write(&mut self, _bytes: &[u8]) {}

    fn size(&self) -> (usize, usize) {
        (100, 30)
    }
}

/// Run on scripted key bytes; when they run out, the picker quits like Ctrl-D.
pub fn run_scripted<V: PickValue, R: 'static>(first: Screen<V, R>, keys: &[u8]) -> Outcome<R> {
    let (read, write) = rustix::pipe::pipe().expect("create key pipe");
    let mut written = 0;
    while written < keys.len() {
        match rustix::io::write(&write, &keys[written..]) {
            Ok(n) => written += n,
            Err(_) => break,
        }
    }
    drop(write);
    let mut io = ScriptIo { input: read };
    let options = LoopOptions {
        eof_cancels: true,
        snapshot: None,
        abort: None,
        sigwinch: false,
    };
    match run_loop(first, &mut io, options) {
        LoopEnd::Finished(outcome) => outcome,
        LoopEnd::Aborted => Outcome::Cancelled(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::Line;

    struct Screen {
        size: (usize, usize),
        parser: vt100::Parser,
    }

    impl Screen {
        fn new(columns: u16, rows: u16) -> Screen {
            let nz = |n: u16| std::num::NonZeroU16::new(n).unwrap();
            Screen {
                size: (columns as usize, rows as usize),
                parser: vt100::Parser::new(nz(rows), nz(columns), 0),
            }
        }

        fn rows(&self) -> Vec<String> {
            self.parser
                .screen()
                .rows(0, self.size.0 as u16)
                .map(|r| r.trim_end().to_string())
                .collect()
        }
    }

    impl Io for Screen {
        fn input(&self) -> BorrowedFd<'_> {
            rustix::stdio::stdin()
        }

        fn write(&mut self, bytes: &[u8]) {
            self.parser.process(bytes);
        }

        fn size(&self) -> (usize, usize) {
            self.size
        }
    }

    fn plain(text: &str) -> Line {
        vec![(Style::Plain, text.to_string())]
    }

    fn frame(count: usize, cursor: usize) -> Frame {
        let body = (0..count)
            .map(|i| {
                if i == cursor {
                    vec![(Style::Selected, format!("> item {i}"))]
                } else {
                    plain(&format!("  item {i}"))
                }
            })
            .collect();
        Frame {
            header: vec![plain("Title")],
            body,
            cursor_line: Some(cursor),
            footer: "hints".into(),
        }
    }

    #[test]
    fn draws_in_place_and_erases_when_done() {
        let mut screen = Screen::new(40, 10);
        screen.write(b"before\r\n");
        let mut renderer = Renderer::default();
        renderer.draw(&mut screen, &frame(3, 0));
        assert_eq!(
            screen.rows()[..6],
            ["before", "Title", "> item 0", "  item 1", "  item 2", "hints"]
        );
        renderer.draw(&mut screen, &frame(2, 1));
        assert_eq!(
            screen.rows()[..6],
            ["before", "Title", "  item 0", "> item 1", "hints", ""]
        );
        renderer.erase(&mut screen);
        assert_eq!(screen.rows()[..2], ["before", ""]);
    }

    #[test]
    fn long_lines_wrap_in_the_body_and_cut_in_header_and_footer() {
        let mut screen = Screen::new(10, 10);
        let mut renderer = Renderer::default();
        let frame = Frame {
            header: vec![plain("A very long title")],
            body: vec![plain("0123456789abcde")],
            cursor_line: None,
            footer: "footer hints that are long".into(),
        };
        renderer.draw(&mut screen, &frame);
        assert_eq!(
            screen.rows()[..4],
            ["A very lon", "0123456789", "abcde", "footer hin"]
        );
    }

    #[test]
    fn control_characters_show_as_prompt_toolkit_draws_them() {
        let mut screen = Screen::new(40, 10);
        let mut renderer = Renderer::default();
        let frame = Frame {
            header: vec![plain("T\x1b[2Jitle")],
            body: vec![plain("a\tb\x7fc\u{85}d\x00")],
            cursor_line: None,
            footer: "x\ry".into(),
        };
        renderer.draw(&mut screen, &frame);
        assert_eq!(screen.rows()[..3], ["T^[[2Jitle", "a^Ib^?c<85>d^@", "x^My"]);
        let mut narrow = Screen::new(4, 10);
        let mut renderer = Renderer::default();
        let frame = Frame {
            header: vec![],
            body: vec![plain("\t\t\t")],
            cursor_line: None,
            footer: String::new(),
        };
        renderer.draw(&mut narrow, &frame);
        assert_eq!(narrow.rows()[..3], ["^I^I", "^I", ""]);
        renderer.erase(&mut narrow);
        assert_eq!(narrow.rows()[..2], ["", ""]);
    }

    #[test]
    fn the_selection_stays_visible_with_a_line_of_margin() {
        let mut screen = Screen::new(40, 8);
        let mut renderer = Renderer::default();
        // Eight rows: title, six body rows, footer.
        renderer.draw(&mut screen, &frame(50, 0));
        assert_eq!(screen.rows()[1], "> item 0");
        renderer.draw(&mut screen, &frame(50, 10));
        let rows = screen.rows();
        assert_eq!(rows[0], "Title");
        assert_eq!(rows[5], "> item 10");
        assert_eq!(rows[6], "  item 11");
        assert_eq!(rows[7], "hints");
        renderer.draw(&mut screen, &frame(50, 49));
        assert_eq!(screen.rows()[6], "> item 49");
        renderer.draw(&mut screen, &frame(50, 40));
        let rows = screen.rows();
        assert_eq!(rows[1], "  item 39");
        assert_eq!(rows[2], "> item 40");
    }
}
