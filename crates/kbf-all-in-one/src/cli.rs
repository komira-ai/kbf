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

use std::collections::BTreeSet;

use clap::error::ErrorKind;
use clap::{ArgMatches, Command, CommandFactory as _, FromArgMatches as _};

/// What the binary is, for `--help`.
const ABOUT: &str = "The kbf farm server and a worker daemon in one process (Linux, the \
                     container driver only).";

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
    let refuse = |message: String| command().error(ErrorKind::ArgumentConflict, message);
    if matches.value_source("cas").is_some() {
        return Err(refuse(
            "--cas: the daemon half reads and writes blobs through calls into the server \
             half of this process, not over the network"
                .to_owned(),
        ));
    }
    let mut drivers = matches.get_raw("drivers").into_iter().flatten();
    if let Some(other) = drivers.find(|d| *d != "container") {
        return Err(refuse(format!(
            "--driver {}: kbf-all-in-one runs the container driver only; run kbf-daemon \
             for the others",
            other.to_string_lossy()
        )));
    }
    let daemon = kbf_node::Cli::from_arg_matches(matches)?;
    let mut server = kbf_server::Args::from_arg_matches(matches)?;
    // The thread flags matched are the daemon's.
    server.threads = kbf_threads::PoolArgs::default();
    Ok(Flags { server, daemon })
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
