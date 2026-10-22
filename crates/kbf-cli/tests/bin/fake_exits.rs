//! `kbf-cli-fake-exits`: a program that exits at once with status 3, in place of `kbf`, for
//! the test of a farm that cannot start (`tests/beta_b0.rs`). It reads and writes nothing.
//! Built here because a test can run only its own package's binaries. Not shipped.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    eprintln!("fake kbf: refusing to start: {}", args.join(" "));
    ExitCode::from(3)
}
