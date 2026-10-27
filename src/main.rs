use std::io::Write;

use signal_hook::consts::SIGINT;
use signal_hook::iterator::Signals;

fn main() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        gpu::picker::term::restore_terminal();
        default_hook(info);
    }));
    // Ctrl-C outside the picker (line prompts, polling, subprocesses): like
    // Python's KeyboardInterrupt path. The picker reads Ctrl-C as a key.
    if let Ok(mut signals) = Signals::new([SIGINT]) {
        std::thread::spawn(move || {
            if signals.forever().next().is_some() {
                gpu::picker::term::restore_terminal();
                let _ = rustix::io::write(rustix::stdio::stdout(), b"\nCancelled.\n");
                std::process::exit(130);
            }
        });
    }
    let argv: Vec<String> = std::env::args().collect();
    let code = gpu::main_with(&argv, &gpu::ctx::Ctx::real());
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
