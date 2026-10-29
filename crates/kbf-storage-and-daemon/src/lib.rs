//! `kbf-storage-and-daemon`: the library behind `kbf storage-and-daemon` (the `kbf` binary,
//! crates/kbf-cli), the storage node and a worker daemon in one process. It still holds
//! the code of `kbf all-in-one`, a refused mode that goes away with it; that mode was
//! the farm server and a worker daemon in one process, for a one-node
//! Linux farm (the README says what it does today and what it does not do yet; the
//! design is kbf#657). The daemon's blobs move through calls into
//! the server's cache ([`LocalDataPlane`]), not over the network; its control plane
//! (`Hello`, leases, heartbeats, results) is the remote daemon's, over mutual TLS to the
//! server half's worker listener on loopback.
//!
//! [`run`] is the mode, after `kbf` has dispatched the daemon's helper re-execs, before
//! any flag is read (`kbf_node::dispatch_helpers`: a keeper or `__chown-dirs` run of the
//! binary never opens the server's directories or starts a runtime), and parsed one
//! command line holding both halves' flags ([`cli`]); it starts the server half, started as `kbf-server` starts
//! (`kbf_server::start`, on an async runtime of its own, sized as `kbf-server` sizes
//! one), its start line printed to stdout; then the daemon half, as `kbf-daemon` starts
//! (`kbf_node::start_with`, on its own runtime), with the in-process plane in place of
//! the transfer engine. SIGTERM or SIGINT drains both halves and abandons no lease
//! (`all_in_one` says in what order): the server half drains as `kbf-server` does, the
//! daemon half as `kbf-daemon` does once the server half has no execution open, and the
//! server half goes on serving the daemon half until the daemon half has drained; the
//! process exits 0 when both have. A second signal stops both at once. A server half that fails while serving stops
//! the daemon too, and the process exits 1. A start the server half refuses exits 2; one
//! that can pass on a later try (a directory the stopping process still holds) exits 75,
//! `EX_TEMPFAIL` (`kbf_server::StartFailure`); one the daemon half refuses exits as
//! `kbf-daemon` would.
//!
//! Linux only, the container driver only: elsewhere [`run`] exits 2 saying so.
//!
//! The crate also holds `kbf storage-and-daemon` ([`storage_and_daemon`]): the same
//! daemon half and the same plane, beside the storage node in place of the server.
//!
//! The scheduler's state is in Postgres, on the local socket `postgres:///kbf?host=/run/postgresql`
//! unless `--state-store` names another store ([`cli`]). `kbf-installer setup` creates the schema; the
//! start refuses a database without it, or with a version outside what this build supports.
//!
//! Not done yet (the README lists it): the in-process plane's admission; the `--reserve-cpus` split
//! between the halves; the whole-process
//! crash simulation, the driver-panic test and the rest of the helper-dispatch tests;
//! and the deploy shape, the ownership step and the unit.

#[cfg(target_os = "linux")]
mod all_in_one;
pub mod cli;
#[cfg(all(test, target_os = "linux"))]
mod entry_tests;
mod intake;
#[cfg(target_os = "linux")]
mod one_cache;
mod plane;
#[cfg(target_os = "linux")]
mod run;
pub mod storage_and_daemon;

pub use plane::LocalDataPlane;

use std::process::ExitCode;

/// The `kbf all-in-one` mode, on the command line `kbf` parsed (the daemon's helper
/// re-execs having been dispatched by the caller, first thing).
#[cfg(target_os = "linux")]
#[must_use]
pub fn run(flags: &cli::Flags) -> ExitCode {
    run::run(flags)
}

/// The `kbf all-in-one` mode off Linux: it refuses to run.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn run(_flags: &cli::Flags) -> ExitCode {
    eprintln!(
        "kbf all-in-one runs on Linux only, with the container driver; elsewhere run \
         kbf daemon and a frontend"
    );
    ExitCode::from(2)
}
