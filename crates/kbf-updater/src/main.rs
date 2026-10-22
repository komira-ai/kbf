//! The `kbf-updater` binary: the roll API on a unix socket. See [`kbf_updater::run`].

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
