//! `kbf-bench`: see the library docs and the README.
//!
//! Prints the report as a table. Exit status: 0 when every planned blob moved (`noop`:
//! every run succeeded), 1 when a request failed, a blob did not move or the deadline
//! passed (`noop`: a run failed; the report says which), 2 when the run could not start
//! (and for usage errors, as clap reports them).

use std::process::ExitCode;

use clap::Parser as _;
use kbf_bench::cli::{self, Cli};

#[tokio::main]
async fn main() -> ExitCode {
    match cli::run(&Cli::parse()).await {
        Ok(finished) => {
            print!("{}", finished.table());
            ExitCode::from(u8::from(!finished.complete()))
        }
        Err(e) => {
            eprintln!("kbf-bench: {e}");
            ExitCode::from(2)
        }
    }
}
