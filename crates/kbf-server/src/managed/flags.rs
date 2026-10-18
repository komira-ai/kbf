//! The flags of a frontend that runs under the admin port.

use std::net::SocketAddr;

use clap::{Arg, ArgAction, ArgMatches};

/// The id of `--admin-listen`.
pub(crate) const LISTEN: &str = "admin_listen";
/// The id of `--await-takeover`.
pub(crate) const AWAIT: &str = "await_takeover";

/// `--admin-listen` and `--await-takeover`, parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdminFlags {
    /// Where the admin port listens: a loopback address.
    pub listen: SocketAddr,
    /// Start as a candidate: open the state store without taking the generation and
    /// serve nothing but readiness and capabilities until `POST takeover`.
    pub await_takeover: bool,
}

/// The two flags.
pub(crate) fn args() -> [Arg; 2] {
    [
        Arg::new(LISTEN)
            .long("admin-listen")
            .value_name("ADDR")
            .value_parser(clap::value_parser!(SocketAddr))
            .requires("state_store")
            .help(
                "Serve the admin calls (GET role, POST quiesce, resume, takeover, \
                 stand-down) on this loopback address. Anything that is not a loopback \
                 address is refused at start: the calls move the scheduler's role and carry \
                 no credential. Needs --state-store.",
            ),
        Arg::new(AWAIT)
            .long("await-takeover")
            .action(ArgAction::SetTrue)
            .requires(LISTEN)
            .conflicts_with("handover_socket")
            .help(
                "Open the state store without taking its generation, bump no epoch and run no \
                 scheduler, reconciler or worker listener: serve /readyz, the health service \
                 and GetCapabilities until `POST takeover` makes this process the writer. \
                 Needs --admin-listen. A SQLite state file is held by one process (an \
                 exclusive lock), so a candidate cannot run beside the process that holds \
                 it: side by side needs the Postgres store.",
            ),
    ]
}

/// The admin flags of `m`, or why they are refused.
///
/// # Errors
/// `--admin-listen` is not a loopback address.
pub(crate) fn parse(m: &ArgMatches) -> Result<Option<AdminFlags>, String> {
    let Some(listen) = m.get_one::<SocketAddr>(LISTEN).copied() else {
        return Ok(None);
    };
    if !crate::config::loopback_only(listen.ip()) {
        return Err(format!(
            "--admin-listen {listen} is not a loopback address: the admin calls carry no \
             credential and move the scheduler's role"
        ));
    }
    Ok(Some(AdminFlags {
        listen,
        await_takeover: m.get_flag(AWAIT),
    }))
}
