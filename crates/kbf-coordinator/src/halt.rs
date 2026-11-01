//! Halt: a failure no version can repair (`docs/design/host-updates.md`, section 3.8).
//!
//! A host whose last update to a service `Failed` in the `halt` phase has found something a
//! new release cannot fix (a tainted disk, a state no hook can read). The coordinator raises a
//! halt for the service: every host and role of it is held ([`Hold::Halted`]), a new desired
//! version does not lift it, the convergence says `halted`, and nothing retries or rolls back.
//! The coordinator has no pager: the halt is logged at error with the field `alert = "halt"`,
//! which is what an operator alert watches.
//!
//! The halt is kept in the ops store ([`crate::ops`]). Raising it is written *after* it is in
//! memory, so a halt that could not be written still stops the service; clearing it, which
//! only an operator does (`clear_halt` on `/v1/operator`), is written *first*, so a clear
//! that could not be written leaves it standing.
//!
//! The halted host keeps reporting the same `last_update` until it updates again, so a clear
//! also records `(node, target)` as acknowledged, and a report for an acknowledged pair is
//! not raised. A clear clears the service: every other halt report received while the halt
//! stood (`halt_seen`, kept with the halt) is acknowledged with it, so the coordinator and the
//! agents, which lift on the generation, agree. A `last_update` has no attempt number or
//! finish stamp, so a report is identified by the host and its whole `last_update` for as long as
//! the host keeps reporting it: a different `last_update` (another target, reason or class, or no
//! halt at all) ends the host's acknowledgement ([`end_ack`]), and a halt reported after that is
//! new. Design kbf#2197 section 3.8 also ends it when the host is ordered the acknowledged
//! target, and leaves it standing for an order of another target. That order cannot be given
//! while the acknowledgement stands: the only place an order is produced is
//! `Coordinator::reply_to` (`service.rs`, the call of `Fleet::order`), after [`decide`], and
//! `decide_marked` holds a host whose beat's `last_update` is a failure of the desired version
//! (`Hold::FailedHere`, `decide.rs`) and one that is updating (`Hold::Updating`). The
//! acknowledgement stands only while the host's beat carries the acknowledged report, a failed
//! update of its target, and any other beat ends it in [`note`], before [`decide`] and
//! durably, with no order for the service if the write fails. No operator verb lifts
//! `FailedHere`, and a change of the desired state does not change what the host reports.
//! A service the host is updating is not read: its `last_update` is the one before.
//!
//! Each clear raises the service's generation by one. A reply to an agent that
//! announced [`FEATURE`] carries the generation of every service that has one (`halts`): the
//! agent lifts its hold when it sees a number newer than the one it halted under, so a lost
//! heartbeat cannot make it miss a clear, and a restart of the coordinator, which reads the
//! number from the ops file, does not forget it.
//!
//! A report is ignored when its host is not a member of the service (the service is not
//! defined, or does not run on the host's role), when an operator excluded the host from it,
//! when an operator cleared exactly that report, and when the service is halted already: the
//! first halt stands until it is cleared (the convergence names it).

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::NodeId;
use kbf_updater::wire::{Heartbeat, Outcome, Phase, Token, UpdateResult};

use crate::decide::{Decision, Hold, decide_marked};
use crate::desired::{Desired, KBF};
use crate::fleet::Fleet;
use crate::identity::Identity;
use crate::ops::{AuditEntry, Halt, HaltAck, OpsState, OpsStore, Write};
use crate::services::Services;

/// The feature an agent announces to be told the clear generations.
pub const FEATURE: &str = "halt-v1";

/// Who a halt is logged as: the coordinator, on a host's report.
pub const COORDINATOR: &str = "coordinator";

/// The check of a halt whose report named none.
pub const UNSPECIFIED: &str = "unspecified";

/// The halt `last` reports for the node `id`, if it reports one: a failed update in the
/// `halt` phase. The check is the report's reason.
fn reported(last: Option<&UpdateResult>, id: &NodeId) -> Option<Halt> {
    let last = last?;
    matches!(
        (last.outcome, last.phase),
        (Outcome::Failed, Some(Phase::Halt))
    )
    .then(|| Halt {
        node_id: id.to_string(),
        check: last
            .reason
            .as_ref()
            .map_or(UNSPECIFIED, Token::as_str)
            .to_owned(),
        target: last.target.clone(),
        report: Some(last.clone()),
    })
}

/// Whether the service `service` is defined and runs on hosts of `role`.
pub(crate) fn is_member(services: &Services, service: &str, role: &str) -> bool {
    services
        .get(service)
        .is_some_and(|def| def.roles.contains_key(role))
}

/// Why `halt`, reported for `service`, is not raised.
enum Ignored {
    /// An operator excluded the host from the service.
    Excluded,
    /// An operator cleared this report.
    Acked,
    /// The service is halted already: the report is kept for the clear ([`record`]).
    Standing,
}

/// Why `halt`, reported for `service`, is not raised: the first of the reasons in the module
/// documentation that holds, or `None` when it is.
fn ignored(state: &OpsState, service: &str, halt: &Halt) -> Option<Ignored> {
    let marks = state.services.get(service);
    let excluded = state
        .excluded
        .iter()
        .any(|e| (e.service.as_str(), e.node_id.as_str()) == (service, halt.node_id.as_str()));
    if excluded {
        return Some(Ignored::Excluded);
    }
    let acked = marks.is_some_and(|m| {
        m.halt_acked.iter().any(|a| {
            halt.report
                .as_ref()
                .is_some_and(|r| a.matches(&halt.node_id, r))
        })
    });
    if acked {
        return Some(Ignored::Acked);
    }
    if marks.is_some_and(|m| m.halt.is_some()) {
        return Some(Ignored::Standing);
    }
    None
}

/// Raises the halts the heartbeat `beat` of the host `who` reports, for `kbf` (its top-level
/// facts) and for each service it lists. Before the heartbeat is decided, so the host that
/// reports a halt is ordered nothing in the same reply.
///
/// A service whose update the host is running is skipped: its `last_update` is the one before,
/// not a report. Otherwise a `last_update` that is not the halt an operator cleared for the
/// host ends that acknowledgement ([`end_ack`]), and one that is a halt is raised unless it is
/// ignored. Answers the services for which an acknowledgement should have ended and could not
/// be written: the host is ordered nothing of them in this reply.
pub(crate) fn note(
    ops: &OpsStore,
    services: &Services,
    who: &Identity,
    beat: &Heartbeat,
) -> Vec<String> {
    let listed = beat.services.as_deref().unwrap_or_default();
    let top = std::iter::once((KBF, beat.last_update.as_ref(), beat.updating.is_some()));
    let reports = top.chain(listed.iter().map(|one| {
        (
            one.name.as_str(),
            one.last_update.as_ref(),
            one.updating.is_some(),
        )
    }));
    let mut held = Vec::new();
    for (service, last, updating) in reports {
        if updating || !is_member(services, service, who.role.name()) {
            continue;
        }
        let report = reported(last, &who.node_id);
        if !end_ack(ops, service, &who.node_id, report.as_ref()) {
            held.push(service.to_owned());
        }
        if let Some(halt) = report {
            raise(ops, service, halt);
        }
    }
    held
}

/// Ends the acknowledgement the host `id` has for `service`, unless `report`, the halt the
/// host reports now, is the very one acknowledged: a different `last_update` (another target,
/// reason or class, or no halt at all) is a different report, so a halt the host reports from
/// then on is a new failure, even of the same target. Written before it is in memory: it
/// removes protection, and one that stays on disk would swallow the new failure after a
/// restart. Answers whether it holds: `false` when the write failed and the acknowledgement
/// stands, so no order may go out before it is gone.
fn end_ack(ops: &OpsStore, service: &str, id: &NodeId, report: Option<&Halt>) -> bool {
    let node = id.to_string();
    let report = report.and_then(|halt| halt.report.clone());
    let differs = move |ack: &HaltAck| {
        ack.node_id == node && !report.as_ref().is_some_and(|r| ack.matches(&node, r))
    };
    let held = ops.read(|state| {
        state
            .services
            .get(service)
            .is_some_and(|m| m.halt_acked.iter().any(&differs))
    });
    if !held {
        return true;
    }
    let mut entry = AuditEntry::new(COORDINATOR, "halt_ack_ended");
    entry.service = Some(service.to_owned());
    entry.node_id = Some(id.to_string());
    let name = service.to_owned();
    let ended = ops.change(Write::BeforeMemory, entry, |state| {
        let marks = state.services.entry(name).or_default();
        marks.halt_acked.retain(|ack| !differs(ack));
    });
    if let Err(why) = &ended {
        tracing::error!(service, %why, "the end of an acknowledgement could not be written: no order for the host");
    }
    ended.is_ok()
}

/// Raises `halt` for `service` unless it is ignored: in memory first, then written. A failed
/// write is logged and the halt stands in memory (a restart before the next write forgets it,
/// and the host's next report raises it again).
fn raise(ops: &OpsStore, service: &str, halt: Halt) {
    match ops.read(|state| ignored(state, service, &halt)) {
        None => {}
        Some(Ignored::Standing) => return record(ops, service, halt),
        Some(Ignored::Excluded) => {
            tracing::debug!(service, node_id = %halt.node_id, "ignored a halt report: excluded");
            return;
        }
        Some(Ignored::Acked) => {
            tracing::debug!(service, node_id = %halt.node_id, "ignored a halt report: cleared");
            return;
        }
    }
    tracing::error!(
        alert = "halt",
        service,
        node_id = %halt.node_id,
        check = %halt.check,
        target = %halt.target,
        "a host halted the service: nothing is ordered for it until an operator clears the halt"
    );
    let mut entry = AuditEntry::new(COORDINATOR, "halt");
    entry.service = Some(service.to_owned());
    entry.node_id = Some(halt.node_id.clone());
    entry.reason = Some(halt.check.clone());
    let name = service.to_owned();
    let raised = ops.change(Write::AfterMemory, entry, |state| {
        let marks = state.services.entry(name).or_default();
        marks.halt.get_or_insert(halt);
    });
    if let Err(why) = raised {
        tracing::error!(
            alert = "halt",
            service,
            %why,
            "the halt could not be written to the ops file: it stands in memory"
        );
    }
}

/// Keeps `report`, made while the halt of `service` stands, for the clear to acknowledge. Kept
/// once per host, with the halt: a report the halt itself holds, or one kept
/// already, is not kept again.
fn record(ops: &OpsStore, service: &str, report: Halt) {
    let check = report.check.clone();
    let ack = HaltAck::from(report);
    let known = ops.read(|state| {
        state.services.get(service).is_some_and(|m| {
            m.halt
                .iter()
                .any(|h| (&h.node_id, &h.report) == (&ack.node_id, &ack.report))
                || m.halt_seen.contains(&ack)
        })
    });
    if known {
        return;
    }
    let mut entry = AuditEntry::new(COORDINATOR, "halt_report");
    entry.service = Some(service.to_owned());
    entry.node_id = Some(ack.node_id.clone());
    entry.reason = Some(check);
    let name = service.to_owned();
    let kept = ops.change(Write::AfterMemory, entry, |state| {
        let seen = &mut state.services.entry(name).or_default().halt_seen;
        seen.retain(|s| s.node_id != ack.node_id);
        seen.push(ack);
    });
    if let Err(why) = kept {
        tracing::error!(service, %why, "a halt report could not be written to the ops file");
    }
}

/// The standing halt of `service` in `state`, if any.
pub(crate) fn standing(state: &OpsState, service: &str) -> Option<Halt> {
    state.services.get(service).and_then(|m| m.halt.clone())
}

/// [`Hold::Halted`] for a service that is halted, and [`decide_marked`] (which holds the
/// versions marked bad) for one that is not.
///
/// A halt is checked first, so that a halted service is told nothing: not an order, and not
/// the reason a version is bad. The halt and the bad versions are read under one lock.
pub(crate) fn decide(
    ops: &OpsStore,
    services: &Services,
    desired: &BTreeMap<String, Desired>,
    service: &str,
    fleet: &Fleet,
    id: &NodeId,
    now: Duration,
) -> Decision {
    let (halted, bad) = ops.read(|state| {
        (
            standing(state, service).is_some(),
            state
                .services
                .get(service)
                .map(|marks| marks.bad.clone())
                .unwrap_or_default(),
        )
    });
    if halted {
        return Decision::Hold(Hold::Halted);
    }
    decide_marked(services, desired, &bad, service, fleet, id, now)
}

/// The generation of every service an operator has cleared a halt of, by name.
pub(crate) fn generations(ops: &OpsStore) -> BTreeMap<String, u64> {
    ops.read(|state| {
        state
            .services
            .iter()
            .filter(|(_, marks)| marks.halt_generation > 0)
            .map(|(name, marks)| (name.clone(), marks.halt_generation))
            .collect()
    })
}
