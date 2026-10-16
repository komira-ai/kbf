//! `kbf-gateway`: see the library docs, and `--help` of its two commands.

use std::process::ExitCode;

use clap::Parser;
use kbf_gateway::cli::{Cli, run};

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    match run(&Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-gateway: {e}");
            ExitCode::from(2)
        }
    }
}
