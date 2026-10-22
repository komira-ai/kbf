//! `kbf-updater`: roll a new kbf version over the hosts of a farm, one host at a time.
//!
//! The core:
//!
//! - [`api`]: the request, the roll as a poller sees it, the closed set of error codes;
//! - [`version`]: the one rule for a version, shared with the host-side wrapper;
//! - [`inventory`]: the root-owned host file, and the order a roll touches the hosts in;
//! - [`ports`]: safe-to-stop, install and query, health, and the clock, as traits;
//! - [`record`]: `roll.json`, the durable record;
//! - [`Updater`]: the state machine over all of the above. One roll at a time, the frontend
//!   first, the first failure stops it, a restart asks the host before it installs again;
//! - [`fakes`]: a scripted fake of every port.
//!
//! And around it:
//!
//! - [`server`] and [`http`]: the roll API on a unix socket behind kbf-proxy;
//! - [`safe_http`]: the real safe-to-stop, a call to the frontend's loopback operator API;
//! - [`not_built`]: the installer and health check this build does not have yet, which
//!   refuse, so that a roll fails at its first step with no host touched;
//! - [`daemon`]: the flags and the wiring of all of it.
//!
//! The ssh installer and the health check come in later changes.

pub mod api;
pub mod daemon;
pub mod fakes;
pub mod http;
pub mod inventory;
pub mod not_built;
pub mod ports;
pub mod record;
pub mod safe_http;
pub mod server;
pub mod version;

mod updater;

pub use updater::{Ports, Started, Timing, Updater};

#[cfg(test)]
mod daemon_tests;
#[cfg(test)]
mod safe_http_tests;
#[cfg(test)]
mod server_tests;
#[cfg(test)]
mod testutil;
#[cfg(test)]
mod updater_followup_tests;
#[cfg(test)]
mod updater_restart_tests;
#[cfg(test)]
mod updater_tests;

/// The program's text for `--version`.
pub fn version_line() -> String {
    format!("kbf-updater {}", env!("CARGO_PKG_VERSION"))
}

/// The binary, as a function so that it is tested: the argument list in, the exit code and the
/// text out. `--version` prints the version; the six flags of [`daemon`] start the updater,
/// call `ready` with a handle that stops it, and return when it is stopped. Anything else is
/// a usage error (exit 2); a start that fails is exit 1 with the reason.
pub fn run_with(args: &[String], ready: impl FnOnce(daemon::Stopper)) -> (u8, String) {
    if args.get(1).map(String::as_str) == Some("--version") {
        return (0, format!("{}\n", version_line()));
    }
    let cfg = match daemon::parse_flags(args.get(1..).unwrap_or_default()) {
        Ok(cfg) => cfg,
        Err(why) => return (2, format!("kbf-updater: {why}\n{}", daemon::USAGE)),
    };
    daemon::run_config(&cfg, ready)
}

/// [`run_with`], for the binary: nothing to do once started.
pub fn run(args: &[String]) -> (u8, String) {
    run_with(args, |_| {})
}
