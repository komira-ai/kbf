//! `kbf-installer`: parse the command line (`kbf_installer::cli`), run the verb
//! (`kbf_installer::app`), exit with the code the contract names (`kbf_installer::exit`).

use std::process::ExitCode;

use clap::Parser;
use kbf_installer::app::dispatch;
use kbf_installer::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let euid = rustix::process::geteuid().as_raw();
    dispatch(&cli, euid, &mut std::io::stdout(), &mut std::io::stderr()).into()
}
