//! The `tierstore` binary: its flags and its start are in `tierstore::server::cli`.

fn main() -> std::process::ExitCode {
    tierstore::server::cli::main(std::env::args_os().collect())
}
