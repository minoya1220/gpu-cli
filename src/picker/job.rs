//! Background work for picker screens.
//!
//! A `Job` runs a blocking call (a `gh` or `vastai` subprocess) on a detached
//! thread, so quitting never waits for it, and settles on the picker thread:
//! state, result and listeners are only touched there. Listeners run in
//! registration order, which the repository re-validation flow relies on.

use std::cell::{Cell, RefCell};
use std::os::fd::OwnedFd;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

/// Name of the threads that run job work; their panics are caught.
pub const THREAD_NAME: &str = "gpu-picker-job";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Idle,
    Running,
    Done,
    Failed,
}

/// What a screen needs from a job, whatever its result type.
pub trait AnyJob {
    fn state(&self) -> JobState;
    fn error(&self) -> String;
    /// Start unless already running or done; `listener` runs when it settles.
    fn start(&self, listener: Box<dyn FnOnce()>);
}

/// Wakes the picker loop from any thread.
pub struct Waker {
    fd: OwnedFd,
}

impl Waker {
    pub fn new(fd: OwnedFd) -> Waker {
        Waker { fd }
    }

    pub fn wake(&self) {
        // A full or closed pipe means the loop is already awake or gone.
        let _ = rustix::io::write(&self.fd, b"!");
    }
}

trait Pending {
    /// Settle if the work finished; true when settled.
    fn poll(&self) -> bool;
}

/// The picker loop's job bookkeeping, current on the picker thread.
pub struct Runtime {
    waker: Arc<Waker>,
    pending: RefCell<Vec<Box<dyn Pending>>>,
}

thread_local! {
    static CURRENT: RefCell<Option<Rc<Runtime>>> = const { RefCell::new(None) };
}

impl Runtime {
    pub fn new(waker: Arc<Waker>) -> Rc<Runtime> {
        Rc::new(Runtime {
            waker,
            pending: RefCell::new(Vec::new()),
        })
    }

    /// Make this runtime current until the guard drops.
    pub fn enter(self: &Rc<Self>) -> RuntimeGuard {
        let previous = CURRENT.with(|c| c.replace(Some(self.clone())));
        RuntimeGuard { previous }
    }

    /// Settle every job whose work finished. Listeners run with no borrow
    /// held, so they may start other jobs.
    pub fn poll(&self) {
        let jobs = std::mem::take(&mut *self.pending.borrow_mut());
        let mut unsettled = Vec::new();
        for job in jobs {
            if !job.poll() {
                unsettled.push(job);
            }
        }
        self.pending.borrow_mut().extend(unsettled);
    }

    fn current() -> Rc<Runtime> {
        CURRENT
            .with(|c| c.borrow().clone())
            .expect("a picker Job can only start while a picker runs")
    }
}

pub struct RuntimeGuard {
    previous: Option<Rc<Runtime>>,
}

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT.with(|c| *c.borrow_mut() = previous);
    }
}

type Work<T> = Arc<dyn Fn(Arc<Waker>) -> Result<T, String> + Send + Sync>;

struct Inner<T> {
    work: Work<T>,
    state: Cell<JobState>,
    result: RefCell<Option<T>>,
    error: RefCell<String>,
    listeners: RefCell<Vec<Box<dyn FnOnce()>>>,
    observed: Arc<Mutex<(JobState, String)>>,
    interim: Arc<Mutex<Option<T>>>,
}

/// A blocking call run in the background; cheap to clone, shared by screens.
pub struct Job<T> {
    inner: Rc<Inner<T>>,
}

impl<T> Clone for Job<T> {
    fn clone(&self) -> Self {
        Job {
            inner: self.inner.clone(),
        }
    }
}

/// Reads a job's state from another thread (tests).
#[cfg(test)]
#[derive(Clone)]
pub struct JobObserver {
    observed: Arc<Mutex<(JobState, String)>>,
}

#[cfg(test)]
impl JobObserver {
    pub fn state(&self) -> JobState {
        self.observed.lock().unwrap_or_else(|p| p.into_inner()).0
    }

    pub fn error(&self) -> String {
        self.observed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .1
            .clone()
    }
}

impl<T: Clone + Send + 'static> Job<T> {
    pub fn new(work: impl Fn() -> Result<T, String> + Send + Sync + 'static) -> Job<T> {
        Self::progress(move |_| work())
    }
    /// Publish partial results through shared data, then wake the renderer.
    pub fn progress(
        work: impl Fn(Arc<Waker>) -> Result<T, String> + Send + Sync + 'static,
    ) -> Job<T> {
        Self::build(Arc::new(work), Arc::default())
    }
    /// Show each published value as the result until the work settles.
    pub fn publishing(
        work: impl Fn(&dyn Fn(T)) -> Result<T, String> + Send + Sync + 'static,
    ) -> Job<T> {
        let interim: Arc<Mutex<Option<T>>> = Arc::default();
        let slot = interim.clone();
        Self::build(
            Arc::new(move |wake: Arc<Waker>| {
                work(&|value| {
                    *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(value);
                    wake.wake();
                })
            }),
            interim,
        )
    }
    fn build(work: Work<T>, interim: Arc<Mutex<Option<T>>>) -> Job<T> {
        Job {
            inner: Rc::new(Inner {
                work,
                state: Cell::new(JobState::Idle),
                result: RefCell::new(None),
                error: RefCell::new(String::new()),
                listeners: RefCell::new(Vec::new()),
                observed: Arc::new(Mutex::new((JobState::Idle, String::new()))),
                interim,
            }),
        }
    }

    pub fn result(&self) -> Option<T> {
        self.inner.result.borrow().clone().or_else(|| {
            self.inner
                .interim
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        })
    }

    #[cfg(test)]
    pub fn observer(&self) -> JobObserver {
        JobObserver {
            observed: self.inner.observed.clone(),
        }
    }

    fn set_state(&self, state: JobState) {
        self.inner.state.set(state);
        let error = self.inner.error.borrow().clone();
        *self
            .inner
            .observed
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = (state, error);
    }

    fn settle(&self, outcome: Result<T, String>) {
        *self.inner.interim.lock().unwrap_or_else(|p| p.into_inner()) = None;
        match outcome {
            Ok(value) => {
                *self.inner.result.borrow_mut() = Some(value);
                self.inner.error.borrow_mut().clear();
                self.set_state(JobState::Done);
            }
            Err(error) => {
                *self.inner.result.borrow_mut() = None;
                *self.inner.error.borrow_mut() = error;
                self.set_state(JobState::Failed);
            }
        }
        let listeners = std::mem::take(&mut *self.inner.listeners.borrow_mut());
        for listener in listeners {
            listener();
        }
    }
}

struct PendingJob<T> {
    job: Job<T>,
    slot: Arc<Mutex<Option<Result<T, String>>>>,
}

impl<T: Clone + Send + 'static> Pending for PendingJob<T> {
    fn poll(&self) -> bool {
        let outcome = self.slot.lock().unwrap_or_else(|p| p.into_inner()).take();
        match outcome {
            Some(outcome) => {
                self.job.settle(outcome);
                true
            }
            None => false,
        }
    }
}

impl<T: Clone + Send + 'static> AnyJob for Job<T> {
    fn state(&self) -> JobState {
        self.inner.state.get()
    }

    fn error(&self) -> String {
        self.inner.error.borrow().clone()
    }

    fn start(&self, listener: Box<dyn FnOnce()>) {
        match self.inner.state.get() {
            JobState::Running => {
                self.inner.listeners.borrow_mut().push(listener);
                return;
            }
            JobState::Done => return,
            JobState::Idle | JobState::Failed => {}
        }
        *self.inner.listeners.borrow_mut() = vec![listener];
        self.set_state(JobState::Running);
        let runtime = Runtime::current();
        let slot = Arc::new(Mutex::new(None));
        let (work, waker, filled) = (self.inner.work.clone(), runtime.waker.clone(), slot.clone());
        std::thread::Builder::new()
            .name(THREAD_NAME.into())
            .spawn(move || {
                // Thread boundary: an escaping panic would leave the screen loading forever.
                let outcome = match catch_unwind(AssertUnwindSafe(|| work(waker.clone()))) {
                    Ok(Ok(value)) => Ok(value),
                    Ok(Err(error)) => {
                        let error = error.trim().to_string();
                        Err(if error.is_empty() {
                            "RuntimeError".into()
                        } else {
                            error
                        })
                    }
                    Err(_) => Err("internal error".into()),
                };
                *filled.lock().unwrap_or_else(|p| p.into_inner()) = Some(outcome);
                waker.wake();
            })
            .expect("spawn picker job thread");
        runtime.pending.borrow_mut().push(Box::new(PendingJob {
            job: self.clone(),
            slot,
        }));
    }
}
