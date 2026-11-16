//! The farm's placement under `--executor js-agent`: no lease for a Linux daemon
//! (`docs/design/actions-as-jobs.md`, 8.2); and no lease for a Mac daemon under any executor
//! (section 14: a macOS action runs only as a job of the job service's Macs).
//!
//! While the frontend runs placed actions as jobs of the job service, the Linux nodes' daemons
//! stay connected beside `js-agent`, so that going back to `--executor kbf-daemon` is one line
//! and a restart; but the farm places nothing on them. A daemon whose node report says
//! `os=linux` is registered, and its resent `Hello`s taken, with no driver: the scheduler
//! places a lease kind only where a driver serves it, so it gets no lease of any kind, and the
//! session row written for it lists no driver either, so no frontend of the state store counts
//! it as able to run anything. Everything else about it is as before: it is heard from, its
//! node report and status are kept, and a lease it held already (taken up from the state
//! store after the restart into the executor) stays its own. A daemon that reports `os=macos`
//! is registered with no driver in the same way under either executor: the Macs run no
//! `kbf-daemon`, and one that connects is never given a lease. A daemon that reports any other
//! `os` or none is placed as before; the executor's virtual workers, which report `os=linux`
//! (its Mac workers `os=macos`), register and resend their reports through [`Farm::register_virtual`]
//! and [`Farm::resize_virtual`], which this filter does not touch.
//!
//! It is not a cordon: nothing is written to the state store but the session row every
//! registration writes, and an operator's cordon, drain or uncordon of the node is kept and
//! applied as always. It is read from the setting the farm was built with at each `Hello`, so
//! a frontend restarted under `--executor kbf-daemon` places the Linux daemons again with
//! nothing to lift.

use kbf_proto::worker::ServerMessage;
use kbf_sched::DaemonInstance;
use kbf_types::WorkerId;

use super::{Farm, Outbound, Reported, StreamId};
use crate::executor::ExecutorKind;

/// The `os` a Linux node's daemon reports.
const LINUX: &str = "linux";

/// The `os` a Mac's daemon reports.
const MACOS: &str = "macos";

impl Farm {
    /// This farm, placing for the executor `executor` (`--executor`): under
    /// [`ExecutorKind::JsAgent`] no Linux daemon is placed on (the module docs say how); under
    /// [`ExecutorKind::KbfDaemon`], the default, every daemon is.
    #[must_use]
    pub(crate) fn with_executor(mut self, executor: ExecutorKind) -> Self {
        self.settings.executor = executor;
        self
    }

    /// Registers the executor's virtual worker `worker` as [`Farm::register`] registers a
    /// daemon, with `reported` as it is: a virtual worker is where `--executor js-agent` places.
    pub(crate) fn register_virtual(
        &self,
        worker: &WorkerId,
        instance: DaemonInstance,
        predecessor: DaemonInstance,
        reported: Reported,
        outbound: Outbound,
        welcome: ServerMessage,
    ) -> StreamId {
        self.enroll(worker, instance, predecessor, reported, outbound, welcome)
    }

    /// Takes the resent report of the executor's virtual worker `worker` as [`Farm::resize`]
    /// takes a daemon's, with `reported` as it is.
    pub(crate) fn resize_virtual(&self, worker: &WorkerId, stream: StreamId, reported: Reported) {
        self.retake(worker, stream, reported);
    }

    /// What placement is told of the daemon `worker` that `reported`: a Mac daemon's report
    /// with no driver, and under `--executor js-agent` a Linux daemon's too; any other as it is.
    /// `first` is a stream's first `Hello`, whose skip is logged once.
    pub(super) fn placed(
        &self,
        worker: &WorkerId,
        mut reported: Reported,
        first: bool,
    ) -> Reported {
        let os = reported.caps.exact.get("os").map(String::as_str);
        let why = match os {
            Some(MACOS) => "a Mac daemon: macOS actions run only as jobs of the job service",
            Some(LINUX) if self.settings.executor == ExecutorKind::JsAgent => {
                "a Linux daemon under --executor js-agent"
            }
            _ => return reported,
        };
        if first {
            tracing::info!(%worker, "{why}: the farm places no lease on it");
        }
        reported.caps.drivers.clear();
        reported
    }
}

#[cfg(test)]
#[path = "linux_daemons_tests.rs"]
mod tests;
