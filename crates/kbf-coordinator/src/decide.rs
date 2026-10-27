//! The reconciling decision: whether to tell one host to update now
//! (`docs/design/updates.md`, section 3).
//!
//! A pure function of the desired state, the fleet the heartbeats built, the host and the
//! time. A host is told to update only if every rule below holds, checked in this order:
//!
//! 1. the coordinator has been up for [`crate::fleet::QUIET_START`];
//! 2. the host is not already updating;
//! 3. the host is behind: it does not run the desired version (a rollback is a desired
//!    version older than the one the hosts run, under the same rules);
//! 4. the host did not fail an update to this very version (it is not told again; setting
//!    another version lifts it);
//! 5. its role has room: fewer hosts of the role hold a slot than `max_updating` allows (a
//!    host holds a slot while updating, after a failed update until it heartbeats healthy,
//!    and from the order the coordinator gave it until the host reports it or the order
//!    ages out, [`crate::fleet::ORDER_AGE`]);
//! 6. a frontend waits for every live storage, daemon and Mac host to run the desired
//!    version and be done updating;
//! 7. the hosts that would still be up without this one number at least the minimum:
//!    `min_up.storage` for a storage host, `min_up.frontend` for a frontend, and at least
//!    one frontend whatever the setting says. A host is up when it is live, healthy, not
//!    updating and not under an outstanding order, so a host that has not heartbeated since
//!    a restart counts as missing.
//!
//! The decision records nothing: the caller notes an `Update` it returns with
//! [`Fleet::order`], under the same lock, so the next host's decision sees it. A host that
//! asks again while its own order is outstanding is told the same again: its own order never
//! counts against it, and the others' slots have not changed.
//!
//! The reply carries the desired version and nothing else.

use std::time::Duration;

use kbf_types::{HostRole, NodeId};
use kbf_updater::wire::{Outcome, Reply};

use crate::desired::Desired;
use crate::fleet::{Fleet, Host};

/// What to tell the host `id` at `now`.
#[must_use]
pub fn decide(desired: &Desired, fleet: &Fleet, id: &NodeId, now: Duration) -> Reply {
    if order(desired, fleet, id, now) {
        Reply::Update {
            version: desired.version.clone(),
        }
    } else {
        Reply::Nothing
    }
}

fn order(desired: &Desired, fleet: &Fleet, id: &NodeId, now: Duration) -> bool {
    let Some(host) = fleet.get(id) else {
        return false;
    };
    let others = || fleet.hosts().filter(|(other, _)| *other != id);
    let quiet = fleet.is_quiet(now);
    let failed_here = host
        .beat
        .last_update
        .as_ref()
        .is_some_and(|r| r.outcome == Outcome::Failed && r.target == desired.version);
    let slots = others()
        .filter(|(o, h)| h.role == host.role && fleet.holds_slot(o, h, now))
        .count();
    !quiet
        && host.beat.updating.is_none()
        && host.beat.version != desired.version
        && !failed_here
        && slots < desired.max_updating.of(host.role) as usize
        && (host.role != HostRole::Frontend
            || others().all(|(_, h)| !is_pending_before_frontends(h, desired, now)))
        && others()
            .filter(|(o, h)| h.role == host.role && fleet.is_up(o, h, now))
            .count()
            >= required_up(desired, host.role)
}

/// Whether `h` is a live storage, daemon or Mac host that a frontend must wait for.
fn is_pending_before_frontends(h: &Host, desired: &Desired, now: Duration) -> bool {
    h.role != HostRole::Frontend
        && h.is_live(now)
        && (h.beat.version != desired.version || h.beat.updating.is_some())
}

/// How many other hosts of `role` must be up while one updates.
fn required_up(desired: &Desired, role: HostRole) -> usize {
    match role {
        HostRole::Storage => desired.min_up.storage as usize,
        HostRole::Frontend => (desired.min_up.frontend as usize).max(1),
        HostRole::Daemon | HostRole::MacDaemon => 0,
    }
}
