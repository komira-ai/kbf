//! The canary gate of a service (`docs/design/host-updates.md`, section 5.3): while it is
//! closed only the first hosts are ordered.
//!
//! A service with `canary = { count = n }` orders at most `n` hosts until `n` hosts at the
//! desired version have baked. The roles and labels form of the canary is not implemented:
//! the services file refuses it.
//!
//! The hosts that have *started* are those updating, under an order, failed at the desired
//! version, or at it: silent ones too, so a canary that went down after it began is not
//! replaced. The next hosts are chosen among the live ones that have not started, the roles
//! that wait for no other role first and then by node id, leaving out the hosts that are
//! deferring: the smallest id alone could pick a host of a role that cannot go until another
//! has, and the gate would never open.

use std::time::Duration;

use kbf_types::NodeId;
use kbf_updater::wire::Outcome;

use crate::desired::Desired;
use crate::fleet::{Fleet, Host};
use crate::services::ServiceDef;

/// The hosts of a service that go alone first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Canary {
    /// How many hosts.
    pub count: u32,
}

/// Whether the canary gate of `def` is closed to the host `id` at `now`: the service has a
/// canary, fewer than `count` hosts have baked at the desired version, and `id` (the host
/// `me`, a member of the service) is not among the hosts that may still be ordered. A host under its own order is never held:
/// it is told the same again.
#[must_use]
pub(crate) fn closed_to(
    def: &ServiceDef,
    want: &Desired,
    fleet: &Fleet,
    id: &NodeId,
    me: &Host,
    now: Duration,
) -> bool {
    let Some(canary) = def.canary else {
        return false;
    };
    let service = def.name.as_str();
    let members = || {
        fleet
            .hosts_of(service)
            .filter(|(_, h)| def.roles.contains_key(h.role_name()))
    };
    let started = |o: &NodeId, h: &Host| {
        h.beat.updating.is_some()
            || fleet.is_ordered(service, o, now)
            || h.beat.version == want.version
            || h.beat
                .last_update
                .as_ref()
                .is_some_and(|r| r.outcome == Outcome::Failed && r.target == want.version)
    };
    let baked = members()
        .filter(|(o, h)| {
            h.beat.version == want.version
                && fleet.is_up(service, o, h, now)
                && fleet.bake_beats(service, o, &want.version) >= def.bake_beats
        })
        .count();
    if baked >= canary.count as usize {
        return false;
    }
    // A host that declined the update is left alone: it is not chosen, and the next host is.
    let deferring = |o: &NodeId| {
        fleet
            .deferred_until(service, o, &want.version, &def.defer_backoff)
            .is_some_and(|until| now < until)
    };
    // The key a host is chosen by: the roles that wait for no role first, then the id.
    let key = |o: &NodeId, h: &Host| {
        let waits = def
            .roles
            .get(h.role_name())
            .is_some_and(|r| !r.after.is_empty());
        (waits, o.clone())
    };
    if started(id, me) {
        return false;
    }
    let mine = key(id, me);
    let before = members()
        .filter(|(o, h)| h.is_live(now) && !started(o, h) && !deferring(o) && key(o, h) < mine)
        .count();
    let free =
        (canary.count as usize).saturating_sub(members().filter(|(o, h)| started(o, h)).count());
    before >= free
}
