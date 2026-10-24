//! The `kbf-coordinator` binary: see [`kbf_coordinator::config`].

fn main() -> std::process::ExitCode {
    kbf_coordinator::config::main(std::env::args_os().collect())
}
