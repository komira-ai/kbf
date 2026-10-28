//! What a [`crate::Daemon`] session asks of the node it runs on, beyond leases: the
//! hooks of its loop that name more than the lease protocol (the storage map, the data
//! plane's counters, a driver's report, the Mac session's phase, the memory backstop).
//! [`crate::KbfExtras`] answers them for a kbf node; [`NoExtras`] answers none.

use std::future::Future;

use futures::channel::mpsc::UnboundedSender;
use kbf_proto::worker::{DaemonMessage, Heartbeat, NodeStatus, Welcome, server_message};
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
    /// The server's Welcome of a new stream was accepted: take what it carries for the
    /// node (the map, whether the frontend takes pack commits).
    fn on_welcome(&mut self, welcome: Welcome);

    /// A server message arrived; returns whether the node took it (the loop then does
    /// nothing with it).
    fn on_server_message(&mut self, message: &server_message::Message) -> bool;

    /// Sets what a Heartbeat carries beyond the lease protocol's fields.
    fn heartbeat_fields(&self, heartbeat: &mut Heartbeat);

    /// The `NodeStatus` this node sends now, for the node report `report`.
    fn node_status(&self, report: &NodeReport) -> NodeStatus;

    /// The phase every Hello carries; `draining` says whether the daemon drains.
    fn hello_phase(&self, draining: bool) -> NodePhase;

    /// A Welcome named lease epoch `epoch` (`None`: none): runs the node holds outside
    /// `core` that belong to another epoch are dropped here, before `core` drops its
    /// own.
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
/// own and no backstop.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoExtras;

impl SessionExtras for NoExtras {
    fn on_welcome(&mut self, _: Welcome) {}

    fn on_server_message(&mut self, _: &server_message::Message) -> bool {
        false
    }

    fn heartbeat_fields(&self, _: &mut Heartbeat) {}

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
