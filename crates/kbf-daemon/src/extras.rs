//! What a [`crate::Daemon`] session asks of the node it runs on, beyond leases: the
//! hooks of its loop that name more than the lease protocol (the storage map, the data
//! plane's counters, a driver's report, the Mac session's phase, the memory backstop).
//! [`crate::KbfExtras`] answers them for a kbf node; [`NoExtras`] answers none.

use std::future::Future;

use futures::channel::mpsc::UnboundedSender;
use kbf_proto::worker::{
    DaemonMessage, NodeStatus, PackReadCounts, PackWriteCounts, Welcome, server_message,
};
use kbf_types::NodePhase;

use crate::core::LeaseCore;
use crate::report::NodeReport;

/// What woke the session loop that is not the stream, a heartbeat, a run, the recheck or
/// the drain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtraWake {
    /// The driver's report changed.
    Driver,
    /// The runtime's phase changed.
    Phase,
    /// The memory pressure reading changed.
    Pressure,
    /// The storage map's holder waits for a generation it does not have.
    MapAsked,
}

/// The hooks of the session loop a node answers. Each is called from the loop at the
/// place its name says; none may block.
pub trait SessionExtras: Send {
    /// The server's Welcome of a new stream passed the version and interval checks;
    /// called before the stream counts as welcomed (before contact, the start window,
    /// the fence recheck and the epoch change), so the node takes from it what must be
    /// in place first (whether the frontend takes pack commits).
    fn welcome_checked(&mut self, welcome: &Welcome);

    /// The Welcome of a new stream was taken, after the epoch change: take what else it
    /// carries for the node (the map).
    fn on_welcome(&mut self, welcome: Welcome);

    /// A server message arrived; returns whether the node took it (the loop then does
    /// nothing with it).
    fn on_server_message(&mut self, message: &server_message::Message) -> bool;

    /// The pack read and write counts a Heartbeat carries, as they stand now.
    fn pack_counts(&self) -> (Option<PackReadCounts>, Option<PackWriteCounts>);

    /// The `NodeStatus` this node sends now, for the node report `report`.
    fn node_status(&self, report: &NodeReport) -> NodeStatus;

    /// The phase every Hello carries; `draining` says whether the daemon drains.
    fn hello_phase(&self, draining: bool) -> NodePhase;

    /// A Welcome named lease epoch `epoch` (`None`: none): runs the node holds outside
    /// `core` that belong to another epoch are dropped here, before `core` drops its
    /// own. The session calls this before `core`'s epoch change, not after: the hook sees
    /// `core` as the Welcome found it (the old epoch, every grant still in place), and
    /// the core then drops what it granted under that epoch. Kills are by lease id and
    /// repeat harmlessly, so today either order drops the same leases; an implementation
    /// that reads `core` must keep this order.
    fn epoch_changed(&mut self, epoch: Option<u64>, core: &mut LeaseCore);

    /// Completes when the node has something to take (an [`ExtraWake`]); never
    /// otherwise. Cancel-safe: the loop drops it whenever another arm wins.
    fn wake(&mut self) -> impl Future<Output = ExtraWake> + Send;

    /// Called after every wake of the loop, whatever its cause; may send on `tx`.
    fn after_wake(&mut self, tx: &UnboundedSender<DaemonMessage>);

    /// Takes the driver's newest report into `report` (the node report `base` plus the
    /// driver's entries); returns whether `report` changed.
    fn take_driver(&mut self, base: &NodeReport, report: &mut NodeReport) -> bool;

    /// Takes the runtime's newest phase; returns whether it changed.
    fn take_phase(&mut self) -> bool;

    /// Takes the newest pressure reading, given the number of leases running; returns
    /// whether the phase a Hello carries changed.
    fn take_pressure(&mut self, running: usize, draining: bool) -> bool;
}

/// A node with nothing but leases: no map, no counters, no driver report, no phase of its
/// own and no backstop. Its `NodeStatus` is empty: the status a kbf node sends (software,
/// Xcodes, the daemon's build) is kbf-specific, built from [`crate::status::Software`] and
/// the build's commit.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoExtras;

impl SessionExtras for NoExtras {
    fn welcome_checked(&mut self, _: &Welcome) {}

    fn on_welcome(&mut self, _: Welcome) {}

    fn on_server_message(&mut self, _: &server_message::Message) -> bool {
        false
    }

    fn pack_counts(&self) -> (Option<PackReadCounts>, Option<PackWriteCounts>) {
        (None, None)
    }

    fn node_status(&self, _: &NodeReport) -> NodeStatus {
        NodeStatus::default()
    }

    fn hello_phase(&self, draining: bool) -> NodePhase {
        if draining {
            NodePhase::Draining
        } else {
            NodePhase::Serving
        }
    }

    fn epoch_changed(&mut self, _: Option<u64>, _: &mut LeaseCore) {}

    fn wake(&mut self) -> impl Future<Output = ExtraWake> + Send {
        std::future::pending()
    }

    fn after_wake(&mut self, _: &UnboundedSender<DaemonMessage>) {}

    fn take_driver(&mut self, _: &NodeReport, _: &mut NodeReport) -> bool {
        false
    }

    fn take_phase(&mut self) -> bool {
        false
    }

    fn take_pressure(&mut self, _: usize, _: bool) -> bool {
        false
    }
}
