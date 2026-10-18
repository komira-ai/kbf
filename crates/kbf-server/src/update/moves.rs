//! The pure side of a host's move, which the [`super::Controller`] reads, writes and gives:
//! the step's answer for a host, the row that answer makes, and which of its orders go
//! before that row is written.

use kbf_types::FarmTime;

use super::codec::{Desired, HostRow};
use super::command::Command;
use super::facts::{Facts, HostFacts, HostRecord};
use super::node_step::{HostPhase, Transition, step};
use super::policy::Policy;

/// The pure step's move for `host` under `facts`. The controller moves only hosts it found
/// in the facts, so it passes the host itself rather than an id to look up.
pub(super) fn decide(
    facts: &Facts,
    host: &HostFacts,
    policy: &Policy,
    now: FarmTime,
) -> Transition {
    step(&facts.observe_host(host), policy, now)
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
