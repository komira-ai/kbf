//! The farm's view of its nodes for operators: `GET /v1/nodes` and `GET /v1/nodes/{id}`
//! ([`crate::api`], [`crate::fleet`]).

use kbf_sched::Cordon;
use kbf_types::{FarmTime, WorkerId};

use super::{Farm, NodeUnread, State};
use crate::fleet::{
    NodeDetail, NodeView, NodesView, PhaseView, PlacementView, ResourcesView, needs_attention,
};

impl Farm {
    /// This build, and every node registered since this farm started, in node-id order,
    /// with the oldest protocol version among them ([`NodesView::of_this_build`]).
    pub fn nodes(&self) -> NodesView {
        let mut state = self.lock();
        state.note_closed_streams();
        let nodes = state
            .machine
            .nodes()
            .map(|worker| self.node(&state, worker))
            .collect();
        NodesView::of_this_build(nodes)
    }

    /// `worker` as `GET /v1/nodes` lists it, if it has registered.
    pub fn node_view(&self, worker: &WorkerId) -> Option<NodeView> {
        let mut state = self.lock();
        state.note_closed_streams();
        state
            .machine
            .node(worker)
            .is_some()
            .then(|| self.node(&state, worker))
    }

    /// `worker` as `GET /v1/nodes/{id}` answers: its list entry, what it reported it can
    /// run, what is left of that, and its leases; `None` if neither this frontend nor the
    /// state store knows it.
    ///
    /// A drain or cordon may have been written through another frontend of the store, and
    /// the read may land on any frontend: the `cordon/` rows are taken up first, as an intake
    /// poll does, so the node's own frontend shows them at once; and a node another frontend
    /// holds is answered from the store (`State::stored_detail`), since this frontend's
    /// scheduler runs none of its work.
    ///
    /// # Errors
    /// The node is another frontend's, and the state store could not be read.
    pub fn node_detail(&self, worker: &WorkerId) -> Result<Option<NodeDetail>, NodeUnread> {
        let now = self.now();
        let mut state = self.lock();
        state.note_closed_streams();
        state.take_cordons(now);
        if !state.owns_worker(now, worker) && state.known_elsewhere(worker) {
            let stored = state.stored_detail(worker);
            return stored
                .map(Some)
                .ok_or_else(|| NodeUnread(worker.as_str().to_owned()));
        }
        if state.machine.node(worker).is_none() {
            return Ok(None);
        }
        let node = self.node(&state, worker);
        let capacity = state.sched.capacity(worker).unwrap_or_default();
        let booked = state.sched.booked(worker).unwrap_or_default();
        let leases = state
            .sched
            .leases_on(worker)
            .iter()
            .map(ToString::to_string)
            .collect();
        Ok(Some(NodeDetail {
            node,
            capacity: ResourcesView::of(capacity),
            free: ResourcesView::of(capacity.saturating_sub(booked)),
            leases,
        }))
    }

    /// One node's view; `worker` is registered, and every closed stream noted
    /// ([`State::note_closed_streams`]).
    pub(super) fn node(&self, state: &State, worker: &WorkerId) -> NodeView {
        let wall = |at: FarmTime| self.epoch_unix_ms.saturating_add(at.as_millis());
        let held = state.sched.leases_on(worker);
        let leases = || held.iter().map(ToString::to_string).collect();
        let placement = match state.sched.cordon(worker) {
            None => PlacementView::Serving,
            Some(Cordon::Cordoned) => PlacementView::Cordoned,
            Some(Cordon::Draining { deadline }) => PlacementView::Draining {
                deadline_unix_ms: wall(*deadline),
                leases: leases(),
            },
            Some(Cordon::Drained) => PlacementView::Drained,
            Some(Cordon::Paused { deadline }) => PlacementView::DrainPaused {
                deadline_unix_ms: wall(*deadline),
                leases: leases(),
            },
        };
        let node = state.machine.node(worker);
        let software = node.and_then(|n| n.status.clone());
        let phase = state.sched.phase(worker).cloned().unwrap_or_default();
        let connected = node.is_some_and(|n| n.connected);
        NodeView {
            node_id: worker.as_str().to_owned(),
            connected,
            protocol: state.links.get(worker).and_then(|link| link.protocol),
            needs_attention: needs_attention(software.as_ref(), &phase),
            software,
            placement,
            phase: PhaseView::new(&phase),
            lease_count: u32::try_from(held.len()).unwrap_or(u32::MAX),
            booked: ResourcesView::of(state.sched.booked(worker).unwrap_or_default()),
        }
    }
}
