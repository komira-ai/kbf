//! The pure side of a host's move, which the [`super::Controller`] reads, writes and gives:
//! the step's answer for a host, the row that answer makes, and which of its orders go
//! before that row is written.

use kbf_types::{FarmTime, WorkerId};

use super::codec::{Desired, HostRow};
use super::command::Command;
use super::facts::{Facts, HostRecord};
use super::node_step::{HostPhase, Transition, step};
use super::policy::Policy;
use super::report::ControllerError;

/// The pure step's move for `id` under `facts`.
pub(super) fn decide(
    facts: &Facts,
    id: &WorkerId,
    policy: &Policy,
    now: FarmTime,
) -> Result<Transition, ControllerError> {
    match facts.observe(id) {
        Some(obs) => Ok(step(&obs, policy, now)),
        None => Err(ControllerError::Invariant(format!(
            "{id} is not in the facts"
        ))),
    }
}

/// The row after `moved`, made at `now` in the run `desired`: the phase and the fields the
/// move names. A host that enters `Selected` from elsewhere takes the generation and commit
/// of the run that starts it; every other move keeps them, so a host finishes the run that
/// began it.
pub(super) fn next_row(
    old: &HostRow,
    moved: &Transition,
    now: FarmTime,
    desired: &Desired,
) -> HostRow {
    let began = moved.next == HostPhase::Selected && old.record.phase != HostPhase::Selected;
    HostRow {
        record: HostRecord {
            phase: moved.next.clone(),
            since: if moved.next == old.record.phase {
                old.record.since
            } else {
                now
            },
            generation: if began {
                desired.generation
            } else {
                old.record.generation
            },
            order: moved.order.unwrap_or(old.record.order),
            boot_before: moved.boot.or(old.record.boot_before),
        },
        commit: if began {
            desired.commit.clone()
        } else {
            old.commit.clone()
        },
    }
}

/// Splits a move's orders into those given before its row is written and those after. A
/// move that leaves its step gives the undoing orders first (see the controller's docs);
/// every other move writes first.
pub(super) fn split(commands: &[Command], leaves: bool) -> (Vec<Command>, Vec<Command>) {
    let undoes = |c: &Command| {
        matches!(
            c,
            Command::Undown | Command::Uncordon | Command::Confirm { .. }
        )
    };
    if leaves {
        commands.iter().cloned().partition(undoes)
    } else {
        (Vec::new(), commands.to_vec())
    }
}

/// Whether a command reaches the farm: not a place in the slot (a write of its own) and not
/// an alert.
pub(super) fn acts_on_the_farm(command: &Command) -> bool {
    !matches!(command, Command::CasSlot(_) | Command::Alert(_))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::update::facts::{Generation, OrderId, Release};
    use crate::update::slot::Slot;

    /// Catches: a step taken for a host the facts do not show, whether by a panic on a missing
    /// host or by stepping an invented one. The controller only asks about hosts it found in
    /// the facts, so no tick reaches this answer: it is checked here, where `decide` can be
    /// called with a host the controller would never pass.
    #[test]
    fn a_host_the_facts_do_not_show_is_refused_by_name() {
        let facts = Facts {
            target: Release::new("v2"),
            generation: Generation(1),
            next_order: OrderId::new(1, 1),
            hosts: Vec::new(),
            members: Vec::new(),
            slot: Slot::default(),
            queued_classes: Some(BTreeSet::new()),
            wait_since: FarmTime::from_millis(0),
        };
        let ghost = WorkerId::new("ghost");
        let now = FarmTime::from_millis(5);
        let error = decide(&facts, &ghost, &Policy::default(), now).unwrap_err();
        assert!(matches!(error, ControllerError::Invariant(_)));
        assert!(error.to_string().contains("ghost"));
    }
}
