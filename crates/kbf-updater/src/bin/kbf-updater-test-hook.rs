//! `kbf-updater-test-hook`: the hook of a fake service, the program the tests of the root part
//! declare. Not shipped. Everything is in the library (`kbf_updater::root::test_hook`), where it
//! is tested and measured.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let program = std::env::current_exe().ok();
    ExitCode::from(kbf_updater::root::test_hook::exit_code(
        &args,
        program.as_deref(),
        &mut std::io::stdout(),
    ))
}
