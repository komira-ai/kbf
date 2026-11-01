//! The failure reports an operator override acknowledged
//! (design kbf#2197, section 3.8, the shape of a halt's acks).
//!
//! A host keeps the report of its last failed update until it runs another, so when CI sets
//! by hand a version the coordinator had marked bad, the hosts that failed it still say so.
//! Counting those reports again would roll the override back at the next heartbeat. When the
//! mark is cleared ([`clear_bad`]) the reports standing at that moment are acked, one per host
//! with the whole report, in the same write; the acks are in the ops file, so a restart keeps
//! them. An ack matches one report: a report of the same host that differs in any part (the
//! target, phase, class or reason) is another report and counts ([`is_acked`]). A host's ack
//! is dropped when the host reports something else ([`stale_acks`]) and when the host is next
//! ordered the service ([`release_ack`]), so a failure after the override counts as one.

use kbf_types::NodeId;
use kbf_updater::version::Version;

use crate::fleet::{Fleet, Host};
use crate::ops::{FailureAck, OpsError, OpsStore, ServiceOps, Write};
use crate::reconcile::{CI, COORDINATOR, Change, entry, is_release_failure};

/// Whether the failure report the host `id` makes now is acked in `marks`: the same host and
/// the whole report.
pub(crate) fn is_acked(marks: &ServiceOps, id: &NodeId, host: &Host) -> bool {
    let name = id.to_string();
    host.beat.last_update.as_ref().is_some_and(|report| {
        marks
            .failure_acked
            .iter()
            .any(|ack| (&ack.node_id, &ack.report) == (&name, report))
    })
}

/// The acks in `marks` whose host, as `service` last heard it, no longer makes the report the
/// ack is for. A host the coordinator has not heard from keeps its ack.
pub(crate) fn stale_acks(service: &str, marks: &ServiceOps, fleet: &Fleet) -> Vec<Change> {
    marks
        .failure_acked
        .iter()
        .filter(|ack| {
            fleet.hosts_of(service).any(|(id, host)| {
                id.to_string() == ack.node_id && host.beat.last_update.as_ref() != Some(&ack.report)
            })
        })
        .map(|ack| Change::DropAck(ack.node_id.clone()))
        .collect()
}

/// Drops the ack of the host `node`, in the order `write` says.
pub(crate) fn drop_ack(
    ops: &OpsStore,
    service: &str,
    node: &str,
    why: String,
    write: Write,
) -> Result<(), OpsError> {
    let audit = entry(COORDINATOR, "drop_failure_ack", service, why);
    ops.change(write, audit, |state| {
        let marks = state.services.entry(service.to_owned()).or_default();
        marks.failure_acked.retain(|ack| ack.node_id != node);
    })
    .map(drop)
}

/// The host `node` is about to be ordered `service`: drops its ack, if it has one, durably (in
/// the file before memory) and answers whether the order may go out. When the write fails the
/// ack is still in the file and in memory, the failure is logged, and the answer is no: an
/// order is never given while its host's ack is on disk, so after a crash a failure that is
/// byte for byte the one acked cannot come back hidden.
pub fn release_ack(ops: &OpsStore, service: &str, node: &NodeId) -> bool {
    let name = node.to_string();
    if !ops
        .marks(service)
        .failure_acked
        .iter()
        .any(|ack| ack.node_id == name)
    {
        return true;
    }
    let why = format!("{name} is to be ordered");
    match drop_ack(ops, service, &name, why, Write::BeforeMemory) {
        Ok(()) => true,
        Err(why) => {
            tracing::error!(%service, node_id = %name, %why, "a failure ack was not dropped: no order");
            false
        }
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
    let acks: Vec<FailureAck> = fleet
        .hosts_of(service)
        .filter(|(_, host)| is_release_failure(host, version))
        .filter_map(|(id, host)| {
            host.beat.last_update.clone().map(|report| FailureAck {
                node_id: id.to_string(),
                report,
            })
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
