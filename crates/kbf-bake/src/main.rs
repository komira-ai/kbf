//! `kbf-bake`: see `kbf_bake::cli`.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args = std::env::args_os().collect();
    ExitCode::from(kbf_bake::cli::execute(
        args,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    ))
}
