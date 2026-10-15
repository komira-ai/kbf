//! The command line: every flag of `kbf-daemon` and of `kbf-server`, in one command.
//!
//! The daemon's flags are taken as they are. Each server flag is added beside them,
//! but for the ones the daemon already has: `--async-threads`, `--blocking-threads`
//! and `--cpu-threads`, which size the daemon half (with `--reserve-cpus`). The server
//! half sizes itself as `kbf-server` does with none of them given.
//!
//! Refused, though each half alone takes it: any `--driver` but `container` (the
//! native drivers run actions as the farm's own user, beside the server's state, and
//! belong to Macs, which run `kbf-daemon`); `--cas` (the daemon half reads and writes
//! blobs through calls into the server half, not over the network); the daemon's node
//! identity and dial, `--server`, `--ca-cert`, `--cert`, `--key` and
//! `--tls-server-name` (the daemon half opens its sessions through calls into the
//! server half: it dials no server and presents no certificate); and the server's
//! worker listener, `--worker-listen`, `--worker-tls-cert`, `--worker-tls-key`,
//! `--worker-client-ca` and `--worker-deny-list` (the process binds none: no daemon
//! but its own reaches the server half). Each refused flag is hidden from `--help`, and
//! given, it is a usage error that names it and says why.

use std::collections::BTreeSet;

use clap::error::ErrorKind;
use clap::parser::ValueSource;
use clap::{Arg, ArgAction, ArgMatches, Command, CommandFactory as _, FromArgMatches as _};

/// What the binary is, for `--help`.
const ABOUT: &str = "The kbf farm server and a worker daemon in one process (Linux, the \
                     container driver only).";

/// Why `--cas` is refused.
const BLOBS: &str = "the daemon half reads and writes blobs through calls into the server \
                     half of this process, not over the network";

/// Why the daemon's node identity and dial are refused.
const DIALS: &str = "the daemon half opens its sessions through calls into the server half of \
                     this process: it dials no server and presents no certificate";

/// Why the worker listener's flags are refused.
const LISTENS: &str = "kbf-all-in-one binds no worker listener, so no daemon but its own \
                       reaches the server half; run kbf-server for remote daemons";

/// The flags refused, by id: each one's name and why.
const REFUSED: [(&str, &str, &str); 11] = [
    ("cas", "cas", BLOBS),
    ("servers", "server", DIALS),
    ("ca_cert", "ca-cert", DIALS),
    ("cert", "cert", DIALS),
    ("key", "key", DIALS),
    ("tls_server_name", "tls-server-name", DIALS),
    ("worker_listen", "worker-listen", LISTENS),
    ("worker_tls_cert", "worker-tls-cert", LISTENS),
    ("worker_tls_key", "worker-tls-key", LISTENS),
    ("worker_client_ca", "worker-client-ca", LISTENS),
    ("worker_deny_list", "worker-deny-list", LISTENS),
];

/// The one command: the daemon's flags, then the server's that do not share a name with
/// one of them, the [`REFUSED`] ones among them hidden and taken only to be refused.
#[must_use]
pub fn command() -> Command {
    let daemon = kbf_node::Cli::command();
    let taken: BTreeSet<clap::Id> = daemon.get_arguments().map(|a| a.get_id().clone()).collect();
    let server: Vec<clap::Arg> = kbf_server::Args::command()
        .get_arguments()
        .filter(|a| !taken.contains(a.get_id()))
        .cloned()
        .collect();
    let command = daemon
        .name("kbf-all-in-one")
        .bin_name("kbf-all-in-one")
        .about(ABOUT)
        .args(server);
    REFUSED.into_iter().fold(command, |command, (id, long, _)| {
        command.mut_arg(id, |arg| refusable(arg, long))
    })
}

/// `arg` taken only to be refused: hidden, and never required. One with a default
/// keeps it (the half's flags must still parse); the others are made anew as `--long`
/// taking any number of values, so that none of their own rules (what each requires,
/// how often it may be given) answers before the refusal does.
fn refusable(arg: Arg, long: &'static str) -> Arg {
    if !arg.get_default_values().is_empty() {
        return arg.hide(true);
    }
    Arg::new(arg.get_id().clone())
        .long(long)
        .hide(true)
        .action(ArgAction::Append)
        .value_parser(arg.get_value_parser().clone())
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
/// A `--driver` other than `container`, or a [`REFUSED`] flag.
pub fn parse(matches: &ArgMatches) -> Result<Flags, clap::Error> {
    let refuse = |message: String| command().error(ErrorKind::ArgumentConflict, message);
    for (id, long, why) in REFUSED {
        if matches.value_source(id) == Some(ValueSource::CommandLine) {
            return Err(refuse(format!("--{long}: {why}")));
        }
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
