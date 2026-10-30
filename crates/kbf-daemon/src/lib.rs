//! `kbf-daemon`: the worker daemon.
//!
//! It holds one outbound mutual-TLS `kbf.worker.v1` stream to a server, reports
//! its node (detected through `kbf-caps`), sends heartbeats, and runs a lease manager:
//! work starts only on `Start`, and only while the `Start` is inside the window it
//! names (issue #23); each lease reports one `Result`, kept until the server
//! acknowledges it; a lease the server cancels is killed; and every running lease is
//! killed and reported when no heartbeat has been acknowledged for the fence time T,
//! measured on a clock that counts the time the machine was suspended.
//! Execution goes through the [`Runtime`] trait. A runtime reads an action and its inputs from the front's CAS and writes the
//! outputs back through a [`Cas`]; `kbf_transfer::Transfers`, the process's transfer engine, is the one that talks to a
//! front.
//!
//! - [`config`]: command-line flags and TLS files.
//! - [`connect`]: which server to try next, and the wait between rounds of attempts;
//!   the daemon never stops trying.
//! - [`report`]: the node report and its hash.
//! - [`reach`]: the start-up check that the storage members answer a connect.
//! - [`status`]: the node's software status (OS, kernel, daemon, Xcodes).
//! - [`driver_watch`]: the driver's report channel, one part per source that feeds it.
//! - [`cpu_pool`]: [`CpuPool`], the threads (`--cpu-threads`) for work that keeps a core
//!   busy, such as hashing outputs.
//! - [`runtime`]: the runtime trait and [`FakeRuntime`], which runs nothing.
//! - [`runtimes`]: [`Runtimes`], the daemon's runtimes, at most one per lease kind.
//! - [`phases`]: where each lease's time went, logged when it ends (`lease phases:`)
//!   and sent as [`Event::LeaseEnded`].
//! - [`cas`]: the [`Cas`] trait (from `kbf-transfer`).
//! - [`packed`] and [`remote_index`]: with `--pack-read`, a data plane that reads blobs from
//!   the packs of the farm's object store first ([`PackedCas`]), finding them through the
//!   frontend's pack index ([`RemoteIndex`]); a blob not in a pack is read where it always was.
//! - `cache` (Linux): the node's input cache, which the container driver links input
//!   files from (`docs/design/worker-cache.md`).
//! - [`clock`]: the suspend-counting [`Clock`] the fence reads (issue #78).
//! - [`tree`]: writing an input root from the CAS and reading outputs back.
//! - [`usage`]: what an action used, measured by the kernel, and where it is carried.
//! - [`keeper`]: the keeper mode of the `kbf-daemon` binary, the process each run gets
//!   that ends the run when the daemon dies; the drivers start it.
//! - `uid` (macOS): every process of one uid, listed and killed; the native driver's
//!   exact kill of an action it runs as a pool user, and its keeper's.
//! - [`LocalRuntime`]: **tests only**, runs actions as plain child processes.
//! - [`Daemon`]: the session loop, with the contact clock and lease manager inside.
//! - [`LeaseCore`] (`core`): the lease state the session loop drives (leases, contact,
//!   start window, unacknowledged Results, epoch) and [`Event`], what it tells an observer.
//! - [`SessionExtras`] (`extras`): the hooks of the session loop that name more than
//!   leases; [`NoExtras`] answers none.
//! - [`KbfExtras`] (`kbf_extras`): a kbf node's answers (storage map, pack counters, driver
//!   report, session phase, memory backstop); the builders on `Daemon<KbfExtras>` set them.
//!
//! The `kbf-daemon` binary lives in the `kbf-node` crate: it picks its drivers
//! (`--driver fake|container|native`, once per runtime), and the drivers depend on this
//! crate, so a binary here naming them would be a cycle Cargo refuses. It never offers
//! [`LocalRuntime`], which isolates nothing.

mod acks;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod cache;
pub mod cas;
pub mod clock;
pub mod config;
pub mod connect;
mod contact;
mod core;
pub mod cpu_pool;
mod daemon;
mod drain;
pub mod driver_watch;
mod extras;
mod kbf_extras;
pub mod keeper;
mod lease;
mod lease_minutes;
mod lease_panic;
mod lease_peak;
#[cfg(target_os = "linux")]
mod local;
mod map;
#[cfg(target_os = "linux")]
pub mod numa;
pub mod packed;
pub mod phases;
mod plane;
pub mod pressure;
pub mod reach;
pub mod remote_index;
pub mod report;
pub mod runtime;
pub mod runtimes;
pub mod status;
pub mod tree;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod trees;
#[cfg(target_os = "macos")]
pub mod uid;
#[cfg(target_os = "linux")]
pub mod usage;
mod welcome;
mod window;

/// The daemon's version as it reports it (`daemon_version` in `Hello` and
/// `NodeStatus`, which `GET /v1/nodes` shows): the package version, `+`, and the
/// commit it was built from (12 hex digits), or `unknown` when the build had no git
/// checkout (issue #170). `build.rs` embeds the commit.
pub const DAEMON_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("KBF_BUILD_COMMIT"));

pub use crate::core::{Event, LeaseCore, superseded};
pub use cas::{Cas, CasError};
pub use clock::{Clock, Moment, SystemClock};
pub use config::{
    Args, DaemonConfig, FENCE_AFTER, KeepAlive, MAX_UNACKED, RECHECK_EVERY, TlsFiles,
};
pub use contact::{Acked, Contact};
pub use cpu_pool::CpuPool;
pub use daemon::{Daemon, PROTOCOL_VERSION, SessionError};
pub use driver_watch::{DriverPart, DriverWatch};
pub use extras::{ExtraWake, NoExtras, SessionExtras};
pub use kbf_extras::KbfExtras;
pub use kbf_mapsync::MapHolder;
pub use kbf_route::{DEFAULT_HOLD, Liveness};
#[cfg(target_os = "linux")]
pub use local::{LOCAL_DRIVER, LocalRuntime};
pub use map::{HeldMaps, REFETCH_WITHIN};
pub use packed::{
    DEFAULT_WRITE_INFLIGHT_BYTES, FallbackStats, PackCommit, PackCounter, PackReads, PackStats,
    PackWriter, PackedCas, WriteSettings, WriteStats,
};
pub use plane::{
    FarmCas, FarmLease, LocalMember, StorageNodes, member_cas, member_cas_with_hold, storage_nodes,
};
pub use remote_index::RemoteIndex;
pub use report::{Capacity, NodeReport};
pub use runtime::{
    FakeRuntime, LeaseReading, LeaseUsage, NodeLoadError, NodeReading, Runtime, RuntimeError,
    UPLOAD_DEADLINE, Work, upload_within,
};
pub use runtimes::{KindServedTwice, Runtimes};
pub use status::DriverReport;

// The unit tests run this test binary as kbf-testkit's `kbf-act` (src/usage.rs).
#[cfg(test)]
kbf_testkit::act_test_hook!();

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::DAEMON_VERSION;

    /// Catches (issue #170): a version that names the package version alone, so two
    /// builds of it read the same in `/v1/nodes`; and a commit that is not this
    /// checkout's (the build script fell back to `unknown`, or embedded another).
    #[test]
    fn the_version_names_the_commit_built() {
        let (package, commit) = DAEMON_VERSION.split_once('+').expect("a + in the version");
        assert_eq!(package, env!("CARGO_PKG_VERSION"));
        let head = Command::new("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .output()
            .expect("git runs");
        assert!(head.status.success(), "the tests run in a git checkout");
        let head = String::from_utf8(head.stdout).expect("UTF-8");
        assert_eq!(commit, head.trim());
        assert_eq!(commit.len(), 12, "{commit}");
        assert!(commit.bytes().all(|b| b.is_ascii_hexdigit()), "{commit}");
    }
}
