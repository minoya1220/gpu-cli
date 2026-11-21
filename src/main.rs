use std::io::Write;

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

fn main() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some(gpu::picker::job::THREAD_NAME) {
            return;
        }
        gpu::picker::term::restore_terminal();
        default_hook(info);
    }));
    let ctx = gpu::ctx::Ctx::real();
    // `gpu autosync-run` is the detached background syncer: its own session,
    // deaf to its terminal closing, and a stop lets a pull in progress finish.
    let background = std::env::args().nth(1).as_deref() == Some("autosync-run");
    let signals = if background {
        let _ = rustix::process::setsid();
        let _ = signal_hook::flag::register(SIGHUP, std::sync::Arc::new(Default::default()));
        Signals::new([SIGINT, SIGTERM])
    } else {
        Signals::new([SIGINT, SIGHUP, SIGTERM])
    };
    if let Ok(mut signals) = signals {
        let handler_ctx = ctx.clone();
        std::thread::spawn(move || {
            for signal in signals.forever() {
                if background {
                    gpu::process::wait_critical(std::time::Duration::from_secs(30));
                    let _ = rustix::io::write(rustix::stdio::stderr(), b"Autosync stopped.\n");
                    std::process::exit(0);
                }
                if handler_ctx.cooperative() {
                    let count = handler_ctx.interrupt();
                    if count < 3 {
                        let message = if count == 1 {
                            "Stopping job; Ctrl-C again to skip downloading.\n"
                        } else {
                            "Skipping download; destroying instance.\n"
                        };
                        let _ = rustix::io::write(rustix::stdio::stderr(), message.as_bytes());
                        continue;
                    }
                }
                gpu::process::forward_signal(signal);
                gpu::picker::term::restore_terminal();
                handler_ctx.report_interrupt(128 + signal);
                std::process::exit(128 + signal);
            }
        });
    }
    let argv: Vec<String> = std::env::args().collect();
    let code = gpu::main_with(&argv, &ctx);
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
