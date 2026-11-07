//! Elapsed-time feedback for blocking cloud operations, never on stdout.
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::ctx::{Console, Ctx};
use crate::error::Result;

/// Owns the ticker so every return path stops it before printing a result or
/// opening a picker. Dropping wakes it immediately, without a one-second delay.
pub(crate) struct Wait {
    console: Arc<Console>,
    ticker: Option<(mpsc::Sender<()>, JoinHandle<()>)>,
}

impl Wait {
    pub(crate) fn start(ctx: &Ctx, message: &str) -> Result<Self> {
        let console = ctx.console.clone();
        if !ctx.interactive() || !console.stderr_terminal() {
            return Ok(Self {
                console,
                ticker: None,
            });
        }
        console.write_err(&line(message, 0))?;
        let (stop, wake) = mpsc::channel();
        let output = console.clone();
        let clock = ctx.clock.clone();
        let start = clock.monotonic();
        let message = message.to_owned();
        let ticker = std::thread::Builder::new()
            .name("cloud-wait".into())
            .spawn(move || {
                while wake.recv_timeout(Duration::from_secs(1))
                    == Err(mpsc::RecvTimeoutError::Timeout)
                {
                    let seconds = (clock.monotonic() - start).max(0.0) as u64;
                    if output.write_err(&line(&message, seconds)).is_err() {
                        break;
                    }
                }
            });
        match ticker {
            Ok(ticker) => Ok(Self {
                console,
                ticker: Some((stop, ticker)),
            }),
            Err(error) => {
                let _ = console.write_err("\r\x1b[2K");
                Err(error.into())
            }
        }
    }
}

fn line(message: &str, seconds: u64) -> String {
    format!("\r\x1b[2K{message}... {}:{:02}", seconds / 60, seconds % 60)
}

impl Drop for Wait {
    fn drop(&mut self) {
        if let Some((stop, ticker)) = self.ticker.take() {
            let _ = stop.send(());
            let _ = ticker.join();
            let _ = self.console.write_err("\r\x1b[2K");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_time_keeps_counting_past_an_hour() {
        assert_eq!(line("Waiting", 0), "\r\x1b[2KWaiting... 0:00");
        assert_eq!(line("Waiting", 62), "\r\x1b[2KWaiting... 1:02");
        assert_eq!(line("Waiting", 3601), "\r\x1b[2KWaiting... 60:01");
    }
}
