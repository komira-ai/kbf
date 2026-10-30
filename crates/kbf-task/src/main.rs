//! `kbf-task`: runs one build action and stores its pack; see the crate documentation for the
//! steps and `kbf_task::task` for the flags and the exit codes.

use clap::Parser as _;

fn main() -> std::process::ExitCode {
    // First, before any socket exists: the Command runs as this user and must not take them.
    kbf_task::task::run_cli_with(
        kbf_task::network::protect_this_process(),
        &kbf_task::task::Cli::parse(),
    )
}
