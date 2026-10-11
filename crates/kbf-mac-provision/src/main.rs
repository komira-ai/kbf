//! `kbf-mac-provision`: see the library's documentation.

use clap::Parser as _;

fn main() -> std::process::ExitCode {
    kbf_mac_provision::cli::main(&kbf_mac_provision::cli::Cli::parse())
}
