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
        wrap_title: false,
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
        wrap_title: false,
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
fn paid_prompt_titles_wrap_without_hiding_the_deadline_or_reply() {
    let mut screen = Screen::new(20, 10);
    let mut renderer = Renderer::default();
    let frame = Frame {
        header: vec![
            plain("Launch GPU for $1.00/hr, delete at 18:30?"),
            plain("Type yes:"),
        ],
        wrap_title: true,
        body: Vec::new(),
        cursor_line: None,
        footer: "← back".into(),
    };
    renderer.draw(&mut screen, &frame);
    assert_eq!(
        screen.rows()[..5],
        [
            "Launch GPU for $1.00",
            "/hr, delete at 18:30",
            "?",
            "Type yes:",
            "← back"
        ]
    );
}

#[test]
fn control_characters_show_as_prompt_toolkit_draws_them() {
    let mut screen = Screen::new(40, 10);
    let mut renderer = Renderer::default();
    let frame = Frame {
        header: vec![plain("T\x1b[2Jitle")],
        wrap_title: false,
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
        wrap_title: false,
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

#[test]
fn restore_terminal_without_active_picker_does_not_wait_for_stdout() {
    assert!(!ACTIVE.load(Ordering::SeqCst));
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _stdout = std::io::stdout().lock();
        locked_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(std::time::Duration::from_secs(10));
    });
    locked_rx.recv().unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        restore_terminal();
        let _ = done_tx.send(());
    });
    let restored = done_rx.recv_timeout(std::time::Duration::from_secs(2));
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    assert!(
        restored.is_ok(),
        "restore_terminal blocked on stdout with no active picker"
    );
}

#[test]
#[should_panic(expected = "scripted picker stalled")]
fn a_scripted_picker_that_never_finishes_loading_fails_instead_of_hanging() {
    let job: crate::picker::Job<Vec<String>> = crate::picker::Job::new(|| {
        std::thread::sleep(Duration::from_secs(60));
        Ok(Vec::new())
    });
    let first = crate::picker::Screen::<String, String>::new("Stuck", Vec::new, |_| {
        crate::picker::Next::Done(String::new())
    })
    .load(job)
    .loading("Loading...");
    run_scripted(vec![first], b"\0\r");
}

#[test]
fn a_job_still_running_when_its_picker_ends_runs_again_in_the_next_picker() {
    use crate::picker::{Job, Next, Opt, Outcome, Screen};
    use std::sync::atomic::{AtomicBool, Ordering};
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let job: Job<String> = Job::new(move || {
        while !gate.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok("ready".into())
    });
    let screen = |job: &Job<String>| {
        let rows = job.clone();
        Screen::<String, String>::new(
            "Prices",
            move || {
                rows.result()
                    .map(|value| vec![Opt::new(value.clone(), value)])
                    .unwrap_or_default()
            },
            Next::Done,
        )
    };
    let first = screen(&job).background(job.clone());
    let (outcome, _) = run_scripted(vec![first], b"\x04");
    assert_eq!(outcome, Outcome::Cancelled(1));
    release.store(true, Ordering::SeqCst);
    let second = screen(&job).load(job.clone());
    let (outcome, _) = run_scripted(vec![second], b"\0\r");
    assert_eq!(outcome, Outcome::Done("ready".into()));
}
