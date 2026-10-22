//! The supervisor's status, assembled from the intent record, the proxy's permits
//! ([`Gate::open_streams`]) and the child's own counts ([`ChildAdmin::open_counts`]).

use super::super::api::model::{NewChild, OldChild, UpdateId};
use super::super::api::traits::{RunningUpdate, SupervisorStatus};
use super::super::model::{Intent, Phase, Slot, Step};
use super::Parts;

/// The slot that serves while the update is open: the new one from the switch on, unless
/// a rollback is under way (the old one is taking the generation back).
pub(super) fn serving_live(intent: &Intent) -> Slot {
    if intent.phase >= Phase::Switched && !intent.rolling_back {
        intent.new_slot
    } else {
        intent.old_slot
    }
}

/// The slot that serves once the update has ended in a terminal phase. `new_holds` asks the
/// store's answer, and is read only for the one ending the record alone cannot tell apart
/// (a restart that failed after the switch, or before it).
pub(super) fn active_after(intent: &Intent, new_holds: &dyn Fn() -> bool) -> Slot {
    let new_serves = match intent.phase {
        Phase::Done => true,
        Phase::RolledBack => false,
        Phase::Failed => match intent.failure.as_ref().map(|f| f.step) {
            // A confirm that failed after the switch, a rollback that did not land, or an order
            // for the nodes the new child would not take: the new child keeps serving.
            Some(Step::Confirm | Step::Rollback | Step::Nodes) => true,
            Some(Step::Restart) => new_holds(),
            _ => false,
        },
        _ => return serving_live(intent),
    };
    if new_serves {
        intent.new_slot
    } else {
        intent.old_slot
    }
}

/// The slot whose child is being drained: the old one, or the new one after a rollback.
pub(super) fn draining_slot(intent: &Intent) -> Slot {
    intent.retire.unwrap_or(if intent.rolling_back {
        intent.new_slot
    } else {
        intent.old_slot
    })
}

/// Whether the intent is in the part of an update where an old child is alive beside the
/// serving one.
pub(super) fn has_old_phase(intent: &Intent) -> bool {
    (Phase::Switched..=Phase::Draining).contains(&intent.phase)
}

/// The status. The blocking admin call for the old child's counts is made only while an
/// old child is draining.
pub(super) fn assemble(
    parts: &Parts,
    intent: Option<Intent>,
    active: Slot,
    open: bool,
) -> SupervisorStatus {
    let Some(intent) = intent else {
        return SupervisorStatus::default();
    };
    // An open update whose record has ended (the swap thread has not closed it yet) serves
    // where the ending says: `serving_live` reads a `failed` record as past the switch.
    let holds = || parts.admin.role(intent.new_slot).is_ok_and(|r| r.holder);
    let serving = if open {
        active_after(&intent, &holds)
    } else {
        active
    };
    let running = if serving == intent.new_slot {
        intent.to.clone()
    } else {
        intent.from.clone()
    };
    let old = (open && has_old_phase(&intent))
        .then(|| old_child(parts, &intent))
        .flatten();
    let update = RunningUpdate {
        update_id: UpdateId(intent.update_id.clone()),
        from: intent.from.clone(),
        to: intent.to.clone(),
        phase: intent.phase,
        since_s: intent.phase_started,
        active_slot: serving,
        old,
        new: NewChild {
            open_streams: parts.gate.open_streams(intent.new_slot),
        },
        // The intent keeps `nodes_posted` as a flag; the API reads only whether it is zero.
        nodes_updated: u32::from(intent.nodes_posted),
        failure: intent.failure.clone(),
        source: intent.source,
    };
    SupervisorStatus {
        update: Some(update),
        // An old child is drained inside the update, and `old` says so; the swap thread
        // does not end before it is gone.
        old_child_draining: false,
        running: Some(running),
    }
}

/// The child being drained, if its process is still there. `open_streams` are the proxy's
/// permits; `open_executions` is what the child counts of its own Execute and
/// WaitExecution streams. The child's open worker sessions and queued operations are not
/// shown: queued operations do not gate the retire, and the wire has no field for sessions.
/// `oldest_stream_age_s` is 0: the gate does not report an age.
fn old_child(parts: &Parts, intent: &Intent) -> Option<OldChild> {
    let slot = draining_slot(intent);
    let ident = if slot == intent.old_slot {
        intent.old_proc.as_ref()?
    } else {
        intent.new_proc.as_ref()?
    };
    if parts.child.exited(ident).is_some() {
        return None;
    }
    let counts = parts.admin.open_counts(slot).ok();
    Some(OldChild {
        open_streams: parts.gate.open_streams(slot),
        oldest_stream_age_s: 0,
        open_executions: counts.map_or(0, |c| c.open_streams),
        queued_operations: 0,
    })
}
