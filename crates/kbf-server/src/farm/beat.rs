//! A worker's heartbeat as the farm takes it: fed to the scheduler, its leases not held
//! here cancelled, and the callers of an operation it finished answered.

use kbf_proto::worker::{Cancel, ServerMessage, server_message};
use kbf_sched::Event;
use kbf_types::sched::usage::NodeLoad;
use kbf_types::{FarmTime, LeaseId, WorkerId};

use super::{Farm, Settled, State, StreamId, wire_lease};

impl Farm {
    /// Heartbeat `seq` on `stream`. Returns whether it was taken and is to be
    /// acknowledged: a heartbeat from a replaced stream is dropped, and one is not
    /// acknowledged (though the scheduler hears it) when this frontend no longer owns the
    /// worker, or has not written the worker's session row lately, so a daemon that the
    /// other frontends cannot see alive, or that has moved, fences on time. Each lease it
    /// lists that the scheduler no longer holds on `worker` is sent a `Cancel`. A lease it
    /// leaves out that a proven restart lost past its operation's farm reruns finishes the
    /// operation: its callers are answered (nothing is cached).
    ///
    /// The scheduler hears no load: see [`Farm::heartbeat_with_load`].
    pub async fn heartbeat(
        &self,
        worker: &WorkerId,
        stream: StreamId,
        seq: u64,
        running: Vec<LeaseId>,
    ) -> bool {
        self.heartbeat_with_load(worker, stream, seq, running, None)
            .await
    }

    /// [`Farm::heartbeat`] carrying the node load `load` the heartbeat's reading came to
    /// ([`Farm::note_node_load`]), which the scheduler is handed with the heartbeat, whatever
    /// `--placement-load` says. A heartbeat of a replaced stream feeds it nothing.
    pub async fn heartbeat_with_load(
        &self,
        worker: &WorkerId,
        stream: StreamId,
        seq: u64,
        running: Vec<LeaseId>,
        load: Option<NodeLoad>,
    ) -> bool {
        js_arm_async!(self, hot::heartbeat(worker, stream, seq, running, load));
        let now = self.now();
        let taken = self
            .lock()
            .take_heartbeat(now, worker, stream, seq, running, load);
        let Some((settled, acknowledged)) = taken else {
            return false;
        };
        for settled in settled {
            self.deliver(settled).await;
        }
        acknowledged
    }
}

impl State {
    /// [`Farm::heartbeat`] under the lock: `None` for a heartbeat from a replaced
    /// stream, else what its callers are told, for each operation it finished, and whether
    /// the heartbeat is acknowledged. Not generic, unlike `Farm::heartbeat`, so every test
    /// binary's runs count toward the same function's coverage.
    fn take_heartbeat(
        &mut self,
        now: FarmTime,
        worker: &WorkerId,
        stream: StreamId,
        seq: u64,
        running: Vec<LeaseId>,
        load: Option<NodeLoad>,
    ) -> Option<(Vec<Settled>, bool)> {
        let link = self
            .side
            .links
            .get_mut(worker)
            .filter(|l| l.stream == stream)?;
        link.newest_beat = link.newest_beat.max(seq);
        let outbound = link.outbound.clone();
        let event = Event::Heartbeat {
            worker: worker.clone(),
            running: running.clone(),
            load,
        };
        let answers = self.feed(now, event);
        // A node this frontend does not own is not kept alive in the store by it.
        let owns = self.engine.owns_worker(now, worker);
        if owns {
            self.engine.renew_session(now, worker);
        }
        for lease in self.engine.sched.not_held(worker, &running) {
            tracing::info!(%worker, %lease, "a lease not held here is listed: cancelled");
            let cancel = server_message::Message::Cancel(Cancel {
                lease_id: Some(wire_lease(lease)),
            });
            // A stream that has just ended drops it; the next heartbeat listing the
            // lease, on the next stream, sends it again.
            let _ = outbound.send(Ok(ServerMessage {
                message: Some(cancel),
            }));
        }
        let acknowledged = owns && self.engine.session_fresh(now, worker);
        let settled = answers.iter().map(|a| self.settle(a)).collect();
        Some((settled, acknowledged))
    }
}
