//! `tierstore-bench`: see the library docs and the README.

use std::process::ExitCode;

use clap::Parser as _;
use tierstore_bench::cli::{self, Cli};

#[tokio::main]
async fn main() -> ExitCode {
    match cli::run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tierstore-bench: {e}");
            ExitCode::from(2)
        }
    }
}
