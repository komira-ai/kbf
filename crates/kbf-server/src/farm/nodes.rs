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
        js_arm!(self, views::nodes());
        let mut state = self.lock();
        state.side.note_closed_streams();
        let nodes = state
            .side
            .machine
            .nodes()
            .map(|worker| self.node(&state, worker))
            .collect();
        NodesView::of_this_build(nodes)
    }

    /// `worker` as `GET /v1/nodes` lists it, if it has registered.
    pub fn node_view(&self, worker: &WorkerId) -> Option<NodeView> {
        js_arm!(self, views::node_view(worker));
        let mut state = self.lock();
        state.side.note_closed_streams();
        state
            .side
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
    /// The node is another frontend's and the state store could not be read; or the store
    /// cannot say who holds the node, and it is not connected to this frontend.
    pub fn node_detail(&self, worker: &WorkerId) -> Result<Option<NodeDetail>, NodeUnread> {
        js_arm!(self, views::node_detail(worker));
        let now = self.now();
        let mut guard = self.lock();
        let state = &mut *guard;
        state.side.note_closed_streams();
        let cordons = state.engine.take_cordons(&mut state.side, now);
        let unread = || NodeUnread(worker.as_str().to_owned());
        let connected = state
            .side
            .machine
            .node(worker)
            .is_some_and(|node| node.connected);
        match state.engine.held_elsewhere(now, worker) {
            Some(true) => {
                let cordon = cordons.ok_or_else(unread)?.get(worker).copied();
                let stored = state.engine.stored_detail(worker, cordon);
                return stored.map(Some).ok_or_else(unread);
            }
            // The store cannot say who holds the node: only one connected here is answered.
            None if !connected => return Err(unread()),
            Some(false) | None => {}
        }
        if state.side.machine.node(worker).is_none() {
            return Ok(None);
        }
        let node = self.node(state, worker);
        let capacity = state.engine.sched.capacity(worker).unwrap_or_default();
        let booked = state.engine.sched.booked(worker).unwrap_or_default();
        let leases = state
            .engine
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
    /// ([`Side::note_closed_streams`](super::side::Side::note_closed_streams)).
    pub(super) fn node(&self, state: &State, worker: &WorkerId) -> NodeView {
        let (engine, side) = (&state.engine, &state.side);
        let wall = |at: FarmTime| self.epoch_unix_ms.saturating_add(at.as_millis());
        let held = engine.sched.leases_on(worker);
        let leases = || held.iter().map(ToString::to_string).collect();
        let placement = match engine.sched.cordon(worker) {
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
        let node = side.machine.node(worker);
        let software = node.and_then(|n| n.status.clone());
        let phase = engine.sched.phase(worker).cloned().unwrap_or_default();
        let connected = node.is_some_and(|n| n.connected);
        NodeView {
            node_id: worker.as_str().to_owned(),
            connected,
            protocol: side.links.get(worker).and_then(|link| link.protocol),
            needs_attention: needs_attention(software.as_ref(), &phase),
            software,
            placement,
            phase: PhaseView::new(&phase),
            lease_count: u32::try_from(held.len()).unwrap_or(u32::MAX),
            booked: ResourcesView::of(engine.sched.booked(worker).unwrap_or_default()),
        }
    }
}
