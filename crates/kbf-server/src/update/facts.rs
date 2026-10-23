//! The facts of a split-mode update: plain data filled from the
//! `Farm`, `Members`, the hosts' heartbeats and the `update/*` rows. Nothing here reads
//! a clock or a store.
//!
//! **An absent metric is `None`, and `None` fails closed.** A lease count, a write
//! count, a repair debt, a placement or a boot counter the controller could not read is
//! never filled in as zero: every use below treats it as "not proven", so the gate it
//! feeds stays shut.
//!
//! **Facts may lag.** A heartbeat is up to a time to live old, and a member row can say
//! `Active` for a node whose update the record already started. So *what is out* comes
//! from the records ([`HostRecord`]) and the slot, which the controller wrote itself,
//! never from member state. A member that was not heard within its time to live counts as
//! down ([`MemberFacts::is_fresh`]); the host metrics carry no observation time, so the
//! controller fills them from the same heartbeat look it reads the members from.

use std::collections::BTreeSet;

use kbf_map::MemberId;
use kbf_types::{FarmTime, WorkerId};
use serde::{Deserialize, Serialize};

use super::command::Command;
use super::node_step::HostPhase;
use super::slot::Slot;
use crate::storage_gate::MemberFacts;
use crate::update_status::ReleaseSource;

/// A release, by whatever the controller compares: a commit or a software-set digest.
/// Only equality is defined, so a rollback is another target and never "an older one".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release(String);

impl Release {
    /// The release called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Its name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Where the release of a run comes from. The same two words as `kbf_update::Source`, which
/// this crate's library does not depend on. The default is [`Source::Fetch`], what every run
/// has meant so far, so a row that names no source is a fetch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The hosts pull the release from the source their installer is configured with.
    #[default]
    Fetch,
    /// The hosts use the release their installer's package carries.
    Staged,
}

/// The generation of a run (`update/desired`): it grows with every change of the desired
/// release, and a rollback is the older release under a higher generation (section 2.11;
/// `pull-updates.md` section 5). A higher generation is a new run. A host row carries the
/// generation of the run that wrote it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Generation(pub u64);

/// The id of an order to a host's helper: the store's epoch and a number drawn from a
/// counter of that epoch. A helper refuses a lower id than it has seen, and a status it
/// reports names the order it answers, so a status left over from an earlier update is never
/// read as the answer to this one (section 6.1).
///
/// The epoch comes first in the order, so a store restored from a backup, whose counters have
/// gone back, still issues ids above every id of the store it replaced: it opens at a later
/// epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OrderId {
    /// The epoch of the store that issued it.
    pub epoch: u64,
    /// The number within the epoch.
    pub seq: u64,
}

impl OrderId {
    /// The id `seq` of `epoch`.
    #[must_use]
    pub const fn new(epoch: u64, seq: u64) -> Self {
        Self { epoch, seq }
    }
}

/// What kind of Mac a host is: the class a queued operation asks for (`os`, hardware and
/// Xcode, as the platform match sees them).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlatformClass(String);

impl PlatformClass {
    /// The class called `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// Its name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The operating system of a host, and for a Mac its class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Platform {
    /// A Linux host.
    Linux,
    /// A Mac of this class (section 2.10).
    Mac(PlatformClass),
}

/// What a host runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKind {
    /// A storage half and a daemon half in one process: all ten steps apply. Only a Linux
    /// host is one; the facts cannot say otherwise, but a controller should not build a
    /// `StorageAndDaemon` Mac.
    StorageAndDaemon,
    /// A daemon alone (the Macs, a cloud daemon group): no storage steps (section 2.6).
    DaemonOnly,
}

/// What the scheduler says a daemon is doing. Idle is all three at rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DaemonLoad {
    /// Leases the host holds.
    pub leases: u32,
    /// Claims granted to the host that its daemon has not been sent (section 2.7: a lease
    /// is not over until it has been delivered and ended).
    pub undelivered: u32,
    /// A whole-machine hold, as a Mac's `Preparing` and `Restoring` phases take (section
    /// 2.10: a Mac with no leases may still be switching its session).
    pub whole_machine_hold: bool,
}

/// What the host's root helper last reported about the order it was given (section 2.6,
/// steps 6 and 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HelperStatus {
    /// `ACTIVATED_UNCONFIRMED`: flipped and restarted, the revert timer armed.
    ActivatedUnconfirmed,
    /// `CONFIRMED`: the revert timer is cancelled.
    Confirmed,
    /// `REVERTED`: the helper put the previous release back (it did so itself).
    Reverted,
    /// `REFUSED`: the helper refused the order. At step 6 that is the activate order itself,
    /// before any flip. At steps 7 to 9 it is a revert that could not run, because the
    /// previous release is missing.
    Refused,
}

/// A helper's status and the order it answers. The node step reads a status only when
/// `order` is the activation order of the host's current update; any other is left over
/// from an earlier update, or not this controller's, and reads as no report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HelperReport {
    /// The activation order the status is about.
    pub order: OrderId,
    /// What the helper did with it.
    pub status: HelperStatus,
}

/// What a host that was uncordoned has been given (section 2.6, step 10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    /// The first lease was granted to the node.
    Leased,
    /// No queued operation could run on the node for one heartbeat.
    QueueEmpty,
    /// Neither yet.
    Waiting,
}

/// One host's `update/host/<id>` row: the controller resumes from it after a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostRecord {
    /// Where the host is in the node step.
    pub phase: HostPhase,
    /// When it entered that phase.
    pub since: FarmTime,
    /// The generation of the run that wrote the row. A row of an earlier run does not say
    /// this run's host is done ([`HostRecord::is_open_in`]).
    pub generation: Generation,
    /// The activation order of this update: set with the row that enters `Activating`
    /// ([`super::Transition::order`]), and what a status must name to count.
    pub order: OrderId,
    /// The member's boot counter when the planned-down mark was written
    /// ([`super::Transition::boot`]): the new process must report a larger one.
    pub boot_before: Option<u64>,
}

impl HostRecord {
    /// The row of a host no run has touched: `Pending`, written by no run (generation 0,
    /// before the first offer), with no order and no mark.
    #[must_use]
    pub const fn unstarted() -> Self {
        Self {
            phase: HostPhase::Pending,
            since: FarmTime::from_millis(0),
            generation: Generation(0),
            order: OrderId::new(0, 0),
            boot_before: None,
        }
    }

    /// Whether the picker may start the host in the run `run`: a host that has not begun
    /// or was deferred, and a finished host of an *earlier* run, because a rollback walks
    /// back the hosts the forward run finished (section 2.11). `Held` and every step are
    /// never open: they hold the run.
    #[must_use]
    pub fn is_open_in(&self, run: Generation) -> bool {
        match self.phase {
            HostPhase::Pending | HostPhase::Deferred => true,
            HostPhase::Done => self.generation < run,
            HostPhase::Selected
            | HostPhase::Cordoned
            | HostPhase::WaitingLeases
            | HostPhase::PlannedDown
            | HostPhase::Gated
            | HostPhase::Activating
            | HostPhase::RejoiningStorage
            | HostPhase::Repairing
            | HostPhase::Confirming
            | HostPhase::RejoiningWork
            | HostPhase::Held { .. } => false,
        }
    }

    /// The commands a controller that restarted runs again for the phase it reads: the
    /// phase's entry commands, with this row's own activation order.
    #[must_use]
    pub fn entry_commands(&self) -> Vec<Command> {
        self.phase.entry_commands(self.order)
    }
}

/// A release the host's package carries, which its installer has found whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableFact {
    /// The release: a full commit (40 lowercase hex digits), so it compares equal to a target
    /// that is a full commit.
    pub commit: Release,
    /// The number of commits behind it: puts two builds of one branch in order.
    pub seq: u64,
}

/// One host as the controller sees it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostFacts {
    /// The daemon's node id: the key of its `update/host/<id>` row.
    pub id: WorkerId,
    /// What it runs.
    pub kind: HostKind,
    /// Its operating system.
    pub platform: Platform,
    /// The storage member its storage half is, joined by exact address. `None` is "not
    /// linked": for a [`HostKind::StorageAndDaemon`] host that makes it ineligible, never
    /// a host without storage steps.
    pub member: Option<MemberId>,
    /// The release it runs, as it reported it; `None` if it has not.
    pub release: Option<Release>,
    /// The release its package carries, when its installer has found it whole; `None` if it
    /// reports none, if its installer predates the field, or if it reports a commit that is
    /// not 40 lowercase hex digits.
    pub available: Option<AvailableFact>,
    /// Where its installer fetches releases from; [`ReleaseSource::Unreported`] if it does not
    /// say or sent a number this build does not know (an installer that predates the field, or
    /// no report at all).
    pub release_source: ReleaseSource,
    /// Whether its daemon stream is open.
    pub connected: bool,
    /// Whether placement has it cordoned; `None` if unknown.
    pub cordoned: Option<bool>,
    /// Its daemon half's load; `None` if unknown.
    pub load: Option<DaemonLoad>,
    /// Write sessions open on its storage half; `None` if it does not report them.
    pub open_writes: Option<u64>,
    /// Writes in flight to it from the frontend's router; `None` if unknown.
    pub routed_writes: Option<u64>,
    /// What its helper last reported, and the order it answers; `None` if nothing yet.
    pub helper: Option<HelperReport>,
    /// What it has been given since its uncordon; `None` if unknown.
    pub work: Option<Work>,
    /// Its row in this run.
    pub record: HostRecord,
}

/// The whole fleet at one look.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    /// The release the run drives every host to. A rollback is the same run with the old
    /// release here: hosts are compared with `==`, never ordered.
    pub target: Release,
    /// Where `target` comes from. The picker reads the source of `update/desired`; the node
    /// step reads the source of the run that started the host ([`super::HostRow::source`]),
    /// as it reads that run's commit for `target`, so a host keeps its run's source.
    pub source: Source,
    /// The generation of the run (`update/desired`).
    pub generation: Generation,
    /// The id the next order will carry: greater than every id issued so far. The
    /// controller draws it from the store's counter and, when a [`super::Transition`]
    /// names an order, counts it as used.
    pub next_order: OrderId,
    /// Every host of the run.
    pub hosts: Vec<HostFacts>,
    /// Every storage member.
    pub members: Vec<MemberFacts>,
    /// Who holds `update/slot`.
    pub slot: Slot,
    /// The Mac classes some queued operation asks for; `None` if unknown.
    pub queued_classes: Option<BTreeSet<PlatformClass>>,
    /// When the run started waiting for an idle host: the run's start (E8) or the last
    /// host's uncordon. The D3 and `mac_force_after` windows count from here.
    pub wait_since: FarmTime,
}
