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
    // Ctrl-C outside the picker (line prompts, polling, subprocesses): like
    // Python's KeyboardInterrupt path, plus anything the command still has to
    // say, such as an instance that keeps running. The picker reads Ctrl-C as
    // a key.
    if let Ok(mut signals) = Signals::new([SIGINT, SIGHUP, SIGTERM]) {
        let handler_ctx = ctx.clone();
        std::thread::spawn(move || {
            for signal in signals.forever() {
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
                } else if signal != SIGINT {
                    std::process::exit(128 + signal);
                }
                gpu::picker::term::restore_terminal();
                if handler_ctx.cooperative() {
                    // Third press is an emergency exit, not a completed JSON
                    // command. Never inject prose into run's stdout log/JSON.
                    let _ = rustix::io::write(rustix::stdio::stderr(), b"\nCancelled.\n");
                } else {
                    let _ = rustix::io::write(rustix::stdio::stdout(), b"\nCancelled.\n");
                }
                if let Some(notice) = handler_ctx.interrupt_notice() {
                    let _ = rustix::io::write(
                        rustix::stdio::stderr(),
                        format!("{notice}\n").as_bytes(),
                    );
                }
                std::process::exit(130);
            }
        });
    }
    let argv: Vec<String> = std::env::args().collect();
    let code = gpu::main_with(&argv, &ctx);
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
