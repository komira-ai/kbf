//! The command line: every flag of `kbf-daemon` and of `kbf-server`, in one command.
//!
//! The daemon's flags are taken as they are. Each server flag is added beside them,
//! but for the ones the daemon already has: `--async-threads`, `--blocking-threads`
//! and `--cpu-threads`, which size the daemon half (with `--reserve-cpus`). The server
//! half sizes itself as `kbf-server` does with none of them given.
//!
//! Refused, though each half alone takes it: any `--driver` but `container` (the
//! native drivers run actions as the farm's own user, beside the server's state, and
//! belong to Macs, which run `kbf-daemon`), and `--cas` (the daemon half reads and
//! writes blobs through calls into the server half, not over the network).
//!
//! The scheduler's state is Postgres (`docs/design/frontend-proxy.md`, section 11):
//! `--state-store` is given as `postgres:///kbf?host=/run/postgresql` (the form it is left
//! out for) or, until SQLite is deleted, as `sqlite:///absolute/path`.

use clap::error::ErrorKind;
use clap::{ArgMatches, Command, CommandFactory as _, FromArgMatches as _};
use std::collections::BTreeSet;

/// What the binary is, for `--help`.
const ABOUT: &str = "The kbf farm server and a worker daemon in one process (Linux, the \
                     container driver only).";

/// `--state-store` here, in place of the server's: this binary always has one.
const STATE_STORE_HELP: &str = "Where the scheduler keeps its state, written before each change \
    is acknowledged and replayed at start: `postgres:///kbf?host=/run/postgresql`, the database \
    `kbf` on a local Unix socket, reached as this process's own user (peer authentication; a \
    URL with a password is refused, and the server must flush commits before it acknowledges \
    them, `synchronous_commit` on, or the start is refused), or `sqlite:///absolute/path`, a \
    file on a local disk that this process owns (created, with its directory, if missing; the \
    file is 0600 and a directory it makes 0700). Every start is a new epoch, so the daemon \
    kills the runs of the leases it held before. Left out, it is \
    `postgres:///kbf?host=/run/postgresql`";

/// The one command: the daemon's flags, then the server's that do not share a name with
/// one of them.
#[must_use]
pub fn command() -> Command {
    let daemon = kbf_node::Cli::command();
    let taken: BTreeSet<clap::Id> = daemon.get_arguments().map(|a| a.get_id().clone()).collect();
    let server: Vec<clap::Arg> = kbf_server::Args::command()
        .get_arguments()
        .filter(|a| !taken.contains(a.get_id()))
        .cloned()
        .map(|a| match a.get_id().as_str() {
            "state_store" => a.help(STATE_STORE_HELP),
            _ => a,
        })
        .collect();
    daemon
        .name("kbf-all-in-one")
        .bin_name("kbf-all-in-one")
        .about(ABOUT)
        .args(server)
}

/// Both halves' flags, from one command line.
#[derive(Clone, Debug)]
pub struct Flags {
    /// The server half's.
    pub server: kbf_server::Args,
    /// The daemon half's.
    pub daemon: kbf_node::Cli,
}

/// Both halves' flags from what [`command`] matched.
///
/// # Errors
/// A `--driver` other than `container`, or `--cas`.
pub fn parse(matches: &ArgMatches) -> Result<Flags, clap::Error> {
    refuse_remote_plane(matches, "kbf-all-in-one", "server", command)?;
    let daemon = kbf_node::Cli::from_arg_matches(matches)?;
    let mut server = kbf_server::Args::from_arg_matches(matches)?;
    if server.state_store.is_none() {
        server.state_store = Some(kbf_server::StateStoreUri::local_postgres());
    }
    // The thread flags matched are the daemon's.
    server.threads = kbf_threads::PoolArgs::default();
    Ok(Flags { server, daemon })
}

/// Refuses, for a process whose daemon half reads and writes blobs through calls into
/// its `half` (`server` or `storage`), `--cas` and any `--driver` but `container`, the
/// error made by `name`'s command line `cmd`.
pub(crate) fn refuse_remote_plane(
    matches: &ArgMatches,
    name: &str,
    half: &str,
    cmd: fn() -> Command,
) -> Result<(), clap::Error> {
    let refuse = |message: String| cmd().error(ErrorKind::ArgumentConflict, message);
    if matches.value_source("cas").is_some() {
        return Err(refuse(format!(
            "--cas: the daemon half reads and writes blobs through calls into the {half} \
             half of this process, not over the network"
        )));
    }
    let mut drivers = matches.get_raw("drivers").into_iter().flatten();
    if let Some(other) = drivers.find(|d| *d != "container") {
        return Err(refuse(format!(
            "--driver {}: {name} runs the container driver only; run kbf-daemon for the \
             others",
            other.to_string_lossy()
        )));
    }
    Ok(())
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
