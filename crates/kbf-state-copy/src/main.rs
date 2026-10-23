//! The `kbf-state-copy` binary: see the library docs.

use std::io;
use std::process::ExitCode;

fn main() -> ExitCode {
    let (mut out, mut err) = (io::stdout(), io::stderr());
    let code = kbf_state_copy::main_with(std::env::args_os(), &mut out, &mut err);
    ExitCode::from(code)
}
