//! Rolling a service back by itself: what the coordinator records about a version and what
//! it does when a release fails on enough hosts (`docs/design/host-updates.md`, sections 3.8
//! and 5.3).
//!
//! [`plan`] is a pure function of a service's definition, its desired state, the marks the
//! ops store holds for it ([`crate::ops::ServiceOps`]), the fleet and the time; it says what
//! to change and changes nothing. The coordinator applies the answer after every heartbeat
//! (`apply`).

use std::time::Duration;

use kbf_updater::version::Version;
use kbf_updater::wire::{Class, Outcome, Phase};
use serde::Deserialize;

use crate::converge::State;
use crate::desired::{Desired, DesiredStore, StoreError};
use crate::fleet::{Fleet, Host};
use crate::ops::{AuditEntry, OpsError, OpsStore, ServiceOps, Write};
use crate::service::convergence_of;
use crate::services::ServiceDef;

/// Who the coordinator's own changes are logged as.
pub const COORDINATOR: &str = "coordinator";

/// Who a change made because CI set a version by hand is logged as.
pub const CI: &str = "ci";

/// Whether the coordinator may roll a service back by itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rollback {
    /// Only an operator or CI sets an older desired version.
    #[default]
    Manual,
    /// The coordinator sets the last good version when a release fails on enough hosts.
    Automatic,
}

/// How many hosts must fail an update to the desired version before an `automatic` service
/// rolls back: the larger of `min_hosts` and `percent` of the live hosts, rounded up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FailThreshold {
    /// The share of live hosts, 1 to 100.
    pub percent: u32,
    /// The fewest hosts, at least 1.
    pub min_hosts: u32,
}

impl Default for FailThreshold {
    /// A quarter of the hosts and at least two: two failed hosts is the smallest signal that
    /// is not one bad host.
    fn default() -> Self {
        Self {
            percent: 25,
            min_hosts: 2,
        }
    }
}

impl FailThreshold {
    /// How many failed hosts it takes when `live` hosts are live.
    #[must_use]
    pub fn hosts(self, live: usize) -> usize {
        (self.percent as usize * live)
            .div_ceil(100)
            .max(self.min_hosts as usize)
    }
}

/// One thing [`plan`] says to do for a service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    /// Records `version` as the last good one.
    LastGood(Version),
    /// Forgets the last good version.
    ClearLastGood,
    /// Marks `version` bad: it is never ordered again until CI sets it by hand.
    MarkBad(Version),
    /// Sets the desired version to `version`, the other desired fields as they are.
    RollBackTo(Version),
    /// Drops the failure ack of the host with this node id.
    DropAck(String),
}

/// Whether `host`'s last update was a failure of `version` that is the release's fault: it
/// failed after the commit or at the release gate, and the hook did not blame the
/// environment. A failure that says nothing about where it happened (an updater without
/// phases) is not counted, nor is one before the commit, at the environment gate, or a halt.
fn is_release_failure(host: &Host, version: &Version) -> bool {
    let Some(last) = host.beat.last_update.as_ref() else {
        return false;
    };
    if last.outcome != Outcome::Failed || last.target != *version || host.beat.version == *version {
        return false;
    }
    matches!(last.phase, Some(Phase::AfterCommit | Phase::GateRelease))
        && last.class != Some(Class::Environment)
}

/// What to change for `def` given its desired state `want`, the marks `marks` the ops store
/// holds for it, and the fleet at `now`.
///
/// Nothing for a `manual` service (a farm that never asks for a rollback keeps its files as
/// they were), nothing in the quiet start (the coordinator has not heard the hosts that would
/// outvote a few failures) and nothing while the service is halted. Otherwise, the first that
/// applies:
///
/// 1. the desired version has converged and baked and nothing holds a slot: record it as
///    the last good version (unless it is, or is marked bad);
/// 2. the desired version is `min_version` and an older last good version is recorded: forget
///    it, since the version before the first release with hooks is no rollback target;
/// 3. an older last good version is recorded, and the
///    desired version is marked bad or failed its release on [`FailThreshold::hosts`] live
///    hosts: mark it bad (unless it already is), then set the desired version to the last
///    good one. The mark comes first, so a stop between the two leaves a marked desired
///    version, which this finishes on the next beat.
#[must_use]
pub fn plan(
    def: &ServiceDef,
    want: &Desired,
    marks: &ServiceOps,
    fleet: &Fleet,
    now: Duration,
) -> Vec<Change> {
    if def.rollback != Rollback::Automatic || fleet.is_quiet(now) || marks.halt.is_some() {
        return Vec::new();
    }
    let wanted = &want.version;
    let service = def.name.as_str();
    let live = || {
        fleet
            .hosts_of(service)
            .filter(|(_, h)| def.roles.contains_key(h.role_name()))
            .filter(|(_, h)| h.is_live(now))
    };
    let older = marks.last_good.as_ref().filter(|good| *good != wanted);
    let marked = marks.bad.contains(wanted);
    let settled = convergence_of(def, Some(want), fleet, now).state == State::Converged
        && !live().any(|(id, h)| fleet.holds_slot(service, id, h, now));
    let recorded = marks.last_good.as_ref() == Some(wanted);
    if settled && !recorded && !marked {
        return vec![Change::LastGood(wanted.clone())];
    }
    let floored = def.min_version.as_ref() == Some(wanted);
    if floored {
        return older.map(|_| Change::ClearLastGood).into_iter().collect();
    }
    let Some(back) = older else {
        return Vec::new();
    };
    let failed = live()
        .filter(|(_, h)| is_release_failure(h, wanted))
        .count();
    if !marked && failed < def.rollback_after_failed.hosts(live().count()) {
        return Vec::new();
    }
    let mut changes = Vec::new();
    if !marked {
        changes.push(Change::MarkBad(wanted.clone()));
    }
    changes.push(Change::RollBackTo(back.clone()));
    changes
}

/// Why a change [`plan`] asked for was not made.
#[derive(Debug, thiserror::Error)]
enum Failed {
    /// The ops file could not be written.
    #[error("the ops file could not be written: {0}")]
    Ops(#[from] OpsError),
    /// The desired state could not be written.
    #[error("the desired state could not be written: {0}")]
    Desired(#[from] StoreError),
}

fn entry(by: &str, action: &str, service: &str, reason: String) -> AuditEntry {
    let mut entry = AuditEntry::new(by, action);
    entry.service = Some(service.to_owned());
    entry.reason = Some(reason);
    entry
}

fn apply(
    service: &str,
    want: &Desired,
    change: Change,
    ops: &OpsStore,
    store: &DesiredStore,
) -> Result<(), Failed> {
    match change {
        Change::LastGood(version) => {
            let audit = entry(COORDINATOR, "last_good", service, version.to_string());
            ops.change(Write::BeforeMemory, audit, |state| {
                state
                    .services
                    .entry(service.to_owned())
                    .or_default()
                    .last_good = Some(version);
            })?;
        }
        Change::ClearLastGood => {
            let why = "the desired version is the first with update hooks".to_owned();
            let audit = entry(COORDINATOR, "clear_last_good", service, why);
            ops.change(Write::BeforeMemory, audit, |state| {
                state
                    .services
                    .entry(service.to_owned())
                    .or_default()
                    .last_good = None;
            })?;
        }
        Change::MarkBad(version) => {
            let why = format!("{version} failed its release on enough hosts");
            let audit = entry(COORDINATOR, "mark_bad", service, why);
            // Raising: in memory first, so that a failed write still stops the orders.
            ops.change(Write::AfterMemory, audit, |state| {
                state
                    .services
                    .entry(service.to_owned())
                    .or_default()
                    .bad
                    .push(version);
            })?;
        }
        Change::RollBackTo(version) => {
            tracing::warn!(%service, from = %want.version, to = %version, "rolling back");
            let mut back = want.clone();
            back.version = version;
            store.set(service, back)?;
        }
        Change::DropAck(_) => {}
    }
    Ok(())
}

/// Plans and makes the changes for `def` at `now`, after a heartbeat. A change that fails is
/// logged and the rest are left for the next beat: the heartbeat is answered either way.
/// A service with no desired state has nothing to plan.
pub fn reconcile(
    def: &ServiceDef,
    ops: &OpsStore,
    store: &DesiredStore,
    fleet: &Fleet,
    now: Duration,
) {
    let Some(want) = store.get(&def.name) else {
        return;
    };
    for change in plan(def, &want, &ops.marks(&def.name), fleet, now) {
        if let Err(why) = apply(&def.name, &want, change, ops, store) {
            tracing::error!(service = %def.name, %why, "a rollback change was not made");
            return;
        }
    }
}

/// Removes the mark that `version` is bad, if it has one: CI set it by hand. Written to the
/// file before it leaves memory (it clears protection), and before CI's desired state is
/// stored, so that a failed write leaves both as they were.
///
/// # Errors
/// [`OpsError`]: the ops file could not be written.
pub fn clear_bad(ops: &OpsStore, service: &str, version: &Version) -> Result<(), OpsError> {
    if !ops.marks(service).bad.contains(version) {
        return Ok(());
    }
    let audit = entry(
        CI,
        "unmark_bad",
        service,
        format!("{version} was set by CI"),
    );
    ops.change(Write::BeforeMemory, audit, |state| {
        state
            .services
            .entry(service.to_owned())
            .or_default()
            .bad
            .retain(|bad| bad != version);
    })
    .map(drop)
}
