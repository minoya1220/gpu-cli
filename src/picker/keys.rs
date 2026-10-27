//! Terminal input bytes to keys.
//!
//! Like prompt_toolkit, a lone ESC is held for half a second (its
//! `ttimeoutlen`) before it counts as Escape, so an arrow key split across
//! reads still moves. Complete escape sequences that are not bound to
//! anything are consumed whole and reported as `Ignored`, so they never
//! reach the filter.

use std::time::{Duration, Instant};

pub const ESCAPE_TIMEOUT: Duration = Duration::from_millis(500);
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Backspace,
    CtrlC,
    CtrlD,
    CtrlR,
    F2,
    Escape,
    /// Text: one character, which may still be unprintable.
    Char(char),
    /// Bracketed paste contents.
    Paste(String),
    /// Any other key (Home, Tab, F5, Shift-Left, Ctrl-A, ...).
    Ignored,
}

#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    paste: Option<Vec<u8>>,
    last_input: Option<Instant>,
}

enum Step {
    Key(Key, usize),
    Incomplete,
}

fn csi(buf: &[u8]) -> Step {
    // buf starts with ESC '['.
    if buf.get(2) == Some(&b'[') {
        // Linux console F1-F5: ESC [ [ A..E.
        return match buf.get(3) {
            None => Step::Incomplete,
            Some(b'B') => Step::Key(Key::F2, 4),
            Some(_) => Step::Key(Key::Ignored, 4),
        };
    }
    let mut i = 2;
    while let Some(&b) = buf.get(i) {
        match b {
            0x20..=0x3f => i += 1,
            0x40..=0x7e => {
                let params = &buf[2..i];
                let key = match (params, b) {
                    (b"", b'A') => Key::Up,
                    (b"", b'B') => Key::Down,
                    (b"", b'C') => Key::Right,
                    (b"", b'D') => Key::Left,
                    (b"12", b'~') => Key::F2,
                    _ => Key::Ignored,
                };
                return Step::Key(key, i + 1);
            }
            // Not a CSI sequence after all: ESC, then the rest as typed.
            _ => return Step::Key(Key::Escape, 1),
        }
    }
    Step::Incomplete
}

fn step(buf: &[u8]) -> Step {
    let first = buf[0];
    match first {
        0x1b => match buf.get(1) {
            None => Step::Incomplete,
            Some(b'[') => csi(buf),
            Some(b'O') => match buf.get(2) {
                None => Step::Incomplete,
                Some(b'A') => Step::Key(Key::Up, 3),
                Some(b'B') => Step::Key(Key::Down, 3),
                Some(b'C') => Step::Key(Key::Right, 3),
                Some(b'D') => Step::Key(Key::Left, 3),
                Some(b'Q') => Step::Key(Key::F2, 3),
                Some(_) => Step::Key(Key::Ignored, 3),
            },
            Some(_) => Step::Key(Key::Escape, 1),
        },
        b'\r' => Step::Key(Key::Enter, 1),
        0x7f | 0x08 => Step::Key(Key::Backspace, 1),
        0x03 => Step::Key(Key::CtrlC, 1),
        0x04 => Step::Key(Key::CtrlD, 1),
        0x12 => Step::Key(Key::CtrlR, 1),
        0x00..=0x1f => Step::Key(Key::Ignored, 1),
        _ => {
            let width = match first {
                0x00..=0x7f => 1,
                0xc0..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf7 => 4,
                _ => return Step::Key(Key::Ignored, 1),
            };
            if buf.len() < width {
                return Step::Incomplete;
            }
            match std::str::from_utf8(&buf[..width])
                .ok()
                .and_then(|s| s.chars().next())
            {
                Some(c) => Step::Key(Key::Char(c), width),
                None => Step::Key(Key::Ignored, 1),
            }
        }
    }
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    /// Keys completed by these bytes.
    pub fn feed(&mut self, bytes: &[u8], now: Instant) -> Vec<Key> {
        self.buffer.extend_from_slice(bytes);
        self.last_input = Some(now);
        self.drain(false)
    }

    /// When a held partial sequence should be flushed, if any.
    pub fn deadline(&self) -> Option<Instant> {
        if self.buffer.is_empty() || self.paste.is_some() {
            return None;
        }
        self.last_input.map(|t| t + ESCAPE_TIMEOUT)
    }

    /// After the timeout, a held ESC is Escape and the rest is typed text.
    pub fn flush(&mut self, now: Instant) -> Vec<Key> {
        match self.deadline() {
            Some(deadline) if now >= deadline => self.drain(true),
            _ => Vec::new(),
        }
    }

    fn drain(&mut self, flushing: bool) -> Vec<Key> {
        let mut keys = Vec::new();
        loop {
            if let Some(paste) = &mut self.paste {
                paste.append(&mut self.buffer);
                match paste.windows(PASTE_END.len()).position(|w| w == PASTE_END) {
                    Some(end) => {
                        let rest = paste.split_off(end + PASTE_END.len());
                        paste.truncate(end);
                        let text = String::from_utf8_lossy(paste).into_owned();
                        self.paste = None;
                        self.buffer = rest;
                        keys.push(Key::Paste(text));
                        continue;
                    }
                    None => return keys,
                }
            }
            if self.buffer.is_empty() {
                return keys;
            }
            if self.buffer.starts_with(PASTE_START) {
                self.buffer.drain(..PASTE_START.len());
                self.paste = Some(Vec::new());
                continue;
            }
            match step(&self.buffer) {
                Step::Key(key, used) => {
                    self.buffer.drain(..used);
                    keys.push(key);
                }
                Step::Incomplete if flushing => {
                    let key = if self.buffer[0] == 0x1b {
                        Key::Escape
                    } else {
                        Key::Ignored
                    };
                    self.buffer.remove(0);
                    keys.push(key);
                }
                Step::Incomplete => return keys,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(decoder: &mut Decoder, bytes: &str, at: Instant) -> Vec<Key> {
        decoder.feed(bytes.as_bytes(), at)
    }

    #[test]
    fn arrows_in_csi_and_ss3_forms() {
        let now = Instant::now();
        let mut decoder = Decoder::new();
        let keys = feed(
            &mut decoder,
            "\x1b[A\x1b[B\x1b[C\x1b[D\x1bOA\x1bOB\x1bOC\x1bOD",
            now,
        );
        use Key::*;
        assert_eq!(keys, [Up, Down, Right, Left, Up, Down, Right, Left]);
        assert_eq!(decoder.deadline(), None);
    }

    #[test]
    fn a_split_arrow_waits_for_the_rest() {
        let now = Instant::now();
        let mut decoder = Decoder::new();
        assert!(feed(&mut decoder, "\x1b", now).is_empty());
        assert!(feed(&mut decoder, "[", now + Duration::from_millis(20)).is_empty());
        assert!(decoder.flush(now + Duration::from_millis(100)).is_empty());
        assert_eq!(
            feed(&mut decoder, "B", now + Duration::from_millis(40)),
            [Key::Down]
        );
    }

    #[test]
    fn a_held_escape_flushes_after_the_timeout() {
        let now = Instant::now();
        let mut decoder = Decoder::new();
        assert!(feed(&mut decoder, "\x1b", now).is_empty());
        assert_eq!(decoder.deadline(), Some(now + ESCAPE_TIMEOUT));
        assert!(decoder.flush(now + Duration::from_millis(499)).is_empty());
        assert_eq!(decoder.flush(now + ESCAPE_TIMEOUT), [Key::Escape]);
        // A partial sequence flushes as Escape plus typed text, like prompt_toolkit.
        feed(&mut decoder, "\x1b[", now);
        assert_eq!(
            decoder.flush(now + ESCAPE_TIMEOUT),
            [Key::Escape, Key::Char('[')]
        );
    }

    #[test]
    fn escape_before_other_text() {
        let now = Instant::now();
        let mut decoder = Decoder::new();
        assert_eq!(feed(&mut decoder, "\x1b\x1b", now), [Key::Escape]);
        assert_eq!(feed(&mut decoder, "`", now), [Key::Escape, Key::Char('`')]);
        assert_eq!(
            feed(&mut decoder, "\x1ba", now),
            [Key::Escape, Key::Char('a')]
        );
    }

    #[test]
    fn f2_forms_and_ignored_special_keys() {
        let now = Instant::now();
        let mut decoder = Decoder::new();
        assert_eq!(
            feed(&mut decoder, "\x1b[12~\x1bOQ\x1b[[B", now),
            [Key::F2, Key::F2, Key::F2]
        );
        let special = "\x1b[H\x1b[F\x1b[1~\x1b[4~\x1b[5~\x1b[6~\x1b[2~\x1b[3~\x1bOP\x1bOR\x1bOS\x1b[15~\x1b[24~\x1b[1;2D\x1b[1;5C\x1b[Z\x1b[99~\x1b[[A";
        let keys = feed(&mut decoder, special, now);
        assert_eq!(keys.len(), 18);
        assert!(keys.iter().all(|k| *k == Key::Ignored), "{keys:?}");
    }

    #[test]
    fn control_bytes() {
        let now = Instant::now();
        let mut decoder = Decoder::new();
        use Key::*;
        let keys = feed(&mut decoder, "\r\n\t\x7f\x08\x03\x04\x12\x01\x0c\x00", now);
        assert_eq!(
            keys,
            [
                Enter, Ignored, Ignored, Backspace, Backspace, CtrlC, CtrlD, CtrlR, Ignored,
                Ignored, Ignored
            ]
        );
    }

    #[test]
    fn bracketed_paste_across_reads() {
        let now = Instant::now();
        let mut decoder = Decoder::new();
        assert!(feed(&mut decoder, "\x1b[200~https://host/", now).is_empty());
        assert_eq!(decoder.deadline(), None);
        assert!(decoder.flush(now + Duration::from_secs(5)).is_empty());
        assert_eq!(
            feed(&mut decoder, "a/b.git\x1b[201~x", now),
            [Key::Paste("https://host/a/b.git".into()), Key::Char('x')]
        );
    }

    #[test]
    fn utf8_split_across_reads() {
        let now = Instant::now();
        let mut decoder = Decoder::new();
        let bytes = "é".as_bytes();
        assert!(decoder.feed(&bytes[..1], now).is_empty());
        assert_eq!(decoder.feed(&bytes[1..], now), [Key::Char('é')]);
    }
}
