//! A node another frontend holds, as the state store has it, for a read of the node on a
//! frontend it is not connected to ([`crate::farm::Farm::node_detail`]).
//!
//! An operator's drain, or a host updater's, may land on any frontend of the store: it writes
//! the node's `cordon/` row there, and the node's own frontend applies the row at its next
//! intake poll. The read that follows may land on any frontend too (a load balancer picks it).
//! This frontend's scheduler runs none of the node's work, so it answers from the store: the
//! node's placement is its `cordon/` row, and its leases are the `lease/` rows on it.

use kbf_types::Resources;

use super::*;
use crate::fleet::{NodeDetail, NodeView, PhaseView, PlacementView, ResourcesView};

impl State {
    /// `worker`, which another frontend holds, as the state store has it; `None` if the store
    /// cannot be read. A drain with no lease left reads as drained, as the node's own frontend
    /// has it; the store does not say whether a drain paused at its deadline. What the node
    /// reported it runs and can run is its own frontend's to say: none here.
    pub(in crate::farm) fn stored_detail(&self, worker: &WorkerId) -> Option<NodeDetail> {
        let mut read = None;
        self.durably("a node read", |d| {
            let cordon = d.cordons()?.into_iter().find(|(named, _)| named == worker);
            let leases: Vec<String> = (d.held_by(worker)?.into_iter())
                .filter_map(|op| op.lease)
                .map(|grant| grant.lease.to_string())
                .collect();
            read = Some((cordon.map(|(_, cordon)| cordon), leases));
            Ok(())
        });
        let (cordon, leases) = read?;
        let placement = match cordon {
            None => PlacementView::Serving,
            Some(StoredCordon::Cordoned) => PlacementView::Cordoned,
            Some(StoredCordon::Draining { .. }) if leases.is_empty() => PlacementView::Drained,
            Some(StoredCordon::Draining { deadline_unix_ms }) => PlacementView::Draining {
                deadline_unix_ms,
                leases: leases.clone(),
            },
        };
        let none = ResourcesView::of(Resources::default());
        let node = NodeView {
            node_id: worker.as_str().to_owned(),
            connected: false,
            protocol: None,
            software: None,
            needs_attention: Vec::new(),
            placement,
            phase: PhaseView::new(&Default::default()),
            lease_count: u32::try_from(leases.len()).unwrap_or(u32::MAX),
            booked: none,
        };
        Some(NodeDetail {
            node,
            capacity: none,
            free: none,
            leases,
        })
    }
}
