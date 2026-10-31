//! Whether the farm has reached the desired version: the answer CI waits for after a
//! release is published (`docs/deploy/kbf-updater.md`, "Turn on the deploy step").
//!
//! A pure function of the desired state, the fleet the heartbeats built and the time; it
//! stores nothing. A host counts only while it is live (heartbeated within
//! [`crate::fleet::DOWN_AFTER`]); a silent host is `down`, and is not at the desired
//! version whatever it last said.
//!
//! The state is one of three:
//!
//! - `blocked`: some live host failed an update to the desired version and is still behind
//!   it. It stays so until the host is fixed or another version is set (the coordinator
//!   does not tell it again, `decide`).
//! - `converged`: past the quiet start, at least one live host, every live host at the
//!   desired version and none updating or failed, and at least `min_up` hosts of the
//!   storage and of the frontend role up. The last two keep a coordinator that has just
//!   restarted, and has heard from few hosts, from reporting a farm converged.
//! - `in_progress`: everything else.
//!
//! How long a farm has gone without progress is the caller's to judge: it needs the time of
//! the last change, which the coordinator does not keep.

use std::time::Duration;

use kbf_updater::version::Version;
use kbf_updater::wire::Outcome;
use serde::Serialize;

use crate::desired::Desired;
use crate::fleet::{Fleet, Host};
use crate::services::ServiceDef;

/// Where the farm stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Every live host runs the desired version, none is updating or failed, and the
    /// minimums are met.
    Converged,
    /// A live host failed its update to the desired version.
    Blocked,
    /// Neither.
    InProgress,
}

/// The hosts of one role, by where they stand.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RoleCounts {
    /// The role (`frontend`, `storage`, `daemon`, `mac-daemon`, or another a definition names).
    pub role: String,
    /// Live and running the desired version.
    pub at_desired: usize,
    /// Live and updating.
    pub updating: usize,
    /// Live, behind, not updating, and the last update to the desired version failed.
    pub failed: usize,
    /// Silent for [`crate::fleet::DOWN_AFTER`] or more.
    pub down: usize,
}

/// What CI reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Convergence {
    /// The desired version, if one was ever set.
    pub desired: Option<Version>,
    /// The coordinator is still within its quiet period and orders nothing.
    pub quiet: bool,
    /// Where the farm stands.
    pub state: State,
    /// The counts of every role that has a host, in role order.
    pub roles: Vec<RoleCounts>,
    /// The commit this coordinator was built from ([`crate::BUILD_COMMIT`]): what a client
    /// compares with a release before it orders one.
    pub commit: &'static str,
}

fn failed_to_reach(host: &Host, desired: &Desired) -> bool {
    host.beat.version != desired.version
        && host.beat.updating.is_none()
        && host
            .beat
            .last_update
            .as_ref()
            .is_some_and(|r| r.outcome == Outcome::Failed && r.target == desired.version)
}

fn counts(
    service: &str,
    role: &str,
    desired: &Desired,
    fleet: &Fleet,
    now: Duration,
) -> RoleCounts {
    let mine = || {
        fleet
            .hosts_of(service)
            .map(|(_, h)| h)
            .filter(move |h| h.role_name() == role)
    };
    RoleCounts {
        role: role.to_owned(),
        at_desired: mine()
            .filter(|h| h.is_live(now) && h.beat.version == desired.version)
            .count(),
        updating: mine()
            .filter(|h| h.is_live(now) && h.beat.updating.is_some())
            .count(),
        failed: mine()
            .filter(|h| h.is_live(now) && failed_to_reach(h, desired))
            .count(),
        down: mine().filter(|h| !h.is_live(now)).count(),
    }
}

/// The counts of every role of `def` that has a host, in role order
/// ([`ServiceDef::role_names`]); none when no desired state was set.
#[must_use]
pub fn role_counts(
    def: &ServiceDef,
    desired: Option<&Desired>,
    fleet: &Fleet,
    now: Duration,
) -> Vec<RoleCounts> {
    let Some(desired) = desired else {
        return Vec::new();
    };
    def.role_names()
        .into_iter()
        .map(|role| counts(&def.name, role, desired, fleet, now))
        .filter(|c| c.at_desired + c.updating + c.failed + c.down > 0)
        .collect()
}

/// Where the farm stands at `now` against `desired`, for `kbf` as shipped.
#[must_use]
pub fn converge(desired: Option<&Desired>, fleet: &Fleet, now: Duration) -> Convergence {
    converge_kbf(&ServiceDef::kbf(), desired, fleet, now)
}

/// Where the hosts of `kbf`, as `def` defines it (the shipped definition or a replacement),
/// stand at `now` against `desired`: the answer clients of `kbf` have always read, so the
/// minimum of hosts up per role is the desired state's `min_up` alone, without the floor of
/// a `never_last` role that [`converge_service`] applies.
#[must_use]
pub fn converge_kbf(
    def: &ServiceDef,
    desired: Option<&Desired>,
    fleet: &Fleet,
    now: Duration,
) -> Convergence {
    converge_with(def, desired, fleet, now, |d, role| {
        d.min_up.min_up_of(role) as usize
    })
}

/// Where the hosts of the service `def` stand at `now` against its `desired`: like
/// [`converge`] over the hosts that heartbeated the service, with the minimum of hosts up
/// per role being what the service requires of the role
/// ([`crate::services::RoleDef::required_up`]).
#[must_use]
pub fn converge_service(
    def: &ServiceDef,
    desired: Option<&Desired>,
    fleet: &Fleet,
    now: Duration,
) -> Convergence {
    converge_with(def, desired, fleet, now, |d, role| {
        def.roles.get(role).map_or(0, |r| r.required_up(role, d))
    })
}

fn converge_with(
    def: &ServiceDef,
    desired: Option<&Desired>,
    fleet: &Fleet,
    now: Duration,
    minimum: impl Fn(&Desired, &str) -> usize,
) -> Convergence {
    let quiet = fleet.is_quiet(now);
    let Some(desired) = desired else {
        return Convergence {
            desired: None,
            quiet,
            state: State::InProgress,
            roles: Vec::new(),
            commit: crate::BUILD_COMMIT,
        };
    };
    let service = def.name.as_str();
    let roles = role_counts(def, Some(desired), fleet, now);
    let live = fleet
        .hosts_of(service)
        .filter(|(_, h)| def.roles.contains_key(h.role_name()) && h.is_live(now))
        .count();
    let at_desired: usize = roles.iter().map(|c| c.at_desired).sum();
    let clear = roles.iter().all(|c| c.updating == 0 && c.failed == 0);
    let up = |role: &str| {
        fleet
            .hosts_of(service)
            .filter(|(_, h)| h.role_name() == role && h.is_up(now))
            .count()
    };
    let state = if roles.iter().any(|c| c.failed > 0) {
        State::Blocked
    } else if !quiet
        && live > 0
        && at_desired == live
        && clear
        && def
            .role_names()
            .into_iter()
            .all(|role| up(role) >= minimum(desired, role))
    {
        State::Converged
    } else {
        State::InProgress
    };
    Convergence {
        desired: Some(desired.version.clone()),
        quiet,
        state,
        roles,
        commit: crate::BUILD_COMMIT,
    }
}
