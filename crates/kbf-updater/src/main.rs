//! The `kbf-updater` binary: a stub until the listener lands. See [`kbf_updater::run`].

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let (code, text) = kbf_updater::run(&args);
    if code == 0 {
        print!("{text}");
    } else {
        eprint!("{text}");
    }
    ExitCode::from(code)
}
