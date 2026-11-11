//! `kbf-task`: runs one build action and stores its pack; see the crate documentation for the
//! steps and `kbf_task::task` for the flags and the exit codes.

use clap::Parser as _;

fn main() -> std::process::ExitCode {
    // First, before any socket exists: the Command runs as this user and must not take them.
    let protection = kbf_task::network::protect_this_process();
    // `--sandbox-relay` and `--sandbox-init` (first argument) are the processes that start an
    // action's Command in its PID namespace; see `kbf_task::relay`.
    #[cfg(target_os = "linux")]
    if kbf_task::relay::selected(std::env::args_os().nth(1).as_deref()) {
        return kbf_task::relay::main_with(&std::env::args_os().skip(1).collect::<Vec<_>>());
    }
    // `--probe-confinement` (first argument) is the confinement probe, not a task; see
    // `kbf_task::probe`.
    if kbf_task::probe::selected(std::env::args_os().nth(1).as_deref()) {
        return kbf_task::probe::run_cli_with(
            protection,
            &kbf_task::probe::ProbeCli::parse_from(std::env::args_os()),
        );
    }
    kbf_task::task::run_cli_with(protection, &kbf_task::task::Cli::parse())
}
