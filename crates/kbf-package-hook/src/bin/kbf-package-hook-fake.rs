//! `kbf-package-hook-fake`: `kbf package-hook` over a fake host, the program the conformance
//! harness runs (tests/conformance.rs). Not shipped. Everything is in the library
//! (`kbf_package_hook::fake`), where it is tested and measured.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    kbf_package_hook::fake::main(&args)
}
