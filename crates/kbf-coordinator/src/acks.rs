//! The failure reports an operator override acknowledged
//! (`docs/design/host-updates.md`, section 3.8, the shape of a halt's acks).
//!
//! A host keeps the report of its last failed update until it runs another, so when CI sets
//! by hand a version the coordinator had marked bad, the hosts that failed it still say so.
//! Counting those reports again would roll the override back at the next heartbeat. When the
//! mark is cleared ([`clear_bad`]) the reports standing at that moment are acked, one per host
//! with the version they are for, in the same write; the acks are in the ops file, so a restart
//! keeps them. An acked report does not count towards a rollback ([`is_acked`]). A host's ack
//! is dropped when the host reports something else ([`stale_acks`]) or is next ordered the
//! service ([`ordered`]), so a failure after the override counts as one.

use kbf_types::NodeId;
use kbf_updater::version::Version;
use kbf_updater::wire::Outcome;

use crate::fleet::Fleet;
use crate::ops::{HaltAck, OpsError, OpsStore, ServiceOps, Write};
use crate::reconcile::{CI, COORDINATOR, Change, entry, is_release_failure};

/// Whether the failure report of the host `id` for `version` is acked in `marks`.
pub(crate) fn is_acked(marks: &ServiceOps, id: &NodeId, version: &Version) -> bool {
    let name = id.to_string();
    marks
        .failure_acked
        .iter()
        .any(|ack| (&ack.target, &ack.node_id) == (version, &name))
}

/// The acks in `marks` whose host, as `service` last heard it, no longer reports the failure
/// the ack is for. A host the coordinator has not heard from keeps its ack.
pub(crate) fn stale_acks(service: &str, marks: &ServiceOps, fleet: &Fleet) -> Vec<Change> {
    marks
        .failure_acked
        .iter()
        .filter(|ack| {
            fleet.hosts_of(service).any(|(id, host)| {
                let same = host.beat.last_update.as_ref().is_some_and(|last| {
                    (last.outcome, &last.target) == (Outcome::Failed, &ack.target)
                });
                id.to_string() == ack.node_id && !same
            })
        })
        .map(|ack| Change::DropAck(ack.node_id.clone()))
        .collect()
}

/// Drops the ack of the host `node`, in memory first: dropping makes its failure count again,
/// which raises protection.
pub(crate) fn drop_ack(
    ops: &OpsStore,
    service: &str,
    node: &str,
    why: String,
) -> Result<(), OpsError> {
    let audit = entry(COORDINATOR, "drop_failure_ack", service, why);
    ops.change(Write::AfterMemory, audit, |state| {
        let marks = state.services.entry(service.to_owned()).or_default();
        marks.failure_acked.retain(|ack| ack.node_id != node);
    })
    .map(drop)
}

/// The host `node` was ordered `service`: its ack, if it has one, is dropped. A write that
/// fails is logged; the ack is gone in memory all the same.
pub fn ordered(ops: &OpsStore, service: &str, node: &NodeId) {
    let name = node.to_string();
    if !ops
        .marks(service)
        .failure_acked
        .iter()
        .any(|ack| ack.node_id == name)
    {
        return;
    }
    let why = format!("{name} was ordered");
    if let Err(why) = drop_ack(ops, service, &name, why) {
        tracing::error!(%service, node_id = %name, %why, "a failure ack was not dropped");
    }
}

/// Removes the mark that `version` is bad, if it has one: CI set it by hand. The failure
/// reports of `service` standing in `fleet` for `version` are acked in the same write. Written
/// to the file before it leaves memory (it clears protection), and before CI's desired state
/// is stored, so that a failed write leaves both as they were.
///
/// # Errors
/// [`OpsError`]: the ops file could not be written.
pub fn clear_bad(
    ops: &OpsStore,
    fleet: &Fleet,
    service: &str,
    version: &Version,
) -> Result<(), OpsError> {
    if !ops.marks(service).bad.contains(version) {
        return Ok(());
    }
    let acks: Vec<HaltAck> = fleet
        .hosts_of(service)
        .filter(|(_, host)| is_release_failure(host, version))
        .map(|(id, _)| HaltAck {
            node_id: id.to_string(),
            target: version.clone(),
        })
        .collect();
    let why = format!(
        "{version} was set by CI; {} failure reports acked",
        acks.len()
    );
    let audit = entry(CI, "unmark_bad", service, why);
    ops.change(Write::BeforeMemory, audit, |state| {
        let marks = state.services.entry(service.to_owned()).or_default();
        marks.bad.retain(|bad| bad != version);
        for ack in acks {
            marks.failure_acked.retain(|old| old.node_id != ack.node_id);
            marks.failure_acked.push(ack);
        }
    })
    .map(drop)
}
