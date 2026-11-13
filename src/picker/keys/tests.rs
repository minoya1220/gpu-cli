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
            Enter, Ignored, Ignored, Backspace, Backspace, CtrlC, CtrlD, CtrlR, Ignored, Ignored,
            Ignored
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
