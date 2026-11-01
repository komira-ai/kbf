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
//! 5. the host did not decline an update to this version within its wait
//!    ([`crate::defer`]; a deferral is not a failure, holds no slot, and a new desired
//!    version lifts it);
//! 6. the service's canary gate, if it has one, is open to the host ([`crate::canary`]; a
//!    host that is deferring is not chosen as a canary);
//! 7. the service runs on the host's role, and the role has room: fewer hosts of the role
//!    hold a slot than `max_updating` allows (a
//!    host holds a slot while updating, while its promotion is pending, after a failed update
//!    until it heartbeats healthy, from the order the coordinator gave it until the host
//!    reports it or the order ages out, [`crate::fleet::ORDER_AGE`], and, at the desired
//!    version, until it has beaten healthy `bake_beats` times, [`crate::bake`]);
//! 8. the role waits for the roles its definition names (`kbf`: a frontend waits for every
//!    live storage, daemon and Mac host) and the service for the services in its `after`: each
//!    live host of those runs that service's desired version and is done updating;
//! 9. the hosts that would still be up without this one number at least the minimum: the
//!    role's `min_up` (`kbf`: `min_up.storage` for a storage host, `min_up.frontend` for a
//!    frontend), and at least one if the role is never the last (`kbf`: frontends), whatever
//!    the setting says. A host is up when it is live, healthy, not
//!    updating, not waiting for its promotion and not under an outstanding order, so a host that has not heartbeated since
//!    a restart counts as missing.
//!
//! A version marked bad ([`crate::reconcile`]) is ordered to nobody, whatever the rules above
//! say, and is held at [`Hold::Bad`] after the quiet start.
//! A host that reports the service `absent` (declared, never registered) is held back after those two
//! and before the rest, unless the definition says `install_when_absent` ([`Hold::Absent`]).
//!
//! The decision records nothing: the caller notes an `Update` it returns with
//! [`Fleet::order`], under the same lock, so the next host's decision sees it. A host that
//! asks again while its own order is outstanding is told the same again: its own order never
//! counts against it, and the others' slots have not changed.
//!
//! The reply carries the desired version and nothing else. [`decide_why`] is the same
//! decision that also says which rule held ([`Hold`]); [`decide_in`] drops the reason.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::NodeId;
use kbf_updater::version::Version;
use kbf_updater::wire::{Health, Outcome, Reply};
use serde::Serialize;

use crate::canary;
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
/// state. A service with no definition or no desired state is told nothing. This is
/// [`decide_why`] without the reason.
#[must_use]
pub fn decide_in(
    services: &Services,
    desired: &BTreeMap<String, Desired>,
    service: &str,
    fleet: &Fleet,
    id: &NodeId,
    now: Duration,
) -> Reply {
    match decide_why(services, desired, service, fleet, id, now) {
        Decision::Update { version } => Reply::Update { version },
        Decision::Hold(_) => Reply::Nothing,
    }
}

/// Which rule held an order back: the first that failed, in the order the module lists them
/// (the lookups before them come first: the definition, the desired state, the host). The
/// status says it in snake case (`no_slot`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Hold {
    /// The service has no definition.
    NoDefinition,
    /// The service has no desired state.
    NoDesired,
    /// The coordinator has not heard from the host for the service, or the service does not
    /// run on the host's role.
    NotAMember,
    /// The coordinator started less than [`crate::fleet::QUIET_START`] ago.
    Quiet,
    /// The service is declared on the host and has never registered, and the definition does
    /// not say `install_when_absent`.
    Absent,
    /// The host is updating this service.
    Updating,
    /// The host is updating another service or has an order outstanding for one.
    BusyElsewhere,
    /// The host runs the desired version.
    UpToDate,
    /// The host failed an update to the desired version.
    FailedHere,
    /// The host declined an update to the desired version and its wait is not over.
    Deferred,
    /// Every slot of the host's role is held.
    NoSlot,
    /// A role of the service that this one waits for has hosts behind or updating.
    WaitingForRole,
    /// A service that this one waits for has hosts behind or updating.
    WaitingForService,
    /// Fewer hosts of the role would stay up than the role requires.
    MinUp,
    /// Every slot of the host's role is held, at least one of them by a host that runs the
    /// desired version and has not yet baked.
    Baking,
    /// The service's canary gate is closed and the host is not one of the canaries.
    CanaryWait,
    /// The desired version is marked bad ([`crate::reconcile`]).
    Bad,
}

/// What [`decide_why`] found for one host and service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Tell the host to update to `version`.
    Update {
        /// The desired version.
        version: Version,
    },
    /// Tell it nothing, for this reason.
    Hold(Hold),
}

/// The decision for the host `id` about `service` at `now`, with the rule that held when it
/// is not an order, for a service none of whose versions is marked bad. [`decide_in`] drops
/// the reason.
#[must_use]
pub fn decide_why(
    services: &Services,
    desired: &BTreeMap<String, Desired>,
    service: &str,
    fleet: &Fleet,
    id: &NodeId,
    now: Duration,
) -> Decision {
    decide_marked(services, desired, &[], service, fleet, id, now)
}

/// [`decide_why`] for a service whose versions `bad` are marked bad: the only implementation
/// of the rules.
#[must_use]
pub fn decide_marked(
    services: &Services,
    desired: &BTreeMap<String, Desired>,
    bad: &[Version],
    service: &str,
    fleet: &Fleet,
    id: &NodeId,
    now: Duration,
) -> Decision {
    let Some(def) = services.get(service) else {
        return Decision::Hold(Hold::NoDefinition);
    };
    let Some(want) = desired.get(service) else {
        return Decision::Hold(Hold::NoDesired);
    };
    match first_hold(def, want, desired, bad, fleet, id, now) {
        Some(why) => Decision::Hold(why),
        None => Decision::Update {
            version: want.version.clone(),
        },
    }
}

/// The first rule that fails for the host `id`, or `None` when every rule holds.
fn first_hold(
    def: &ServiceDef,
    desired: &Desired,
    all: &BTreeMap<String, Desired>,
    bad: &[Version],
    fleet: &Fleet,
    id: &NodeId,
    now: Duration,
) -> Option<Hold> {
    let service = def.name.as_str();
    let member = fleet
        .get_in(service, id)
        .and_then(|h| def.roles.get(h.role_name()).map(|role| (h, role)));
    let Some((host, role)) = member else {
        return Some(Hold::NotAMember);
    };
    let others = || fleet.hosts_of(service).filter(|(other, _)| *other != id);
    let failed_here = host
        .beat
        .last_update
        .as_ref()
        .is_some_and(|r| r.outcome == Outcome::Failed && r.target == desired.version);
    let in_role = || others().filter(|(_, h)| h.role_name() == host.role_name());
    let slots = in_role()
        .filter(|(o, h)| fleet.holds_slot(service, o, h, now))
        .count();
    let baking = in_role()
        .filter(|(o, h)| {
            !fleet.holds_slot(service, o, h, now)
                && fleet.is_baking(service, o, h, &desired.version, def.bake_beats, now)
        })
        .count();
    let rules = [
        (fleet.is_quiet(now), Hold::Quiet),
        (bad.contains(&desired.version), Hold::Bad),
        (
            host.beat.health == Health::Absent && !def.install_when_absent,
            Hold::Absent,
        ),
        (host.beat.updating.is_some(), Hold::Updating),
        (fleet.is_busy_besides(service, id, now), Hold::BusyElsewhere),
        (host.beat.version == desired.version, Hold::UpToDate),
        (failed_here, Hold::FailedHere),
        (
            fleet
                .deferred_until(service, id, &desired.version, &def.defer_backoff)
                .is_some_and(|until| now < until),
            Hold::Deferred,
        ),
        (
            canary::closed_to(def, desired, fleet, id, host, now),
            Hold::CanaryWait,
        ),
        (
            slots >= role.max_updating(host.role_name(), desired),
            Hold::NoSlot,
        ),
        (
            slots + baking >= role.max_updating(host.role_name(), desired),
            Hold::Baking,
        ),
        (
            others().any(|(_, h)| {
                role.after.iter().any(|r| r == h.role_name()) && is_pending(h, desired, now)
            }),
            Hold::WaitingForRole,
        ),
        (
            !def.after.iter().all(|earlier| {
                all.get(earlier).is_none_or(|want| {
                    fleet
                        .hosts_of(earlier)
                        .all(|(_, h)| !is_pending(h, want, now))
                })
            }),
            Hold::WaitingForService,
        ),
        (
            others()
                .filter(|(o, h)| {
                    h.role_name() == host.role_name() && fleet.is_up(service, o, h, now)
                })
                .count()
                < role.required_up(host.role_name(), desired),
            Hold::MinUp,
        ),
    ];
    rules
        .into_iter()
        .find_map(|(holds, why)| holds.then_some(why))
}

/// Whether `h` is live and not yet at `desired`, or still updating: what a later role or
/// service must wait for.
fn is_pending(h: &Host, desired: &Desired, now: Duration) -> bool {
    h.is_live(now) && (h.beat.version != desired.version || h.beat.updating.is_some())
}
