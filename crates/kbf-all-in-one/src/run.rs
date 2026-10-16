//! The two halves started and run together (the crate docs say how), for
//! `kbf all-in-one` (the server half) and `kbf storage-and-daemon` (the storage half,
//! `crate::storage_and_daemon`).

use std::fmt;
use std::future::Future;
use std::io::IsTerminal as _;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;

use kbf_server::ServeError;
use kbf_threads::Topology;
use tokio::runtime::Runtime;

use crate::LocalDataPlane;
use crate::cli::{Flags, command, parse};

/// Parses the command line, starts both halves and serves until the server half stops.
pub(crate) fn main() -> ExitCode {
    let matches = command().get_matches();
    let flags = parse(&matches).unwrap_or_else(|e| e.exit());
    run(&flags)
}

/// Starts both halves on the parsed flags and serves until the server half stops.
pub(crate) fn run(flags: &Flags) -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    exit("kbf-all-in-one", serve(flags))
}

/// The exit status of a run that ended as `served` did, its failure said on stderr
/// after `name`.
pub(crate) fn exit(name: &str, served: Result<(), Failed>) -> ExitCode {
    match served {
        Ok(()) => ExitCode::SUCCESS,
        Err(failed) => {
            eprintln!("{name}: {failed}");
            failed.code()
        }
    }
}

/// Why the process stopped other than on a signal. `half` names the half beside the
/// daemon: `server` or `storage`.
pub(crate) enum Failed {
    /// That half refused to start.
    Server(&'static str, Box<dyn std::error::Error>),
    /// The daemon half refused to start.
    Daemon(kbf_node::Error),
    /// That half stopped serving on its own.
    Stopped(&'static str, String),
}

impl Failed {
    pub(crate) fn server(half: &'static str, e: impl Into<Box<dyn std::error::Error>>) -> Self {
        Self::Server(half, e.into())
    }

    fn stopped(half: &'static str, e: impl fmt::Display) -> Self {
        Self::Stopped(half, e.to_string())
    }

    /// The exit status: 2 for a start the server refused, as `kbf-server` exits; the
    /// daemon's own for a start it refused; 1 for a server half that stopped.
    fn code(&self) -> ExitCode {
        match self {
            Self::Server(..) => ExitCode::from(2),
            Self::Daemon(e) => kbf_node::exit_code(e),
            Self::Stopped(..) => ExitCode::FAILURE,
        }
    }
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Server(half, e) => write!(f, "{half}: {e}"),
            Self::Daemon(e) => write!(f, "daemon: {e}"),
            Self::Stopped(half, e) => write!(f, "the {half} half stopped: {e}"),
        }
    }
}

/// The server half on a runtime of its own, then the daemon half over the in-process
/// plane, the two given one cache when they share a disk (`crate::one_cache`); returns
/// once the server half has stopped and the daemon half after it.
fn serve(flags: &Flags) -> Result<(), Failed> {
    let mut flags = flags.clone();
    crate::one_cache::share(&mut flags.server, &mut flags.daemon);
    let sizes = kbf_server::sizes_on(&flags.server, Topology::detect());
    sizes.log();
    let tokio = sizes.runtime().map_err(|e| Failed::server("server", e))?;
    let started = tokio.block_on(kbf_server::start(&flags.server, kbf_server::interrupted));
    let server = started.map_err(|e| Failed::server("server", e))?;
    println!("{}", server.start_line());
    let plane = LocalDataPlane::new(server.local(), tokio.handle().clone());
    let serving = Box::pin(server.serve());
    with_daemon(&tokio, "server", serving, plane, &flags.daemon)
}

/// What the half beside the daemon serves until it stops.
pub(crate) type Serving = Pin<Box<dyn Future<Output = Result<(), ServeError>> + Send>>;

/// The daemon half of `daemon` over `plane`, beside the half `half` (`server` or
/// `storage`) serving on `tokio`; returns once that half has stopped and the daemon
/// half after it. Not generic, so both modes run one copy of it.
pub(crate) fn with_daemon(
    tokio: &Runtime,
    half: &'static str,
    serving: Serving,
    plane: LocalDataPlane,
    daemon: &kbf_node::Cli,
) -> Result<(), Failed> {
    let plane = Arc::new(plane);
    // Dropped when the other half stops serving, which stops the daemon half.
    let (ended, other_ended) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio.spawn(async move {
        let _ended = ended;
        serving.await
    });
    let node = kbf_node::start_with(daemon, move |_| Ok(Arc::clone(&plane)));
    let node = node.map_err(Failed::Daemon)?;
    node.serve_until(async move {
        let _ = other_ended.await;
    });
    let served = tokio.block_on(serving);
    let served = served.map_err(|e| Failed::stopped(half, e))?;
    served.map_err(|e| Failed::stopped(half, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a failure that exits with another status than its half's (a refused
    /// server start must exit 2, as `kbf-server`'s does, and a server half that stops
    /// serving must not exit 0), or that says nothing of which half failed.
    #[test]
    fn each_failure_exits_with_its_halfs_status() {
        let stopped = Failed::stopped("server", "listener gone");
        assert_eq!(
            stopped.to_string(),
            "the server half stopped: listener gone"
        );
        assert_eq!(stopped.code(), ExitCode::FAILURE);
        let stopped = Failed::stopped("storage", "listener gone");
        assert_eq!(
            stopped.to_string(),
            "the storage half stopped: listener gone"
        );
        let server = Failed::server("server", "no store");
        assert_eq!(server.to_string(), "server: no store");
        assert_eq!(server.code(), ExitCode::from(2));
        let storage = Failed::server("storage", "no store");
        assert_eq!(storage.to_string(), "storage: no store");
        assert_eq!(storage.code(), ExitCode::from(2));
        let daemon = Failed::Daemon("no scratch".into());
        assert_eq!(daemon.to_string(), "daemon: no scratch");
        assert_eq!(daemon.code(), ExitCode::FAILURE);
    }
}
