//! Which hosts a service counts (`docs/design/host-updates.md`, section 5.1).
//!
//! Without a selector a host belongs to a service if its own heartbeat names it and the
//! service runs on its role: the hosts the coordinator has heard announce it. A service with a
//! [`Selector`] says instead which hosts should run it, by certificate role and by the labels
//! an operator gave the host ([`crate::ops`]; a host never sets its own), and a selected host
//! that did not announce the service is `missing`. [`membership`] places one node, checking in
//! this order:
//!
//! 1. an operator excluded it from the service: [`Membership::Excluded`];
//! 2. it is not selected: [`Membership::NotSelected`];
//! 3. it has been silent for [`crate::fleet::DOWN_AFTER`]: [`Membership::Down`], so a host
//!    that went quiet is down, never missing;
//! 4. it reports the service `absent`: [`Membership::Absent`];
//! 5. the service has a selector and the agent did not announce [`HEARTBEAT_V2`]:
//!    [`Membership::Unmanaged`], since an agent that cannot list the service would be missing
//!    for ever;
//! 6. otherwise it is [`Membership::Present`] if it announced the service and
//!    [`Membership::Missing`] if not.
//!
//! Only `Present` and `Absent` hosts are in a service's hosts ([`standing`]); the others are
//! counted in [`HostCounts`], which the convergence shows and which keeps it from reading
//! converged while a host is missing or absent. Only hosts the coordinator has heard from are
//! known: a host never heard from is not missing (the quiet start covers a restart).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use kbf_types::NodeId;
use kbf_updater::version::Version;
use kbf_updater::wire::Health;
use serde::{Deserialize, Serialize};

use crate::converge::{Convergence, State};
use crate::fleet::{Fleet, Node};
use crate::ops::{MAX_LABELS, OpsState, label};
use crate::services::ServiceDef;

/// The feature of an agent that lists every service it runs, `kbf` included, and may say
/// `absent`.
pub const HEARTBEAT_V2: &str = "heartbeat-v2";

/// A selector as a services file spells it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSelector {
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

/// Which hosts should run a service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selector {
    /// The roles selected; always roles of the service.
    roles: BTreeSet<String>,
    /// The labels a host must carry, each with this value.
    labels: BTreeMap<String, String>,
}

impl Selector {
    /// The selector of `raw` over a service whose roles are `service_roles`: no roles means
    /// all of them.
    ///
    /// # Errors
    /// What is wrong, as text: a role the service lacks, or labels beyond the grammar of the
    /// operator's labels.
    pub fn new(raw: RawSelector, service_roles: &BTreeSet<String>) -> Result<Self, String> {
        if let Some(role) = raw.roles.iter().find(|r| !service_roles.contains(*r)) {
            return Err(format!("names {role}, a role it does not have"));
        }
        if raw.labels.len() > MAX_LABELS || !raw.labels.iter().all(|(k, v)| label(k) && label(v)) {
            return Err(
                "has a bad label: at most 8, each key and value 1 to 32 bytes of [a-z0-9-]"
                    .to_owned(),
            );
        }
        let roles = if raw.roles.is_empty() {
            service_roles.clone()
        } else {
            raw.roles.into_iter().collect()
        };
        Ok(Self {
            roles,
            labels: raw.labels,
        })
    }

    fn selects(&self, role: &str, has: Option<&BTreeMap<String, String>>) -> bool {
        self.roles.contains(role)
            && self
                .labels
                .iter()
                .all(|(k, v)| has.and_then(|l| l.get(k)) == Some(v))
    }
}

/// Where a node stands for one service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Membership {
    /// An operator took it out of the service.
    Excluded,
    /// The service is not meant for it.
    NotSelected,
    /// Selected, and silent for [`crate::fleet::DOWN_AFTER`] or more.
    Down,
    /// Declared on the host and never registered.
    Absent,
    /// Selected and announced.
    Present,
    /// Selected, live, with an agent that lists services, and did not announce it.
    Missing,
    /// Selected, with an agent that cannot list services.
    Unmanaged,
}

/// Where the node `id` stands for `def` at `now`.
#[must_use]
pub fn membership(
    def: &ServiceDef,
    id: &NodeId,
    node: &Node,
    ops: &OpsState,
    now: Duration,
) -> Membership {
    let key = id.to_string();
    if ops
        .excluded
        .iter()
        .any(|e| e.service == def.name && e.node_id == key)
    {
        return Membership::Excluded;
    }
    let announced = node.announced.get(&def.name);
    let selected = match &def.selector {
        Some(selector) => {
            selector.selects(&node.role, ops.hosts.get(&key).map(|host| &host.labels))
        }
        None => announced.is_some() && def.roles.contains_key(&node.role),
    };
    if !selected {
        Membership::NotSelected
    } else if !node.is_live(now) {
        Membership::Down
    } else if announced == Some(&Health::Absent) {
        Membership::Absent
    } else if def.selector.is_some() && !node.features.iter().any(|f| f == HEARTBEAT_V2) {
        Membership::Unmanaged
    } else if announced.is_some() {
        Membership::Present
    } else {
        Membership::Missing
    }
}

/// What a heartbeat does to a node's place among a service's hosts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standing {
    /// Its word about the service is recorded.
    Counted,
    /// Its word about the service is not recorded, and what it said before is forgotten.
    Dropped,
    /// Not recorded, and what it said before stands: a service without a selector keeps the
    /// hosts it heard, as it always has, and a down host stays counted as down.
    Ignored,
}

/// The standing of the node `id` among the hosts of `def`.
#[must_use]
pub fn standing(
    def: &ServiceDef,
    id: &NodeId,
    node: &Node,
    ops: &OpsState,
    now: Duration,
) -> Standing {
    match membership(def, id, node, ops, now) {
        Membership::Present | Membership::Absent => Standing::Counted,
        Membership::Excluded => Standing::Dropped,
        Membership::NotSelected | Membership::Unmanaged | Membership::Missing
            if def.selector.is_some() =>
        {
            Standing::Dropped
        }
        _ => Standing::Ignored,
    }
}

/// A node the convergence does not count, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Member {
    /// The node id.
    pub node_id: String,
    /// Its role.
    pub role: String,
    /// Why it is not counted.
    pub membership: Membership,
    /// The operator's reason, for an excluded host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The nodes `def` does not count because they are excluded, absent, missing or unmanaged.
#[must_use]
pub fn members(def: &ServiceDef, fleet: &Fleet, ops: &OpsState, now: Duration) -> Vec<Member> {
    fleet
        .nodes()
        .filter_map(|(id, node)| {
            let membership = membership(def, id, node, ops, now);
            let key = id.to_string();
            let reason = ops
                .excluded
                .iter()
                .find(|e| e.service == def.name && e.node_id == key)
                .map(|e| e.reason.clone());
            matches!(
                membership,
                Membership::Excluded
                    | Membership::Absent
                    | Membership::Missing
                    | Membership::Unmanaged
            )
            .then(|| Member {
                node_id: id.to_string(),
                role: node.role.clone(),
                membership,
                reason,
            })
        })
        .collect()
}

/// How many nodes a service does not count, by why.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct HostCounts {
    /// Selected and live, but never announced the service.
    pub missing: usize,
    /// Declared and never registered.
    pub absent: usize,
    /// Taken out by an operator.
    pub excluded: usize,
    /// Selected, with an agent that cannot list services.
    pub unmanaged: usize,
    /// Declined the update to the desired version, and still within their wait
    /// ([`Fleet::deferred_until`]). They are counted in `roles` as well: this says why they
    /// are not being ordered.
    pub deferred: usize,
}

impl HostCounts {
    /// The counts of `members`.
    #[must_use]
    pub fn of(members: &[Member]) -> Self {
        let count = |m: Membership| members.iter().filter(|x| x.membership == m).count();
        Self {
            missing: count(Membership::Missing),
            absent: count(Membership::Absent),
            excluded: count(Membership::Excluded),
            unmanaged: count(Membership::Unmanaged),
            deferred: 0,
        }
    }
}

/// How many hosts of `def` are within the wait that follows a deferral of `version` at `now`.
#[must_use]
pub fn deferred_count(
    def: &ServiceDef,
    fleet: &Fleet,
    version: Option<&Version>,
    now: Duration,
) -> usize {
    let Some(version) = version else {
        return 0;
    };
    fleet
        .hosts_of(&def.name)
        .filter(|(_, host)| host.is_live(now))
        .filter(|(id, _)| {
            fleet
                .deferred_until(&def.name, id, version, &def.defer_backoff)
                .is_some_and(|until| now < until)
        })
        .count()
}

/// `convergence` with the hosts `def` does not count, and the `deferred` hosts it does: they
/// are shown when `def` has a selector or there is any, and a farm with a missing or an
/// absent host is not converged.
#[must_use]
pub fn account(
    mut convergence: Convergence,
    def: &ServiceDef,
    members: &[Member],
    deferred: usize,
) -> Convergence {
    let counts = HostCounts {
        deferred,
        ..HostCounts::of(members)
    };
    if counts.missing + counts.absent > 0 && convergence.state == State::Converged {
        convergence.state = State::InProgress;
    }
    convergence.hosts =
        (def.selector.is_some() || counts != HostCounts::default()).then_some(counts);
    convergence
}
