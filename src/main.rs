use std::io::Write;

use signal_hook::consts::SIGINT;
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
    if let Ok(mut signals) = Signals::new([SIGINT]) {
        let handler_ctx = ctx.clone();
        std::thread::spawn(move || {
            if signals.forever().next().is_some() {
                gpu::picker::term::restore_terminal();
                let _ = rustix::io::write(rustix::stdio::stdout(), b"\nCancelled.\n");
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
