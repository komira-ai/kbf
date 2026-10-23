//! The phase of a host in a node step (`docs/design/split-mode-updates.md` section 2.6), as
//! the `phase` of its `update/host/<id>` row. Nothing moves a host between phases now:
//! updates are OS packages, driven by `kbf-updater`.
//!
//! ```text
//! Pending -> Selected -> Cordoned -> WaitingLeases -> PlannedDown -> Gated -> Activating
//!   ^  (1 claim) (2 cordon)  (3 leases)     (4 down)      (5 gate)    (6 activate)
//!   |                                                                      |
//!   |   Done <- RejoiningWork <- Confirming <- Repairing <- RejoiningStorage
//!   |  (10 work)    (9 confirm)     (8 repaired)  (7 rejoin storage)
//!   +-- give back (steps 1-5, and 3 for a Mac -> Deferred)       any -> Held
//! ```
//!
//! A phase is the step *entered*; [`HostPhase::entry_commands`] names what entering it
//! issues.

use super::command::{Command, HoldReason, SlotOp};
#[cfg(doc)]
use super::facts::HostRecord;
use super::facts::OrderId;

/// Where one host is in the node step; the `phase` of its `update/host/<id>` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostPhase {
    /// Not started.
    Pending,
    /// Step 1: the slot is claimed for this host.
    Selected,
    /// Step 2: the cordon is ordered.
    Cordoned,
    /// Step 3: waiting for the leases to end by themselves.
    WaitingLeases,
    /// Step 4: the planned-down mark is written; waiting for writes in flight to reach zero.
    PlannedDown,
    /// Step 5: G_S is re-evaluated with the node out.
    Gated,
    /// Step 6: the helper is ordered to flip and restart.
    Activating,
    /// Step 7: the new process registers; `Active` at once, or `CatchingUp` while it rebuilds.
    RejoiningStorage,
    /// Step 8: the debts of the downtime are settled.
    Repairing,
    /// Step 9: `CONFIRM` is ordered.
    Confirming,
    /// Step 10: the node is uncordoned; waiting for its first lease.
    RejoiningWork,
    /// Updated and back at work. The slot is free.
    Done,
    /// A person decides (section 5.1). The slot stays held.
    Held {
        /// Why.
        reason: HoldReason,
    },
    /// A Mac that was busy (section 2.10): the run goes on, and it is picked again when it
    /// goes idle. The UI shows "done with N deferred".
    Deferred,
}

impl HostPhase {
    /// The commands that entering this phase issues. The row is written first and
    /// these run after, so after a restart they run again for the phase read
    /// ([`HostRecord::entry_commands`]). `order` is the row's activation order, which the
    /// two orders carry.
    #[must_use]
    pub fn entry_commands(&self, order: OrderId) -> Vec<Command> {
        match self {
            Self::Selected => vec![Command::CasSlot(SlotOp::Claim)],
            Self::Cordoned => vec![Command::Cordon],
            Self::PlannedDown => vec![Command::MarkDown],
            Self::Activating => vec![Command::Activate { order }],
            Self::Confirming => vec![Command::Confirm { order }],
            Self::RejoiningWork => vec![Command::Uncordon],
            Self::Done => vec![Command::CasSlot(SlotOp::Release)],
            Self::Pending
            | Self::WaitingLeases
            | Self::Gated
            | Self::RejoiningStorage
            | Self::Repairing
            | Self::Held { .. }
            | Self::Deferred => Vec::new(),
        }
    }
}
