//! The `kbf-l2-backfill` binary: see the library docs.

use std::io;
use std::process::ExitCode;

fn main() -> ExitCode {
    let (mut out, mut err) = (io::stdout(), io::stderr());
    let argv = std::env::args_os().collect();
    let code = kbf_l2_backfill::main_with(argv, &mut out, &mut err);
    ExitCode::from(code)
}
