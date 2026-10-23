//! The durable rows of a split-mode farm's update bookkeeping (`docs/design/split-mode-updates.md`
//! sections 2.6 to 2.11, 5.3 and 6.1; the design is #898): the facts about hosts ([`Facts`]),
//! the phase a host's row records ([`HostPhase`]) and the rows themselves ([`Rows`]). Nothing
//! picks hosts or steps them now: updates are OS packages, driven by `kbf-updater`.

pub mod age;
mod codec;
mod command;
mod facts;
mod node_step;
mod policy;
mod proposal;
mod rows;
mod slot;

pub use crate::storage_gate::{GateFail, MemberFacts, member_facts, storage_gate, storage_half};
pub use codec::{Desired, HostRow};
pub use command::{Alert, Command, HoldReason, Severity, SlotOp};
pub use facts::{
    AvailableFact, DaemonLoad, Facts, Generation, HelperReport, HelperStatus, HostFacts, HostKind,
    HostRecord, OrderId, Platform, PlatformClass, Release, Source, Work,
};
pub use node_step::HostPhase;
pub use policy::{Policy, PolicyError};
pub use proposal::{PROPOSED, Proposal, proposal_key};
pub use rows::{RowError, Rows, Stored};
pub use slot::{Holder, Slot, SlotClass};
