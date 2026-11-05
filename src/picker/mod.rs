//! Keyboard picker: one loop hosting a stack of screens.
//!
//! Moving between screens never leaves the loop, so a transition costs one
//! redraw and no keypress can fall between two terminal sessions. Slow work
//! (GitHub and Vast CLI calls) runs in `Job`s off the loop, so every key,
//! including ` (back) and Ctrl-C, is handled immediately even while a screen
//! is still loading. The engine here does no I/O; `term` drives it.

pub mod job;
pub mod keys;
pub mod term;
#[cfg(test)]
pub mod testing;

use std::cell::RefCell;
use std::rc::{Rc, Weak};

pub use job::{AnyJob, Job, JobObserver, JobState};
use keys::Key;

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::pyfmt::{casefold, is_printable, is_printable_str};

/// A choice on a screen: the value it selects and its label.
#[derive(Clone, Debug, PartialEq)]
pub struct Opt<V> {
    pub value: V,
    pub label: String,
    /// Explicit digit on unfiltered menus; otherwise numbering is positional.
    pub digit: Option<u8>,
}

impl<V> Opt<V> {
    pub fn new(value: V, label: impl Into<String>) -> Opt<V> {
        Opt {
            value,
            label: label.into(),
            digit: None,
        }
    }

    pub fn digit(mut self, digit: u8) -> Self {
        self.digit = Some(digit);
        self
    }
}

/// Option values of one flow; text entry screens submit their text.
pub trait PickValue: Clone + 'static {
    fn from_text(text: String) -> Self;
}

impl PickValue for String {
    fn from_text(text: String) -> Self {
        text
    }
}

/// What selecting leads to: a screen, an inline notice, or the flow's result.
#[allow(clippy::large_enum_variant)] // Built once per key press; boxing buys nothing.
pub enum Next<V, R> {
    Screen(Screen<V, R>),
    /// Keep the current screen and show an actionable message until dismissed.
    Notice(String),
    Done(R),
}

type Options<V> = Box<dyn Fn() -> Vec<Opt<V>>>;
/// Checks text entry; the error is shown inline.
pub type Validate = Box<dyn Fn(&str) -> std::result::Result<(), String>>;
type Settled<V, R> = Box<dyn Fn() -> Option<Next<V, R>>>;

/// One step of a picker flow.
///
/// `options` is called on every redraw, so labels can reflect data that loads
/// later. `select` returns the next screen, or the flow's final result.
type PriceSource<V> = Box<dyn Fn(&V) -> Rc<dyn AnyJob>>;

pub struct Screen<V, R> {
    pub title: String,
    pub options: Options<V>,
    pub select: Box<dyn Fn(V) -> Next<V, R>>,
    /// Options searched instead of `options` while a filter is typed.
    pub search: Option<Options<V>>,
    /// Must finish before options are shown; starts when the screen first opens.
    pub load: Option<Box<dyn AnyJob>>,
    pub loading: String,
    pub empty: String,
    /// F2 starts it; `options` should read its result.
    pub prices: Option<Rc<dyn AnyJob>>,
    pub price_source: Option<PriceSource<V>>,
    /// Digits choose directly on unfiltered lists with explicit digits or at most nine rows.
    pub quick_select: bool,
    /// Optional background load: static choices remain usable while it runs or fails.
    pub load_required: bool,
    /// Text entry shares the same loop and stack; validation gives an inline error.
    pub text_input: bool,
    pub input_label: String,
    pub input_hint: String,
    pub back_clears_input: bool,
    pub validate: Option<Validate>,
    pub on_settled: Option<Settled<V, R>>,
    pub notice: String,
    pub wait_initial: bool,
}

impl<V: PickValue, R: 'static> Screen<V, R> {
    pub fn new(
        title: impl Into<String>,
        options: impl Fn() -> Vec<Opt<V>> + 'static,
        select: impl Fn(V) -> Next<V, R> + 'static,
    ) -> Screen<V, R> {
        Screen {
            title: title.into(),
            options: Box::new(options),
            select: Box::new(select),
            search: None,
            load: None,
            loading: "Loading...".into(),
            empty: "Nothing to choose from".into(),
            prices: None,
            price_source: None,
            quick_select: true,
            load_required: true,
            text_input: false,
            input_label: "Input".into(),
            input_hint: "Type a value, then Enter/→".into(),
            back_clears_input: true,
            validate: None,
            on_settled: None,
            notice: String::new(),
            wait_initial: false,
        }
    }

    pub fn search(mut self, search: impl Fn() -> Vec<Opt<V>> + 'static) -> Self {
        self.search = Some(Box::new(search));
        self
    }

    pub fn load(mut self, job: impl AnyJob + 'static) -> Self {
        self.load = Some(Box::new(job));
        self
    }

    pub fn loading(mut self, text: impl Into<String>) -> Self {
        self.loading = text.into();
        self
    }

    pub fn empty(mut self, text: impl Into<String>) -> Self {
        self.empty = text.into();
        self
    }

    pub fn prices(mut self, job: impl AnyJob + 'static) -> Self {
        self.prices = Some(Rc::new(job));
        self
    }

    pub fn price_source(mut self, source: impl Fn(&V) -> Rc<dyn AnyJob> + 'static) -> Self {
        self.price_source = Some(Box::new(source));
        self
    }

    pub fn quick_select(mut self, on: bool) -> Self {
        self.quick_select = on;
        self
    }

    pub fn load_required(mut self, on: bool) -> Self {
        self.load_required = on;
        self
    }

    pub fn text_input(mut self, validate: Option<Validate>) -> Self {
        self.text_input = true;
        self.validate = validate;
        self
    }

    pub fn input_hint(mut self, hint: impl Into<String>) -> Self {
        self.input_hint = hint.into();
        self
    }

    pub fn back_clears_input(mut self, on: bool) -> Self {
        self.back_clears_input = on;
        self
    }

    pub fn input_label(mut self, label: impl Into<String>) -> Self {
        self.input_label = label.into();
        self
    }

    pub fn on_settled(mut self, f: impl Fn() -> Option<Next<V, R>> + 'static) -> Self {
        self.on_settled = Some(Box::new(f));
        self
    }

    pub fn notice(mut self, text: impl Into<String>) -> Self {
        self.notice = text.into();
        self
    }

    pub fn wait_initial(mut self, on: bool) -> Self {
        self.wait_initial = on;
        self
    }
}

/// A screen on the navigation stack with its filter and cursor.
struct View<V, R> {
    screen: Rc<Screen<V, R>>,
    filter: String,
    cursor: usize,
    error: String,
    selection_notice: Option<String>,
}

impl<V: PickValue, R: 'static> View<V, R> {
    fn price_job(&self) -> Option<Rc<dyn AnyJob>> {
        let entries = self.entries();
        self.screen
            .price_source
            .as_ref()
            .and_then(|source| {
                entries
                    .get(self.current(entries.len()))
                    .map(|opt| source(&opt.value))
            })
            .or_else(|| self.screen.prices.clone())
    }

    fn new(screen: Screen<V, R>) -> View<V, R> {
        View {
            screen: Rc::new(screen),
            filter: String::new(),
            cursor: 0,
            error: String::new(),
            selection_notice: None,
        }
    }

    fn ready(&self) -> bool {
        let load = self.screen.load.as_deref();
        if self.screen.wait_initial
            && load.is_some_and(|l| matches!(l.state(), JobState::Idle | JobState::Running))
        {
            return false;
        }
        !self.screen.load_required || load.is_none_or(|l| l.state() == JobState::Done)
    }

    fn entries(&self) -> Vec<Opt<V>> {
        if !self.ready() {
            return Vec::new();
        }
        let source = match &self.screen.search {
            Some(search) if !self.filter.is_empty() => search(),
            _ => (self.screen.options)(),
        };
        let needle = casefold(&self.filter);
        source
            .into_iter()
            .filter(|o| casefold(&o.label).contains(&needle))
            .collect()
    }

    fn current(&self, count: usize) -> usize {
        self.cursor.min(count.saturating_sub(1))
    }

    fn quick_select(&self, entries: &[Opt<V>]) -> bool {
        self.screen.quick_select
            && self.filter.is_empty()
            && (entries.iter().any(|o| o.digit.is_some()) || (1..=9).contains(&entries.len()))
    }

    fn load_failed_required(&self) -> bool {
        self.screen.load_required
            && self
                .screen
                .load
                .as_deref()
                .is_some_and(|l| l.state() == JobState::Failed)
    }

    fn set_filter(&mut self, filter: String) {
        self.filter = filter;
        self.cursor = 0;
        self.error.clear();
    }
}

/// How a picker ended.
#[derive(Debug, PartialEq)]
pub enum Outcome<R> {
    Done(R),
    Cancelled(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Plain,
    Title,
    Filter,
    Selected,
    Note,
    Error,
}

/// One logical line of styled text.
pub type Line = Vec<(Style, String)>;

/// What to draw: header and footer are cut at the width, the body wraps.
pub struct Frame {
    pub header: Vec<Line>,
    pub body: Vec<Line>,
    /// The body line holding the selection, kept visible when scrolling.
    pub cursor_line: Option<usize>,
    pub footer: String,
}

/// The visible state, published for tests after every redraw.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub title: String,
    pub filter: String,
    pub labels: Vec<String>,
    pub footer: String,
    pub notice: String,
    pub loading: String,
    pub body: Vec<String>,
    pub done: bool,
    /// Set once the loop has drawn its first frame.
    pub drawn: bool,
}

/// The picker state machine: a stack of views, driven by keys and job
/// completions.
pub struct Core<V, R> {
    stack: Vec<View<V, R>>,
    quit_pending: bool,
    outcome: Option<Outcome<R>>,
    this: Weak<RefCell<Core<V, R>>>,
}

impl<V: PickValue, R: 'static> Core<V, R> {
    pub fn new(first: Screen<V, R>) -> Rc<RefCell<Core<V, R>>> {
        let core = Rc::new(RefCell::new(Core {
            stack: vec![View::new(first)],
            quit_pending: false,
            outcome: None,
            this: Weak::new(),
        }));
        core.borrow_mut().this = Rc::downgrade(&core);
        core
    }

    fn view(&self) -> &View<V, R> {
        self.stack.last().expect("picker stack is never empty")
    }

    fn view_mut(&mut self) -> &mut View<V, R> {
        self.stack.last_mut().expect("picker stack is never empty")
    }

    pub fn finished(&self) -> bool {
        self.outcome.is_some()
    }

    pub fn take_outcome(&mut self) -> Option<Outcome<R>> {
        self.outcome.take()
    }

    fn exit(&mut self, outcome: Outcome<R>) {
        if self.outcome.is_none() {
            self.outcome = Some(outcome);
        }
    }

    /// Start the top screen's load (`retry` restarts a failed one).
    pub fn open(&mut self, retry: bool) {
        let screen = self.view().screen.clone();
        let Some(load) = screen.load.as_deref() else {
            return;
        };
        if retry || matches!(load.state(), JobState::Idle | JobState::Running) {
            let this = self.this.clone();
            let settled_screen = screen.clone();
            load.start(Box::new(move || {
                if let Some(core) = this.upgrade() {
                    Core::settled(&core, &settled_screen);
                }
            }));
        }
    }

    fn settled(core: &Rc<RefCell<Core<V, R>>>, screen: &Rc<Screen<V, R>>) {
        let Some(on_settled) = &screen.on_settled else {
            return;
        };
        match on_settled() {
            Some(Next::Screen(replacement)) => {
                let mut core = core.borrow_mut();
                core.stack = vec![View::new(replacement)];
                core.open(false);
            }
            Some(Next::Notice(message)) => {
                core.borrow_mut().view_mut().selection_notice = Some(message)
            }
            Some(Next::Done(result)) => core.borrow_mut().exit(Outcome::Done(result)),
            None => {}
        }
    }

    fn choose(&mut self, value: V) {
        let following = (self.view().screen.select)(value);
        match following {
            Next::Screen(screen) => {
                self.stack.push(View::new(screen));
                self.open(false);
            }
            Next::Notice(message) => self.view_mut().selection_notice = Some(message),
            Next::Done(result) => self.exit(Outcome::Done(result)),
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let count = self.view().entries().len();
        if count > 0 {
            let view = self.view_mut();
            let current = view.current(count) as isize;
            view.cursor = (current + delta).rem_euclid(count as isize) as usize;
        }
    }

    pub fn handle_key(&mut self, key: Key) {
        if self.outcome.is_some() {
            return;
        }
        // A selection notice is modal: don't change hidden choices or let
        // buffered Enter presses dismiss setup instructions before they are read.
        if self.view().selection_notice.is_some()
            && !matches!(key, Key::Left | Key::Char('`') | Key::CtrlC | Key::CtrlD)
        {
            return;
        }
        // Any key but a back key disarms the first-screen quit, including ignored keys.
        let back = matches!(key, Key::Left | Key::Char('`'));
        if !back {
            self.quit_pending = false;
        }
        match key {
            Key::Up => self.move_cursor(-1),
            Key::Down => self.move_cursor(1),
            Key::Enter | Key::Right => self.enter(),
            Key::Left | Key::Char('`') => {
                if self.view().selection_notice.is_some() {
                    self.view_mut().selection_notice = None;
                } else if !self.view().filter.is_empty() && self.view().screen.back_clears_input {
                    self.view_mut().set_filter(String::new());
                } else if self.stack.len() > 1 {
                    self.stack.pop();
                } else if self.quit_pending {
                    self.exit(Outcome::Cancelled(1));
                } else {
                    self.quit_pending = true;
                }
            }
            Key::Backspace => {
                let mut filter = self.view().filter.clone();
                filter.pop();
                self.view_mut().set_filter(filter);
            }
            Key::CtrlR => self.open(true),
            Key::F2 => {
                if let Some(prices) = self.view().price_job() {
                    prices.start(Box::new(|| {}));
                }
            }
            Key::CtrlC => self.exit(Outcome::Cancelled(130)),
            Key::CtrlD => self.exit(Outcome::Cancelled(1)),
            Key::Paste(text) => {
                let text = text.trim();
                if is_printable_str(text) {
                    let filter = format!("{}{}", self.view().filter, text);
                    self.view_mut().set_filter(filter);
                }
            }
            Key::Char(c) => self.type_char(c),
            Key::Escape | Key::Ignored => {}
        }
    }

    fn enter(&mut self) {
        let view = self.view();
        if view.screen.text_input {
            if let Some(validate) = &view.screen.validate {
                if let Err(error) = validate(&view.filter) {
                    self.view_mut().error = error;
                    return;
                }
            }
            let text = view.filter.clone();
            self.choose(V::from_text(text));
            return;
        }
        if view.load_failed_required() {
            self.open(true);
            return;
        }
        let entries = view.entries();
        if !entries.is_empty() {
            let value = entries[view.current(entries.len())].value.clone();
            self.choose(value);
        }
    }

    fn type_char(&mut self, c: char) {
        if !is_printable(c) {
            return;
        }
        if let Some(digit) = c.to_digit(10) {
            let entries = self.view().entries();
            if self.view().quick_select(&entries) {
                let option = if entries.iter().any(|o| o.digit.is_some()) {
                    entries.iter().find(|o| o.digit == Some(digit as u8))
                } else {
                    digit.checked_sub(1).and_then(|i| entries.get(i as usize))
                };
                if let Some(option) = option {
                    self.choose(option.value.clone());
                    return;
                }
            }
        }
        let filter = format!("{}{c}", self.view().filter);
        self.view_mut().set_filter(filter);
    }

    pub fn title(&self) -> String {
        self.view().screen.title.clone()
    }

    pub fn filter(&self) -> String {
        self.view().filter.clone()
    }

    /// Labels currently listed, after filtering.
    pub fn labels(&self) -> Vec<String> {
        self.view().entries().into_iter().map(|o| o.label).collect()
    }

    pub fn header(&self) -> Vec<Line> {
        let view = self.view();
        let mut header = vec![vec![(Style::Title, view.screen.title.clone())]];
        if !view.filter.is_empty() {
            let prefix = if view.screen.text_input {
                &view.screen.input_label
            } else {
                "Filter"
            };
            header.push(vec![(Style::Filter, format!("{prefix}: {}", view.filter))]);
        }
        header
    }

    /// Body lines and the line index holding the selection.
    pub fn body(&self) -> (Vec<Line>, Option<usize>) {
        let view = self.view();
        let screen = &view.screen;
        let load = screen.load.as_deref();
        let one = |style: Style, text: String| -> Vec<Line> {
            text.split('\n')
                .map(|l| vec![(style, l.to_string())])
                .collect()
        };
        if let Some(message) = &view.selection_notice {
            return (one(Style::Error, message.clone()), None);
        }
        if screen.text_input {
            return if view.error.is_empty() {
                (one(Style::Note, format!("  {}", screen.input_hint)), None)
            } else {
                (one(Style::Error, view.error.clone()), None)
            };
        }
        if view.load_failed_required() {
            let error = load.map(|l| l.error()).unwrap_or_default();
            let lines: Vec<String> = crate::pyfmt::splitlines(&error)
                .iter()
                .map(|line| format!("  {line}"))
                .collect();
            return (one(Style::Error, lines.join("\n")), None);
        }
        if !view.ready() {
            return (one(Style::Note, format!("  {}", screen.loading)), None);
        }
        let entries = view.entries();
        if entries.is_empty() {
            let text = if view.filter.is_empty() {
                format!("  {}", screen.empty)
            } else {
                "  No matches".into()
            };
            return (one(Style::Note, text), None);
        }
        let current = view.current(entries.len());
        let width = entries.len().to_string().len();
        let mut lines: Vec<Line> = Vec::new();
        if !screen.notice.is_empty() {
            lines.extend(one(Style::Error, format!("  {}", screen.notice)));
        }
        let mut cursor_line = None;
        for (index, option) in entries.iter().enumerate() {
            let number = if view.filter.is_empty() {
                option.digit.map(usize::from).unwrap_or(index + 1)
            } else {
                index + 1
            };
            let number = format!("{number:>width$}");
            let label_lines: Vec<&str> = option.label.split('\n').collect();
            if index == current {
                // Scrolls the list so the selected line stays visible.
                cursor_line = Some(lines.len());
                let first = format!("\u{203a} {number}. {}", label_lines[0]);
                lines.push(vec![(Style::Selected, first)]);
                for rest in &label_lines[1..] {
                    lines.push(vec![(Style::Selected, rest.to_string())]);
                }
            } else {
                lines.push(vec![(
                    Style::Plain,
                    format!("  {number}. {}", label_lines[0]),
                )]);
                for rest in &label_lines[1..] {
                    lines.push(vec![(Style::Plain, rest.to_string())]);
                }
            }
        }
        if let Some(load) = load.filter(|_| !screen.load_required) {
            match load.state() {
                JobState::Running => {
                    lines.extend(one(Style::Note, format!("  {}", screen.loading)))
                }
                JobState::Failed => lines.extend(one(Style::Error, format!("  {}", load.error()))),
                _ => {}
            }
        }
        (lines, cursor_line)
    }

    pub fn footer(&self) -> String {
        let view = self.view();
        if view.selection_notice.is_some() {
            return "`/\u{2190} return to choices  Ctrl-C quit".into();
        }
        let screen = &view.screen;
        let load = screen.load.as_deref();
        let mut hints: Vec<String> = if screen.text_input {
            vec!["Enter/\u{2192} continue".into(), "Backspace edit".into()]
        } else if view.load_failed_required() {
            vec!["Enter/\u{2192} retry".into()]
        } else {
            let mut hints = vec![
                "\u{2191}/\u{2193} move".to_string(),
                "Enter/\u{2192} select".into(),
            ];
            if view.quick_select(&view.entries()) {
                hints.push(if view.entries().iter().any(|o| o.digit == Some(0)) {
                    "0-9 select".into()
                } else {
                    "1-9 select".into()
                });
            }
            hints
        };
        if !view.filter.is_empty() && screen.back_clears_input {
            hints.push("`/\u{2190} clear".into());
        } else if self.quit_pending {
            hints.push("Press `/\u{2190} again to quit".into());
        } else if self.stack.len() > 1 {
            hints.push("`/\u{2190} back".into());
        } else {
            hints.push("`/\u{2190} quit (twice)".into());
        }
        if !screen.load_required && load.is_some_and(|l| l.state() == JobState::Failed) {
            hints.push("Ctrl-R retry repositories".into());
        }
        if let Some(prices) = view.price_job() {
            match prices.state() {
                JobState::Idle => hints.push("F2 prices".into()),
                JobState::Running => hints.push("Loading prices...".into()),
                JobState::Failed => {
                    let error = prices.error();
                    hints.push(format!(
                        "F2 retry prices ({})",
                        crate::pyfmt::splitlines(&error).first().unwrap_or(&"")
                    ));
                }
                JobState::Done => {}
            }
        }
        hints.join("  ")
    }

    pub fn frame(&self) -> Frame {
        let (body, cursor_line) = self.body();
        Frame {
            header: self.header(),
            body,
            cursor_line,
            footer: self.footer(),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let screen = &self.view().screen;
        Snapshot {
            title: self.title(),
            filter: self.filter(),
            labels: self.labels(),
            footer: self.footer(),
            notice: screen.notice.clone(),
            loading: screen.loading.clone(),
            body: self
                .body()
                .0
                .iter()
                .map(|line| line.iter().map(|(_, t)| t.as_str()).collect())
                .collect(),
            done: self.outcome.is_some(),
            drawn: true,
        }
    }
}

/// Run a picker flow from `first` on the terminal (or the scripted keys
/// tests provide); its result, or `Error::Cancelled` when the user quits.
pub fn pick<V: PickValue, R: 'static>(ctx: &Ctx, first: Screen<V, R>) -> Result<R> {
    let outcome = match ctx.ui.next_script() {
        Some(keys) => term::run_scripted(first, &keys),
        None => term::run_terminal(first)?,
    };
    match outcome {
        Outcome::Done(result) => Ok(result),
        Outcome::Cancelled(status) => Err(Error::Cancelled(status)),
    }
}
