//! A node's phase as the farm takes it (`docs/design/worker-protocol.md#node-phases`):
//! fed to the scheduler before placement, a switching lease the scheduler does not
//! hold on the node cancelled, and a quarantine logged as an attention item.

use kbf_caps::NodeCaps;
use kbf_proto::worker::{Cancel, ServerMessage, server_message};
use kbf_sched::{Event, NodePhase};
use kbf_types::{FarmTime, Resources, WorkerId};

use super::{State, wire_lease};
use crate::fleet::quarantine_change;

/// What a `Hello` says about its node that the scheduler takes: the capacity placement
/// may book, the capabilities placement matches, and the node's phase. It also carries
/// the protocol version the node speaks, which the scheduler does not take: the farm
/// keeps it for `GET /v1/nodes`.
#[derive(Clone, Debug)]
pub struct Reported {
    /// What placement may book.
    pub capacity: Resources,
    /// What it offers.
    pub caps: NodeCaps,
    /// What it is doing besides running leases.
    pub phase: NodePhase,
    /// The `kbf.worker.v1` version the stream speaks, which the server sets to the one
    /// `Welcome` names, not the `Hello`'s when that is newer; `None` if none is known
    /// ([`crate::worker::named_protocol`]). Only the first `Hello` of a stream counts: it
    /// is the one `Welcome` answers, and a resent one is not negotiated again
    /// ([`crate::Farm::resize`]).
    pub protocol: Option<u32>,
}

impl Reported {
    /// A serving node with `capacity` and `caps`, whose protocol version is not named.
    #[must_use]
    pub const fn serving(capacity: Resources, caps: NodeCaps) -> Self {
        Self {
            capacity,
            caps,
            phase: NodePhase::Serving,
            protocol: None,
        }
    }
}

impl State {
    /// Takes `worker`'s phase, then places: sends a switching lease's `Start` again,
    /// or a `Cancel` for one not held here, and logs a quarantine raised or cleared.
    pub(super) fn phase(&mut self, now: FarmTime, worker: &WorkerId, phase: NodePhase) {
        let before = self.engine.sched.phase(worker).cloned().unwrap_or_default();
        if let Some((raise, line)) = quarantine_change(worker.as_str(), &before, &phase) {
            if raise {
                tracing::warn!(target: "kbf_server::attention", "{line}");
            } else {
                tracing::info!(target: "kbf_server::attention", "{line}");
            }
        }
        let switching = phase.switching();
        let event = Event::Phase {
            worker: worker.clone(),
            phase,
        };
        self.feed_quiet(now, event);
        let Some(lease) = switching else {
            return;
        };
        if self
            .engine
            .sched
            .not_held(worker, &[lease])
            .next()
            .is_some()
        {
            tracing::info!(%worker, %lease, "the node switches its session for a lease not held here: cancelled");
            let cancel = server_message::Message::Cancel(Cancel {
                lease_id: Some(wire_lease(lease)),
            });
            // A phase comes on the worker's current stream, so it has a link.
            let _ = self.side.links[worker].outbound.send(Ok(ServerMessage {
                message: Some(cancel),
            }));
        }
    }
}
