//! `kbf-updater`: roll a new kbf version over the hosts of a farm, one host at a time.
//!
//! This crate is the core, with no listener; the one adapter in it is the ssh installer:
//!
//! - [`api`]: the request, the roll as a poller sees it, the closed set of error codes;
//! - [`version`]: the one rule for a version, shared with the host-side wrapper;
//! - [`inventory`]: the root-owned host file, and the order a roll touches the hosts in;
//! - [`ports`]: safe-to-stop, install and query, health, and the clock, as traits;
//! - [`ssh_installer`]: the install port over ssh to the `kbf-host-install` forced command;
//! - [`record`]: `roll.json`, the durable record;
//! - [`Updater`]: the state machine over all of the above. One roll at a time, the frontend
//!   first, the first failure stops it, a restart asks the host before it installs again;
//! - [`fakes`]: a scripted fake of every port.
//!
//! The listener on a unix socket behind kbf-proxy, and the adapters that make the other ports
//! real (an HTTP client for safe-to-stop and health), come in later changes.

pub mod api;
pub mod fakes;
pub mod inventory;
pub mod ports;
pub mod record;
pub mod ssh_installer;
pub mod version;

mod updater;

pub use updater::{Ports, Started, Timing, Updater};

#[cfg(test)]
mod updater_restart_tests;
#[cfg(test)]
mod updater_tests;

/// The program's text for `--version`.
pub fn version_line() -> String {
    format!("kbf-updater {}", env!("CARGO_PKG_VERSION"))
}

/// The binary, as a function so that it is tested: the argument list in, the exit code and the
/// text out. No verb does anything yet; the listener is a later change.
pub fn run(args: &[String]) -> (u8, String) {
    match args.get(1).map(String::as_str) {
        Some("--version") => (0, format!("{}\n", version_line())),
        _ => (
            2,
            "usage: kbf-updater --version\nThe roll API listener is not part of this build.\n"
                .to_owned(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a binary that claims to serve: with no listener yet, anything but `--version`
    /// is a usage error with a non-zero exit.
    #[test]
    fn the_stub_answers_version_and_refuses_everything_else() {
        let args = |a: &[&str]| a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let (code, out) = run(&args(&["kbf-updater", "--version"]));
        assert_eq!((code, out), (0, format!("{}\n", version_line())));
        assert!(version_line().starts_with("kbf-updater 0."));
        for a in [&["kbf-updater"][..], &["kbf-updater", "serve"]] {
            let (code, out) = run(&args(a));
            assert_eq!(code, 2);
            assert!(out.contains("usage"));
        }
    }
}
