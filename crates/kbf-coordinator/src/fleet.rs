//! What the coordinator knows of the hosts: the last heartbeat of each, and when it came.
//!
//! Nothing here is stored. After a restart the coordinator knows no host until it
//! heartbeats, which is why the minimums are counted against the heartbeats it has seen and
//! never against a host list (`docs/design/updates.md`, section 3). Time is a `Duration`
//! since the coordinator started, so the decision replays exactly in a test.
//!
//! The orders it has given are kept the same way, in memory only. A host reports that it
//! is updating on its first heartbeat after it hears the order, so until then the beats say
//! nothing is happening; an outstanding order stands in for that report, holding the host's
//! slot and counting the host as not up. Hosts and orders are kept per service; `kbf`'s
//! heartbeat is the top-level one on the wire, so [`Fleet::record`], [`Fleet::get`] and
//! [`Fleet::hosts`] are the `kbf` forms of the per-service calls. A restart forgets the
//! outstanding orders: the start-up quiet period lets the hosts report first, and a host
//! that began updating says so.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::{HostRole, NodeId};
use kbf_updater::version::Version;
use kbf_updater::wire::{Health, Heartbeat, Outcome};

use crate::bake::Bake;
use crate::defer::{DeferBackoff, Defers};
use crate::desired::KBF;

/// A host with no heartbeat for this long is down.
pub const DOWN_AFTER: Duration = Duration::from_secs(45);

/// After it starts, the coordinator orders no update for this long, to hear the hosts first.
pub const QUIET_START: Duration = Duration::from_secs(30);

/// How long an update order holds its slot without the host reporting it. A host reports
/// within one heartbeat interval (15 s) of hearing the order, so this is eight intervals:
/// room for a lost reply and several missed beats, short enough that a host that never heard
/// frees its slot before an operator would notice.
pub const ORDER_AGE: Duration = Duration::from_secs(120);

/// An update the coordinator told a host to run, not yet seen in the host's own heartbeats.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Order {
    target: Version,
    at: Duration,
}

/// A host, as its last heartbeat and certificate left it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    /// What the host runs, from its certificate, by name. Read it with [`Host::role_name`].
    role: String,
    /// What it last said.
    pub beat: Heartbeat,
    /// When it said it.
    pub seen: Duration,
}

impl Host {
    /// The host's role, by name: how coordinator logic reads a role.
    #[must_use]
    pub fn role_name(&self) -> &str {
        &self.role
    }

    /// Whether the host has heartbeated within [`DOWN_AFTER`] of `now`.
    #[must_use]
    pub fn is_live(&self, now: Duration) -> bool {
        now.saturating_sub(self.seen) < DOWN_AFTER
    }

    /// Whether the host counts as up: live, healthy, not in the middle of an update and not
    /// waiting for its promotion.
    #[must_use]
    pub fn is_up(&self, now: Duration) -> bool {
        self.is_live(now)
            && self.beat.health == Health::Healthy
            && self.beat.updating.is_none()
            && !self.beat.promotion_pending
    }

    /// Whether the host holds an update slot (one is live at a time across every service,
    /// [`crate::decide`]): it is updating, its promotion is pending, or its last update failed
    /// and it has not yet heartbeated healthy.
    #[must_use]
    pub fn holds_slot(&self) -> bool {
        let failed = self
            .beat
            .last_update
            .as_ref()
            .is_some_and(|r| r.outcome == Outcome::Failed);
        self.beat.updating.is_some()
            || self.beat.promotion_pending
            || (failed && self.beat.health != Health::Healthy)
    }
}

/// A host the coordinator has heard from, whatever it runs: noted for every heartbeat, so a
/// host is known without a `kbf` beat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// What the host runs, from its certificate, by name.
    pub role: String,
    /// The features its agent announced.
    pub features: Vec<String>,
    /// When it last heartbeated.
    pub seen: Duration,
    /// The services its last heartbeat named, with the health it gave each (`kbf`: the top
    /// level).
    pub announced: BTreeMap<String, Health>,
}

impl Node {
    /// Whether the node has heartbeated within [`DOWN_AFTER`] of `now`.
    #[must_use]
    pub fn is_live(&self, now: Duration) -> bool {
        now.saturating_sub(self.seen) < DOWN_AFTER
    }
}

/// Every host the coordinator has heard from since it started.
#[derive(Clone, Debug)]
pub struct Fleet {
    started: Duration,
    nodes: BTreeMap<NodeId, Node>,
    hosts: BTreeMap<String, BTreeMap<NodeId, Host>>,
    /// What hosts the service no longer counts (excluded, or dropped by its selector) last
    /// said of it, kept so that an update they still run holds its slot
    /// ([`Self::occupants_of`]). A host is in `hosts` or here for a service, never both.
    outside: BTreeMap<String, BTreeMap<NodeId, Host>>,
    orders: BTreeMap<(NodeId, String), Order>,
    /// Healthy beats in a row at one version, per service and host ([`Bake`]).
    bake: BTreeMap<(String, NodeId), Bake>,
    defers: Defers,
}

impl Fleet {
    /// A fleet that knows no host, for a coordinator that started at `started`.
    #[must_use]
    pub fn new(started: Duration) -> Self {
        Self {
            started,
            nodes: BTreeMap::new(),
            hosts: BTreeMap::new(),
            outside: BTreeMap::new(),
            orders: BTreeMap::new(),
            bake: BTreeMap::new(),
            defers: Defers::default(),
        }
    }

    /// Whether `now` is still within [`QUIET_START`] of the coordinator's start.
    #[must_use]
    pub fn is_quiet(&self, now: Duration) -> bool {
        now < self.started.saturating_add(QUIET_START)
    }

    /// When the coordinator started.
    #[must_use]
    pub const fn started(&self) -> Duration {
        self.started
    }

    /// Notes that the host `id` heartbeated, as `node` says.
    pub fn note_node(&mut self, id: NodeId, node: Node) {
        self.nodes.insert(id, node);
    }

    /// The node `id`, if it has heartbeated.
    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    /// Every node that has heartbeated, by node id.
    pub fn nodes(&self) -> impl Iterator<Item = (&NodeId, &Node)> {
        self.nodes.iter()
    }

    /// Stops counting the host `id` for `service`: what it said moves out of the service's
    /// hosts, to be kept with any order for it as what the host still occupies
    /// ([`Self::occupants_of`]), until its own beats, recorded with
    /// [`Self::record_outside_in`], or the order's age release it.
    pub fn forget_in(&mut self, service: &str, id: &NodeId) {
        if let Some(host) = self
            .hosts
            .get_mut(service)
            .and_then(|hosts| hosts.remove(id))
        {
            self.outside
                .entry(service.to_owned())
                .or_default()
                .insert(id.clone(), host);
        }
    }

    /// Notes a `kbf` heartbeat: [`Self::record_in`] for [`KBF`].
    pub fn record(&mut self, id: NodeId, role: HostRole, beat: Heartbeat, now: Duration) {
        self.record_in(KBF, id, role, beat, now);
    }

    /// Notes a heartbeat of `service`: the host's latest word about it replaces its earlier
    /// one.
    ///
    /// An order for the host and service ends when the beat shows it updating (the beat
    /// counts from then), runs the ordered version, reports that update failed, or reports
    /// that it declined it ([`crate::defer`]): a deferral holds no slot, so the order must not
    /// hold it for [`ORDER_AGE`] either.
    pub fn record_in(
        &mut self,
        service: &str,
        id: NodeId,
        role: HostRole,
        beat: Heartbeat,
        now: Duration,
    ) {
        self.record_named_in(service, id, role.name(), beat, now);
    }

    /// [`Self::record_in`] for a host whose role is named `role`.
    pub(crate) fn record_named_in(
        &mut self,
        service: &str,
        id: NodeId,
        role: &str,
        beat: Heartbeat,
        now: Duration,
    ) {
        self.note(service, id, role, beat, now, true);
    }

    /// Notes a heartbeat of `service` from a host the service does not count (excluded, or
    /// dropped by its selector): it ends an order and counts a bake as [`Self::record_in`]
    /// does, and is kept only as what the host occupies ([`Self::occupants_of`]), never as one
    /// of the service's hosts.
    pub fn record_outside_in(
        &mut self,
        service: &str,
        id: NodeId,
        role: &HostRole,
        beat: Heartbeat,
        now: Duration,
    ) {
        self.note(service, id, role.name(), beat, now, false);
    }

    /// The one place a role enters the fleet: the beat ends an order, counts a bake and a
    /// deferral, and is kept as one of the service's hosts when `counted`, else as outside.
    fn note(
        &mut self,
        service: &str,
        id: NodeId,
        role: &str,
        beat: Heartbeat,
        now: Duration,
        counted: bool,
    ) {
        let key = (id, service.to_owned());
        let mut deferred_order = false;
        if let Some(order) = self.orders.get(&key) {
            let failed = beat
                .last_update
                .as_ref()
                .is_some_and(|r| r.outcome == Outcome::Failed && r.target == order.target);
            deferred_order = beat
                .deferred
                .as_ref()
                .is_some_and(|d| d.target == order.target);
            if beat.updating.is_some() || beat.version == order.target || failed || deferred_order {
                self.orders.remove(&key);
            }
        }
        let baking = (key.1.clone(), key.0.clone());
        match Bake::after(self.bake.get(&baking), &beat, now) {
            Some(bake) => {
                self.bake.insert(baking, bake);
            }
            None => {
                self.bake.remove(&baking);
            }
        }
        self.defers.observe(&key, &beat, deferred_order, now);
        let (kept, other) = if counted {
            (&mut self.hosts, &mut self.outside)
        } else {
            (&mut self.outside, &mut self.hosts)
        };
        if let Some(hosts) = other.get_mut(&key.1) {
            hosts.remove(&key.0);
        }
        kept.entry(key.1).or_default().insert(
            key.0,
            Host {
                role: role.to_owned(),
                beat,
                seen: now,
            },
        );
    }

    /// Notes that the host `id` was told at `now` to update `service` to `target`. An order
    /// for the same target that has not aged out stays as it was, so a host that asks again
    /// before it reports is told again without its slot being held longer.
    pub fn order(&mut self, service: &str, id: NodeId, target: Version, now: Duration) {
        if self.is_ordered(service, &id, now)
            && self
                .orders
                .get(&(id.clone(), service.to_owned()))
                .is_some_and(|o| o.target == target)
        {
            return;
        }
        self.orders
            .insert((id, service.to_owned()), Order { target, at: now });
    }

    /// When the wait of the host `id` ends, if it declined an update of `service` to `target`.
    #[must_use]
    pub fn deferred_until(
        &self,
        service: &str,
        id: &NodeId,
        target: &Version,
        backoff: &DeferBackoff,
    ) -> Option<Duration> {
        self.defers
            .until(&(id.clone(), service.to_owned()), target, backoff)
    }

    /// Whether `id` has an order outstanding for `service` at `now`.
    #[must_use]
    pub fn is_ordered(&self, service: &str, id: &NodeId, now: Duration) -> bool {
        self.orders
            .get(&(id.clone(), service.to_owned()))
            .is_some_and(|o| now.saturating_sub(o.at) < ORDER_AGE)
    }

    /// Whether the host `id` is busy with a service other than `service`: one of its other
    /// services is updating, or has an order outstanding for it. A host does one update at a
    /// time.
    #[must_use]
    pub fn is_busy_besides(&self, service: &str, id: &NodeId, now: Duration) -> bool {
        let updating = self
            .hosts
            .iter()
            .filter(|(s, _)| s.as_str() != service)
            .any(|(_, hosts)| hosts.get(id).is_some_and(|h| h.beat.updating.is_some()));
        updating
            || self.orders.iter().any(|((o, s), order)| {
                o == id && s != service && now.saturating_sub(order.at) < ORDER_AGE
            })
    }

    /// Whether the host `id` holds an update slot of `service`: [`Host::holds_slot`], or an
    /// order is outstanding for it.
    #[must_use]
    pub fn holds_slot(&self, service: &str, id: &NodeId, host: &Host, now: Duration) -> bool {
        host.holds_slot() || self.is_ordered(service, id, now)
    }

    /// How many healthy beats in a row the host `id` has sent `service` at `version`.
    #[must_use]
    pub fn bake_beats(&self, service: &str, id: &NodeId, version: &Version) -> u32 {
        self.bake
            .get(&(service.to_owned(), id.clone()))
            .filter(|b| b.target == *version)
            .map_or(0, |b| b.beats)
    }

    /// Whether the host `id` is still baking: live, at `version`, and has beaten healthy
    /// fewer than `need` times in a row.
    #[must_use]
    pub fn is_baking(
        &self,
        service: &str,
        id: &NodeId,
        host: &Host,
        version: &Version,
        need: u32,
        now: Duration,
    ) -> bool {
        host.is_live(now)
            && host.beat.version == *version
            && self.bake_beats(service, id, version) < need
    }

    /// Whether the host `id` counts as up for `service`: [`Host::is_up`] and no order
    /// outstanding for it.
    #[must_use]
    pub fn is_up(&self, service: &str, id: &NodeId, host: &Host, now: Duration) -> bool {
        host.is_up(now) && !self.is_ordered(service, id, now)
    }

    /// The host `id` as `service` last heard it, if it has heartbeated.
    #[must_use]
    pub fn get_in(&self, service: &str, id: &NodeId) -> Option<&Host> {
        self.hosts.get(service)?.get(id)
    }

    /// The `kbf` host `id`.
    #[must_use]
    pub fn get(&self, id: &NodeId) -> Option<&Host> {
        self.get_in(KBF, id)
    }

    /// Every host that heartbeated `service`, by node id.
    pub fn hosts_of(&self, service: &str) -> impl Iterator<Item = (&NodeId, &Host)> {
        self.hosts.get(service).into_iter().flatten()
    }

    /// Every host whose last word about `service` the fleet keeps, counted or not: the
    /// service's hosts and the ones it no longer counts. One live update at a time is counted
    /// over these ([`crate::decide`]), so a host that leaves a service mid-update still holds
    /// its slot.
    pub fn occupants_of(&self, service: &str) -> impl Iterator<Item = (&NodeId, &Host)> {
        self.hosts_of(service)
            .chain(self.outside.get(service).into_iter().flatten())
    }

    /// Every `kbf` host, by node id.
    pub fn hosts(&self) -> impl Iterator<Item = (&NodeId, &Host)> {
        self.hosts_of(KBF)
    }
}
