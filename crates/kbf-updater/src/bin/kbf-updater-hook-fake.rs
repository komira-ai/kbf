//! `kbf-updater-hook-fake`: `kbf update-hook` over a fake host, the program the conformance
//! harness runs (tests/hook_conformance.rs). Not shipped. Everything is in the library
//! (`kbf_updater::agent::hook::fake`), where it is tested and measured.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    kbf_updater::agent::hook::fake::main(&args)
}
