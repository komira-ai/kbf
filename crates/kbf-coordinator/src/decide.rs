//! The reconciling decision: whether to tell one host to update now
//! (`docs/design/updates.md`, section 3).
//!
//! A pure function of the services, their desired states, the fleet the heartbeats built, the
//! host and the time, decided per host and service. A host is told to update a service only if
//! every rule below holds, checked in this order (the role is the host's role in that service's
//! definition; `kbf`'s definition, [`crate::services::ServiceDef::kbf`], gives the rules their
//! old wording):
//!
//! 1. the coordinator has been up for [`crate::fleet::QUIET_START`];
//! 2. the host is not already updating, and no other service updates on it or has an order
//!    outstanding for it (a host does one update at a time);
//! 3. the host is behind: it does not run the desired version (a rollback is a desired
//!    version older than the one the hosts run, under the same rules);
//! 4. the host did not fail an update to this very version (it is not told again; setting
//!    another version lifts it);
//! 5. the service runs on the host's role, and the role has room: fewer hosts of the role
//!    hold a slot than `max_updating` allows (a
//!    host holds a slot while updating, after a failed update until it heartbeats healthy,
//!    and from the order the coordinator gave it until the host reports it or the order
//!    ages out, [`crate::fleet::ORDER_AGE`]);
//! 6. the role waits for the roles its definition names (`kbf`: a frontend waits for every
//!    live storage, daemon and Mac host) and the service for the services in its `after`: each
//!    live host of those runs that service's desired version and is done updating;
//! 7. the hosts that would still be up without this one number at least the minimum: the
//!    role's `min_up` (`kbf`: `min_up.storage` for a storage host, `min_up.frontend` for a
//!    frontend), and at least one if the role is never the last (`kbf`: frontends), whatever
//!    the setting says. A host is up when it is live, healthy, not
//!    updating and not under an outstanding order, so a host that has not heartbeated since
//!    a restart counts as missing.
//!
//! The decision records nothing: the caller notes an `Update` it returns with
//! [`Fleet::order`], under the same lock, so the next host's decision sees it. A host that
//! asks again while its own order is outstanding is told the same again: its own order never
//! counts against it, and the others' slots have not changed.
//!
//! The reply carries the desired version and nothing else.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::NodeId;
use kbf_updater::wire::{Outcome, Reply};

use crate::desired::{Desired, KBF};
use crate::fleet::{Fleet, Host};
use crate::services::{ServiceDef, Services};

/// What to tell the `kbf` host `id` at `now`, when `kbf` is the only service.
#[must_use]
pub fn decide(desired: &Desired, fleet: &Fleet, id: &NodeId, now: Duration) -> Reply {
    let all = BTreeMap::from([(KBF.to_owned(), desired.clone())]);
    decide_in(&Services::default(), &all, KBF, fleet, id, now)
}

/// What to tell the host `id` about `service` at `now`, given every service's `desired`
/// state. A service with no definition or no desired state is told nothing.
#[must_use]
pub fn decide_in(
    services: &Services,
    desired: &BTreeMap<String, Desired>,
    service: &str,
    fleet: &Fleet,
    id: &NodeId,
    now: Duration,
) -> Reply {
    let (Some(def), Some(want)) = (services.get(service), desired.get(service)) else {
        return Reply::Nothing;
    };
    if order(def, want, desired, fleet, id, now) {
        Reply::Update {
            version: want.version.clone(),
        }
    } else {
        Reply::Nothing
    }
}

fn order(
    def: &ServiceDef,
    desired: &Desired,
    all: &BTreeMap<String, Desired>,
    fleet: &Fleet,
    id: &NodeId,
    now: Duration,
) -> bool {
    let service = def.name.as_str();
    let Some(host) = fleet.get_in(service, id) else {
        return false;
    };
    let Some(role) = def.roles.get(&host.role) else {
        return false;
    };
    let others = || fleet.hosts_of(service).filter(|(other, _)| *other != id);
    let failed_here = host
        .beat
        .last_update
        .as_ref()
        .is_some_and(|r| r.outcome == Outcome::Failed && r.target == desired.version);
    let slots = others()
        .filter(|(o, h)| h.role == host.role && fleet.holds_slot(service, o, h, now))
        .count();
    !fleet.is_quiet(now)
        && host.beat.updating.is_none()
        && !fleet.is_busy_besides(service, id, now)
        && host.beat.version != desired.version
        && (failed_here || !failed_here)
        && slots < role.max_updating(host.role, desired)
        && others().all(|(_, h)| !role.after.contains(&h.role) || !is_pending(h, desired, now))
        && def.after.iter().all(|earlier| {
            all.get(earlier).is_none_or(|want| {
                fleet
                    .hosts_of(earlier)
                    .all(|(_, h)| !is_pending(h, want, now))
            })
        })
        && others()
            .filter(|(o, h)| h.role == host.role && fleet.is_up(service, o, h, now))
            .count()
            >= role.required_up(host.role, desired)
}

/// Whether `h` is live and not yet at `desired`, or still updating: what a later role or
/// service must wait for.
fn is_pending(h: &Host, desired: &Desired, now: Duration) -> bool {
    h.is_live(now) && (h.beat.version != desired.version || h.beat.updating.is_some())
}
