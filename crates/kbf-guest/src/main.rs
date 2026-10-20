//! `kbf-guest`: runs one command per boot for the host, inside a VM guest.
//!
//! ```text
//! kbf-guest serve --unix-socket <path> --token-file <inputs>/kbf-guest.token \
//!   --inputs <inputs share> --outputs <outputs share>
//! kbf-guest session
//! ```
//!
//! `session` prints the launchd session type and exits; it is how CI checks what a
//! LaunchAgent and a LaunchDaemon see. Every subcommand refuses to run as root.

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use kbf_guest::server::{Agent, Config, HELLO_TIMEOUT};
use kbf_guest::session;
use kbf_guest::wire::TOKEN_LEN;

/// The agent inside a VM guest: runs one command per boot for the host.
#[derive(Debug, Parser)]
#[command(name = "kbf-guest", version)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Serve the host on a Unix socket.
    Serve {
        /// The socket to create; it must not exist.
        #[arg(long)]
        unix_socket: PathBuf,
        /// The per-boot token, 64 hex digits, written by the host.
        #[arg(long)]
        token_file: PathBuf,
        /// The inputs share; the command's working directory is inside it.
        #[arg(long)]
        inputs: PathBuf,
        /// The outputs share; the command's stdout and stderr go there.
        #[arg(long)]
        outputs: PathBuf,
    },
    /// Print the launchd session type (`Aqua`, `Background`, `System`; empty off
    /// macOS) and exit.
    Session,
}

fn main() -> ExitCode {
    run(Cli::parse(), session::refuse_root_here())
}

/// Runs the subcommand `cli` names, unless `root` is the refusal to run as root
/// ([`session::refuse_root_here`]): then it says why on stderr and exits 2, with
/// nothing run and nothing created.
fn run(cli: Cli, root: Result<(), String>) -> ExitCode {
    if let Err(e) = root {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    match cli.command {
        Cmd::Session => {
            println!("{}", session::manager_name());
            ExitCode::SUCCESS
        }
        Cmd::Serve {
            unix_socket,
            token_file,
            inputs,
            outputs,
        } => match serve(&unix_socket, &token_file, inputs, outputs) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("kbf-guest: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

fn serve(
    socket: &std::path::Path,
    token_file: &std::path::Path,
    inputs: PathBuf,
    outputs: PathBuf,
) -> Result<(), String> {
    let text = std::fs::read_to_string(token_file)
        .map_err(|e| format!("reading {}: {e}", token_file.display()))?;
    let token: [u8; TOKEN_LEN] = hex::decode(text.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| {
            format!(
                "{} does not hold {} hex digits",
                token_file.display(),
                TOKEN_LEN * 2
            )
        })?;
    let listener =
        UnixListener::bind(socket).map_err(|e| format!("binding {}: {e}", socket.display()))?;
    let mut agent = Agent::new(Config {
        token,
        inputs,
        outputs,
        session: session::manager_name(),
        hello_timeout: HELLO_TIMEOUT,
    });
    let served = agent.serve(&listener);
    served.map_err(|e| format!("accepting: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command line of `kbf-guest` with `args`.
    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("kbf-guest").chain(args.iter().copied()))
    }

    /// A `serve` whose token file does not exist, so that it fails (exit 1) if it runs
    /// at all.
    const SERVE_WITHOUT_A_TOKEN: [&str; 5] = [
        "serve",
        "--unix-socket=/nonexistent/kbf-guest.sock",
        "--token-file=/nonexistent/kbf-guest.token",
        "--inputs=/nonexistent/inputs",
        "--outputs=/nonexistent/outputs",
    ];

    /// Catches a root process that goes on to run its subcommand (mutants: the refusal
    /// dropped, or answered with another status): given the refusal, `session` and
    /// `serve` both exit 2, where without it `session` exits 0 and a `serve` with no
    /// token file exits 1, so the status shows the subcommand did not run.
    #[test]
    fn a_process_that_is_root_exits_2_before_running_its_subcommand() {
        let refusal = || Err("kbf-guest refuses to run as root".to_owned());
        let serve = &SERVE_WITHOUT_A_TOKEN;
        assert_eq!(run(cli(&["session"]), refusal()), ExitCode::from(2));
        assert_eq!(run(cli(serve), refusal()), ExitCode::from(2));
        assert_eq!(run(cli(&["session"]), Ok(())), ExitCode::SUCCESS);
        assert_eq!(run(cli(serve), Ok(())), ExitCode::FAILURE);
    }
}
