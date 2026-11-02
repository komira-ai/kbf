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
//! Each clear raises the service's generation by one. A reply to an agent that announced
//! [`FEATURE`] carries the generation of every service the coordinator defines that has one
//! (`halts`): the agent lifts its hold when it sees a number newer than the one it halted
//! under, so a lost heartbeat cannot make it miss a clear, and a restart of the coordinator,
//! which reads the number from the halts file beside the ops file ([`crate::ops_file`]), does
//! not forget it.
//!
//! A report is ignored when its host is not counted by the service (the service is not
//! defined, the host's role or selector leaves it out, it is unmanaged, or an operator excluded
//! it: [`crate::membership::standing`], the test recording uses), when an operator cleared
//! exactly that report, and when the service is halted already: the first halt stands until it
//! is cleared (the convergence names it).

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::NodeId;
use kbf_updater::wire::{Heartbeat, Outcome, Phase, Token, UpdateResult};

use crate::decide::{Decision, Hold, decide_marked};
use crate::desired::{Desired, KBF};
use crate::fleet::{Fleet, Node};
use crate::identity::Identity;
use crate::membership::{self, Standing};
use crate::ops::{AuditEntry, Halt, HaltAck, OpsError, OpsState, OpsStore, Write};
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

/// Why a halt report changed nothing.
enum Unchanged {
    /// An operator cleared this report.
    Acked,
    /// The service is halted already and the report is the halt's own or kept already.
    Known,
    /// The host has no acknowledgement that the report ends.
    NoAck,
    /// The ops file could not be written.
    Store(OpsError),
}

impl From<OpsError> for Unchanged {
    fn from(why: OpsError) -> Self {
        Self::Store(why)
    }
}

/// Raises the halts the heartbeat `beat` of the host `who` reports, for `kbf` (its top-level
/// facts) and for each service it lists. Before the heartbeat is decided, so the host that
/// reports a halt is ordered nothing in the same reply, and under the fleet's lock, which every
/// decision and order is made under (`Coordinator::reply_to`): a halt is never raised between
/// another heartbeat's decision to order and its order.
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
    node: &Node,
    now: Duration,
) -> Vec<String> {
    let state = ops.state();
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
        let counted = services.get(service).is_some_and(|def| {
            membership::standing(def, &who.node_id, node, &state, now) == Standing::Counted
        });
        if updating || !counted {
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
/// restart. Decided under the writers' lock, on the state it changes. Answers whether it
/// holds: `false` when the write failed and the acknowledgement stands, so no order may go out
/// before it is gone.
fn end_ack(ops: &OpsStore, service: &str, id: &NodeId, report: Option<&Halt>) -> bool {
    let node = id.to_string();
    let report = report.and_then(|halt| halt.report.clone());
    let differs = |ack: &HaltAck| {
        ack.node_id == node && !report.as_ref().is_some_and(|r| ack.matches(&node, r))
    };
    let held = ops.read(|state| {
        state
            .services
            .get(service)
            .is_some_and(|m| m.halt_acked.iter().any(differs))
    });
    if !held {
        return true;
    }
    let ended = ops.decide_change(Write::BeforeMemory, |state| {
        let marks = state.services.entry(service.to_owned()).or_default();
        let before = marks.halt_acked.len();
        marks.halt_acked.retain(|ack| !differs(ack));
        if marks.halt_acked.len() == before {
            return Err(Unchanged::NoAck);
        }
        let mut entry = AuditEntry::new(COORDINATOR, "halt_ack_ended");
        entry.service = Some(service.to_owned());
        entry.node_id = Some(node.clone());
        Ok(entry)
    });
    match ended {
        Ok(_) | Err(Unchanged::NoAck) => true,
        Err(Unchanged::Store(why)) => {
            tracing::error!(service, %why, "the end of an acknowledgement could not be written: no order for the host");
            false
        }
        Err(Unchanged::Acked | Unchanged::Known) => unreachable!("not answered by this change"),
    }
}

/// Raises `halt` for `service`, or keeps it for the clear when the service is halted already,
/// unless an operator cleared exactly this report: in memory first, then written. Which of
/// these it is is decided under the writers' lock, on the state the change is applied to, so a
/// clear or another host's halt that got there first is seen. A failed write is logged and the
/// change stands in memory (a restart before the next write forgets it, and the host's next
/// report makes it again).
///
/// A report made while the halt stands is kept once per host, with the halt (`halt_seen`), for
/// the clear to acknowledge: a report the halt itself holds, or one kept already, is not kept
/// again.
fn raise(ops: &OpsStore, service: &str, halt: Halt) {
    let node_id = halt.node_id.clone();
    let changed = ops.decide_change(Write::AfterMemory, |state| {
        let marks = state.services.entry(service.to_owned()).or_default();
        let acked = halt.report.as_ref().is_some_and(|report| {
            marks
                .halt_acked
                .iter()
                .any(|ack| ack.matches(&halt.node_id, report))
        });
        if acked {
            return Err(Unchanged::Acked);
        }
        let mut entry = AuditEntry::new(COORDINATOR, "halt");
        entry.service = Some(service.to_owned());
        entry.node_id = Some(halt.node_id.clone());
        entry.reason = Some(halt.check.clone());
        let Some(standing) = &marks.halt else {
            tracing::error!(
                alert = "halt",
                service,
                node_id = %halt.node_id,
                check = %halt.check,
                target = %halt.target,
                "a host halted the service: nothing is ordered for it until an operator clears the halt"
            );
            marks.halt = Some(halt);
            return Ok(entry);
        };
        let ack = HaltAck::from(halt);
        let own = (&standing.node_id, &standing.report) == (&ack.node_id, &ack.report);
        if own || marks.halt_seen.contains(&ack) {
            return Err(Unchanged::Known);
        }
        marks.halt_seen.retain(|seen| seen.node_id != ack.node_id);
        marks.halt_seen.push(ack);
        entry.action = "halt_report".to_owned();
        Ok(entry)
    });
    match changed {
        Ok(_) | Err(Unchanged::Known | Unchanged::NoAck) => {}
        Err(Unchanged::Acked) => {
            tracing::debug!(service, %node_id, "ignored a halt report: cleared");
        }
        Err(Unchanged::Store(why)) => tracing::error!(
            alert = "halt",
            service,
            %node_id,
            %why,
            "a halt or a halt report could not be written to the ops file: it stands in memory"
        ),
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

/// The generation of every service an operator has cleared a halt of, by name, of the
/// services `services` defines. The ops file keeps the generation of a service that is no longer
/// defined (it counts on if the service comes back), but an agent reads at most a services
/// file's worth of them and refuses an answer with more, so only the defined ones are told.
pub(crate) fn generations(ops: &OpsStore, services: &Services) -> BTreeMap<String, u64> {
    ops.read(|state| {
        state
            .services
            .iter()
            .filter(|(name, marks)| marks.halt_generation > 0 && services.get(name).is_some())
            .map(|(name, marks)| (name.clone(), marks.halt_generation))
            .collect()
    })
}
