//! Port of tests/test_picker.py: the picker engine, driven with real key
//! presses through a pipe.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::picker::testing::*;
use crate::picker::{Job, JobState, Next, Opt, Outcome, Screen};

// Home, End, PageUp, PageDown, Insert, Delete, F-keys except F2, Shift-arrows, Ctrl-Left.
pub const SPECIAL_KEYS: [&str; 24] = [
    "\x1b[H",
    "\x1b[F",
    "\x1b[1~",
    "\x1b[4~",
    "\x1b[5~",
    "\x1b[6~",
    "\x1b[2~",
    "\x1b[3~",
    "\x1bOP",
    "\x1bOR",
    "\x1bOS",
    "\x1b[15~",
    "\x1b[17~",
    "\x1b[18~",
    "\x1b[19~",
    "\x1b[20~",
    "\x1b[21~",
    "\x1b[23~",
    "\x1b[24~",
    "\x1b[1;2D",
    "\x1b[1;2C",
    "\x1b[1;2A",
    "\x1b[1;2B",
    "\x1b[1;5D",
];

pub fn menu<R: 'static>(
    title: &str,
    values: &[&str],
    then: impl Fn(String) -> Next<String, R> + 'static,
) -> Screen<String, R> {
    let values: Vec<String> = values.iter().map(|v| v.to_string()).collect();
    Screen::new(
        title,
        move || {
            values
                .iter()
                .map(|v| Opt::new(v.clone(), v.clone()))
                .collect()
        },
        then,
    )
}

type Three = (String, String, String);

/// Three screens; the result records the value chosen on each.
fn chain() -> Screen<String, Three> {
    menu("First", &["a", "b"], |first| {
        Next::Screen(menu("Second", &["c", "d"], move |second| {
            let first = first.clone();
            Next::Screen(menu("Third", &["e", "f"], move |third| {
                Next::Done((first.clone(), second.clone(), third))
            }))
        }))
    })
}

fn three(a: &str, b: &str, c: &str) -> Outcome<Three> {
    Outcome::Done((a.into(), b.into(), c.into()))
}

/// Blocks a loader until released, like a slow gh or vastai call.
#[derive(Clone, Default)]
pub struct Gate(Arc<(Mutex<bool>, Condvar)>);

impl Gate {
    pub fn set(&self) {
        *self.0 .0.lock().unwrap() = true;
        self.0 .1.notify_all();
    }

    pub fn wait(&self) -> bool {
        let (lock, condvar) = &*self.0;
        let guard = lock.lock().unwrap();
        let (guard, _) = condvar
            .wait_timeout_while(guard, Duration::from_secs(5), |open| !*open)
            .unwrap();
        *guard
    }
}

fn gated<T: Clone + Send + Sync + 'static>(
    value: T,
    gate: &Gate,
    calls: &Arc<AtomicUsize>,
) -> Job<T> {
    let (gate, calls) = (gate.clone(), calls.clone());
    Job::new(move || {
        calls.fetch_add(1, Ordering::SeqCst);
        if !gate.wait() {
            return Err("test never released the loader".into());
        }
        Ok(value.clone())
    })
}

fn not_quit_pending(s: &crate::picker::Snapshot) -> bool {
    !s.footer.contains("again to quit")
}

fn quit_pending(s: &crate::picker::Snapshot) -> bool {
    s.footer.contains("again to quit")
}

#[test]
fn test_typing_filters_and_enter_selects_the_highlighted_match() {
    let result = drive(
        menu("Choose", &["alpha", "beta", "alphabet"], Next::Done),
        |keys| {
            keys.type_text("alp");
            keys.press(DOWN);
            keys.press(ENTER);
        },
    );
    assert_eq!(result, Outcome::Done("alphabet".to_string()));
}

#[test]
fn test_digits_quick_select_only_short_unfiltered_lists() {
    let result = drive(menu("Choose", &["one", "two"], Next::Done), |keys| {
        keys.press("2")
    });
    assert_eq!(result, Outcome::Done("two".to_string()));

    let mut names = vec!["Tesla T4".to_string(), "RTX 4090".to_string()];
    names.extend((0..10).map(|n| format!("GPU {n}")));
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let filter_4090 = |keys: &Keyboard| {
        keys.type_text("4090");
        keys.press(ENTER);
    };
    assert_eq!(
        drive(menu("Long", &refs, Next::Done), filter_4090),
        Outcome::Done("RTX 4090".to_string())
    );
    // GPU screens keep digits as filter text even when the list is short.
    let short = menu("GPU", &refs[..2], Next::Done).quick_select(false);
    assert_eq!(
        drive(short, filter_4090),
        Outcome::Done("RTX 4090".to_string())
    );
}

#[test]
fn test_search_list_replaces_options_while_filtering() {
    let screen = menu("GPU", &["Datacenter", "Consumer"], Next::Done)
        .search(|| {
            ["Tesla T4", "RTX 4090"]
                .iter()
                .map(|m| Opt::new(m.to_string(), *m))
                .collect()
        })
        .quick_select(false);
    let result = drive(screen, |keys| {
        keys.type_text("t4");
        keys.press(ENTER);
    });
    assert_eq!(result, Outcome::Done("Tesla T4".to_string()));
}

#[test]
fn test_back_clears_filter_then_goes_back_one_screen_then_quits_on_first_twice() {
    for back in [BACK, LEFT] {
        let result = drive(chain(), move |keys| {
            keys.press(DOWN);
            keys.press_until(ENTER, on("Second"));
            keys.press_until(ENTER, on("Third"));
            keys.type_text("x");
            keys.press_until(back, |s| s.filter.is_empty());
            assert_eq!(keys.title(), "Third");
            keys.press_until(back, on("Second"));
            keys.press_until(back, on("First"));
            keys.press_until(back, quit_pending);
            keys.press(back);
        });
        assert_eq!(result, Outcome::Cancelled(1), "{back:?}");
    }
}

#[test]
fn test_going_back_restores_the_previous_screen_as_it_was_left() {
    let result = drive(chain(), |keys| {
        keys.press(DOWN);
        keys.press_until(ENTER, on("Second"));
        keys.press_until(BACK, on("First"));
        keys.press_until(ENTER, on("Second"));
        keys.press_until(ENTER, on("Third"));
        keys.press(ENTER);
    });
    assert_eq!(result, three("b", "c", "e"));
}

#[test]
fn test_right_selects_like_enter() {
    for enter in [ENTER, RIGHT] {
        let result = drive(chain(), move |keys| {
            keys.press(DOWN);
            keys.press_until(enter, on("Second"));
            keys.type_text("d");
            keys.press_until(enter, on("Third"));
            keys.press(enter);
        });
        assert_eq!(result, three("b", "d", "e"), "{enter:?}");
    }
}

#[test]
fn test_one_back_then_another_key_on_the_first_screen_does_not_quit() {
    for other in [DOWN, "e", ESC, "\x1b[H", "\t", "\x0c", "\x01"] {
        let result = drive(chain(), move |keys| {
            assert!(keys.footer().contains("`/\u{2190} quit"));
            keys.press_until(BACK, |s| {
                s.footer.contains("Press `/\u{2190} again to quit")
            });
            keys.press_until(other, not_quit_pending);
            keys.press(BACKSPACE); // drops the typed letter, if any
            keys.press_until(BACK, quit_pending);
            keys.press_until(DOWN, not_quit_pending);
            keys.press_until(BACK, quit_pending);
            keys.press_until(RIGHT, on("Second"));
            keys.press_until(ENTER, on("Third"));
            keys.press(ENTER);
        });
        let Outcome::Done((_, second, third)) = result else {
            panic!("{other:?}: {result:?}")
        };
        assert_eq!((second.as_str(), third.as_str()), ("c", "e"), "{other:?}");
    }
}

#[test]
fn test_backtick_then_left_quits_the_first_screen() {
    let result = drive(chain(), |keys| {
        keys.press_until(BACK, quit_pending);
        keys.press(LEFT);
    });
    assert_eq!(result, Outcome::Cancelled(1));
}

#[test]
fn test_special_keys_never_leak_into_the_filter_or_url_entry() {
    let screen = menu("First", &["alpha", "beta"], |_| {
        Next::Screen(
            Screen::new("URL", Vec::new, Next::Done)
                .text_input(None)
                .quick_select(false),
        )
    });
    let result = drive(screen, |keys| {
        keys.type_text("a");
        for key in SPECIAL_KEYS {
            keys.press(key);
        }
        assert_eq!(keys.filter(), "a");
        assert_eq!(keys.title(), "First");
        keys.press_until(RIGHT, on("URL"));
        keys.type_text("o/r");
        for key in SPECIAL_KEYS {
            keys.press(key);
        }
        assert_eq!(keys.filter(), "o/r");
        keys.press(RIGHT);
    });
    assert_eq!(result, Outcome::Done("o/r".to_string()));
}

#[test]
fn test_key_hints_leave_out_ctrl_c() {
    let result = drive(chain(), |keys| {
        assert!(!keys.footer().contains("Ctrl-C"));
        keys.press_until(ENTER, on("Second"));
        assert!(!keys.footer().contains("Ctrl-C"));
        keys.press(CTRL_C);
    });
    assert!(matches!(result, Outcome::Cancelled(_)));
}

#[test]
fn test_ctrl_c_and_ctrl_d_quit_at_once_even_with_a_quit_pending() {
    for first_screen in [true, false] {
        for (key, status) in [(CTRL_C, 130), (CTRL_D, 1)] {
            let result = drive(chain(), move |keys| {
                if first_screen {
                    keys.press_until(BACK, quit_pending);
                } else {
                    keys.press_until(ENTER, on("Second"));
                }
                keys.press(key);
            });
            assert_eq!(result, Outcome::Cancelled(status));
        }
    }
}

#[test]
fn test_ctrl_c_and_ctrl_d_quit_from_a_deep_screen() {
    for (key, status) in [(CTRL_C, 130), (CTRL_D, 1)] {
        let result = drive(chain(), move |keys| {
            keys.press_until(ENTER, on("Second"));
            keys.press_until(ENTER, on("Third"));
            keys.press(key);
        });
        assert_eq!(result, Outcome::Cancelled(status));
    }
}

#[test]
fn test_escape_is_ignored_and_never_leaks_into_navigation() {
    let result = drive(chain(), |keys| {
        keys.press_until(ENTER, on("Second"));
        keys.press(&format!("{ESC}{ESC}"));
        keys.press(ESC);
        assert_eq!(keys.title(), "Second");
        keys.press_until(BACK, on("First"));
        keys.press(DOWN);
        keys.press_until(ENTER, on("Second"));
        keys.press_until(ENTER, on("Third"));
        keys.press(ENTER);
    });
    assert_eq!(result, three("b", "c", "e"));
}

#[test]
fn test_arrow_key_split_across_reads_still_moves() {
    let result = drive(menu("Choose", &["one", "two"], Next::Done), |keys| {
        keys.press("\x1b");
        keys.press("[");
        keys.press("B");
        keys.press(ENTER);
    });
    assert_eq!(result, Outcome::Done("two".to_string()));
}

#[test]
fn test_loading_screen_stays_responsive_while_its_loader_blocks() {
    let (gate, calls) = (Gate::default(), Arc::new(AtomicUsize::new(0)));
    let job = gated(vec!["late".to_string()], &gate, &calls);
    let observer = job.observer();
    let results = job.clone();
    let first = menu("First", &["go"], move |_| {
        let results = results.clone();
        Next::Screen(
            Screen::new(
                "Slow",
                move || {
                    results
                        .result()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|v| Opt::new(v.clone(), v))
                        .collect()
                },
                Next::Done,
            )
            .load(job.clone()),
        )
    });
    let release = gate.clone();
    let result = drive(first, move |keys| {
        let latency = keys.press_until(ENTER, on("Slow"));
        assert!(latency < Duration::from_millis(500));
        keys.wait_for("the loader to run", || {
            observer.state() == JobState::Running
        });
        keys.press(ENTER); // nothing to select yet
        assert_eq!(keys.title(), "Slow");
        keys.type_text("la");
        keys.press_until(BACK, |s| s.filter.is_empty());
        let latency = keys.press_until(BACK, on("First"));
        assert!(latency < Duration::from_millis(500) && observer.state() == JobState::Running);
        keys.press_until(ENTER, on("Slow"));
        release.set();
        keys.wait_for("the loader to finish", || {
            observer.state() == JobState::Done
        });
        keys.wait(|s| s.labels == ["late"]);
        keys.press(ENTER);
    });
    gate.set();
    assert_eq!(result, Outcome::Done("late".to_string()));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn test_failed_load_shows_error_and_enter_retries() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let job = Job::new(move || {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            Err("gh: not logged in".to_string())
        } else {
            Ok(vec!["repo".to_string()])
        }
    });
    let observer = job.observer();
    let results = job.clone();
    let screen = Screen::new(
        "Repository",
        move || {
            results
                .result()
                .unwrap_or_default()
                .into_iter()
                .map(|v| Opt::new(v.clone(), v))
                .collect()
        },
        Next::Done,
    )
    .load(job);
    let result = drive(screen, move |keys| {
        keys.wait_for("the load to fail", || observer.state() == JobState::Failed);
        assert_eq!(observer.error(), "gh: not logged in");
        keys.wait(|s| {
            s.body == ["  gh: not logged in"] && s.footer.starts_with("Enter/\u{2192} retry")
        });
        let retried = observer.clone();
        keys.send_until(ENTER, "the retry to finish", move || {
            retried.state() == JobState::Done
        });
        keys.wait(|s| s.labels == ["repo"]);
        keys.press(ENTER);
    });
    assert_eq!(result, Outcome::Done("repo".to_string()));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[test]
fn test_f2_loads_prices_in_the_background_once() {
    let (gate, calls) = (Gate::default(), Arc::new(AtomicUsize::new(0)));
    let prices = gated(vec![("one".to_string(), "$1".to_string())], &gate, &calls);
    let observer = prices.observer();
    let stats = prices.clone();
    let options = move || {
        let stats = stats.result().unwrap_or_default();
        ["one", "two"]
            .iter()
            .map(|v| {
                let price = stats
                    .iter()
                    .find(|(k, _)| k == v)
                    .map(|(_, p)| p.as_str())
                    .unwrap_or("");
                Opt::new(v.to_string(), format!("{v} {price}").trim().to_string())
            })
            .collect()
    };
    let screen = Screen::new("GPU", options, Next::Done)
        .prices(prices)
        .quick_select(false);
    let release = gate.clone();
    let result = drive(screen, move |keys| {
        let running = observer.clone();
        keys.send_until(F2, "prices to load", move || {
            running.state() == JobState::Running
        });
        // Keys still work while the price search blocks.
        keys.type_text("tw");
        keys.press_until(BACK, |s| s.filter.is_empty());
        release.set();
        keys.wait_for("prices to finish", || observer.state() == JobState::Done);
        keys.wait(|s| s.labels == ["one $1", "two"]);
        keys.press(F2);
        keys.press(ENTER);
    });
    gate.set();
    assert_eq!(result, Outcome::Done("one".to_string()));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn test_quitting_does_not_wait_for_a_running_loader() {
    let (gate, calls) = (Gate::default(), Arc::new(AtomicUsize::new(0)));
    let job = gated(Vec::<String>::new(), &gate, &calls);
    let observer = job.observer();
    let screen: Screen<String, String> = Screen::new("Offer", Vec::new, Next::Done).load(job);
    let start = Instant::now();
    let result = drive(screen, move |keys| {
        keys.wait_for("the loader to run", || {
            observer.state() == JobState::Running
        });
        keys.press(CTRL_C);
    });
    gate.set();
    assert!(matches!(result, Outcome::Cancelled(_)));
    assert!(start.elapsed() < Duration::from_secs(2));
}
