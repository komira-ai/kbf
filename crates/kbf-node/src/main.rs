//! `kbf-daemon`: the worker daemon. A thin main over the `kbf_node` library, which
//! holds the flags, the driver set-up and the serve loop, so another binary can run
//! the daemon beside the server. Configuration is by flags only (`kbf-daemon --help`);
//! the flags and what each driver does are documented there.

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::Parser;
use kbf_node::{Cli, dispatch_helpers, exit_code, start};

fn main() -> ExitCode {
    if let Some(code) = dispatch_helpers() {
        return code;
    }
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    match start(&cli).and_then(kbf_node::NodeHandle::serve) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-daemon: {e}");
            exit_code(&e)
        }
    }
}
