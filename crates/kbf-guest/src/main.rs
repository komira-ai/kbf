//! `kbf-guest`: runs one command per boot for the host, inside a VM guest.
//!
//! ```text
//! kbf-guest serve --unix-socket <path> --token-file <inputs>/kbf-guest.token \
//!   --inputs <inputs share> --outputs <outputs share>
//! kbf-guest session
//! ```
//!
//! `session` prints the launchd session type and exits; it is how CI checks what a
//! LaunchAgent and a LaunchDaemon see. Every subcommand refuses to run as root except
//! `probe-guest` and `probe-ready`, which record what each user can do in a guest
//! (src/probe.rs).

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use kbf_guest::probe;
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
    /// Write what this process can do in the guest (vsock, console ports, a virtiofs
    /// mount, the automount share) to a JSON file. Unlike the other subcommands this
    /// runs as any user, root included.
    ProbeGuest(probe::GuestArgs),
    /// Write a marker file once the guest has reached this point (the share mounted, a
    /// process running). Runs as any user, root included.
    ProbeReady(probe::ReadyArgs),
}

fn main() -> ExitCode {
    run(Cli::parse(), session::refuse_root_here())
}

/// Runs the subcommand `cli` names, unless `root` is the refusal to run as root
/// ([`session::refuse_root_here`]): then it says why on stderr and exits 2, with
/// nothing run and nothing created.
fn run(cli: Cli, root: Result<(), String>) -> ExitCode {
    run_with(cli, root, real_probe_host().as_deref())
}

/// [`run`] with the host the probe subcommands ask (`None`: not a macOS guest).
fn run_with(cli: Cli, root: Result<(), String>, host: Option<&dyn probe::Host>) -> ExitCode {
    if !matches!(cli.command, Cmd::ProbeGuest(_) | Cmd::ProbeReady(_))
        && let Err(e) = root
    {
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
        Cmd::ProbeGuest(args) => probe_cmd(host, &args.out, |h| probe::probe_guest(h, &args)),
        Cmd::ProbeReady(args) => probe_cmd(host, &args.out, |h| probe::probe_ready(h, &args)),
    }
}

/// The host the probes ask: the real guest on macOS, nothing elsewhere.
#[cfg(target_os = "macos")]
fn real_probe_host() -> Option<Box<dyn probe::Host>> {
    Some(Box::new(kbf_guest::probe_host::SystemHost))
}

#[cfg(not(target_os = "macos"))]
fn real_probe_host() -> Option<Box<dyn probe::Host>> {
    None
}

/// Runs a probe against `host` and writes its answer to `out`; `None` for the host means
/// this is not a macOS guest: refuse with 2.
fn probe_cmd(
    host: Option<&dyn probe::Host>,
    out: &std::path::Path,
    run: impl FnOnce(&dyn probe::Host) -> Result<serde_json::Value, String>,
) -> ExitCode {
    let Some(host) = host else {
        eprintln!("kbf-guest: the probe subcommands run inside a macOS guest");
        return ExitCode::from(2);
    };
    let written = run(host).and_then(|v| {
        probe::write_result(out, &v).map_err(|e| format!("write {}: {e}", out.display()))
    });
    match written {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kbf-guest: {e}");
            ExitCode::FAILURE
        }
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

    /// Catches a root process that goes on to run its subcommand (defects: the refusal
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

    fn h() -> H {
        H(std::cell::Cell::new(false))
    }

    /// A host that answers every question.
    struct H(std::cell::Cell<bool>);

    impl probe::Host for H {
        fn ids(&self) -> (u32, u32) {
            (1, 1)
        }
        fn session(&self) -> String {
            "System".to_owned()
        }
        fn exists(&self, _: &std::path::Path) -> bool {
            self.0.get()
        }
        fn process_running(&self, _: &str) -> bool {
            true
        }
        fn vsock(&self, _: u32) -> serde_json::Value {
            serde_json::json!(null)
        }
        fn console(&self, _: &str) -> serde_json::Value {
            serde_json::json!(null)
        }
        fn mount_tag(&self, _: &str, _: &std::path::Path) -> serde_json::Value {
            serde_json::json!(null)
        }
        fn automount(&self, _: &std::path::Path) -> serde_json::Value {
            serde_json::json!(null)
        }
        fn pause(&self, _: std::time::Duration) {
            self.0.set(true);
        }
    }

    /// Catches a probe whose answer is not written, and a failed probe or write that
    /// exits 0.
    #[test]
    fn probe_cmd_writes_the_answer_or_fails() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-guest-main-ok");
        let out = dir.join("r.json");
        let ok = |_: &dyn probe::Host| Ok(serde_json::json!({"k": 1}));
        assert_eq!(probe_cmd(Some(&h()), &out, ok), ExitCode::SUCCESS);
        assert!(out.exists());
        assert_eq!(
            probe_cmd(Some(&h()), &out, |_| Err("no".into())),
            ExitCode::FAILURE
        );
        assert_eq!(
            probe_cmd(Some(&h()), &dir.join("x/r.json"), ok),
            ExitCode::FAILURE
        );
        assert_eq!(probe_cmd(None, &out, ok), ExitCode::from(2));
    }

    /// Catches a probe subcommand refused as root (it must run as any user: the root
    /// control arm of the mount question is root), one that does not reach its probe, and
    /// one that runs where there is no guest.
    #[test]
    fn probes_run_as_root_and_write_their_answer_to_the_file_asked() {
        let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "kbf-guest-main-run");
        let refusal = || Err("kbf-guest refuses to run as root".to_owned());
        let guest = dir.join("g.json");
        let ready = dir.join("r.json");
        let g = [
            "probe-guest",
            "--out",
            guest.to_str().expect("p"),
            "--wait-path",
            "/w",
        ];
        assert_eq!(run_with(cli(&g), refusal(), Some(&h())), ExitCode::SUCCESS);
        let r = [
            "probe-ready",
            "--out",
            ready.to_str().expect("p"),
            "--wait-process",
            "p",
        ];
        assert_eq!(run_with(cli(&r), refusal(), Some(&h())), ExitCode::SUCCESS);
        let text = std::fs::read_to_string(&guest).expect("guest result");
        assert!(text.contains("\"uid\": 1"), "{text}");
        assert!(
            std::fs::read_to_string(&ready)
                .expect("ready result")
                .contains("\"session\"")
        );
        assert_eq!(run_with(cli(&r), refusal(), None), ExitCode::from(2));
    }
}
