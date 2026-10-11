//! `kbf-deploy`: see [`kbf_deploy::cli`].

fn main() -> std::process::ExitCode {
    kbf_deploy::cli::main(std::env::args_os().collect())
}
