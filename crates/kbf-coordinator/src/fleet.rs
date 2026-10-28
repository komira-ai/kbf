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
//! slot and counting the host as not up. A restart forgets the outstanding orders: the
//! start-up quiet period lets the hosts report first, and a host that began updating says so.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::{HostRole, NodeId};
use kbf_updater::version::Version;
use kbf_updater::wire::{Health, Heartbeat, Outcome};

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
    /// What the host runs, from its certificate.
    pub role: HostRole,
    /// What it last said.
    pub beat: Heartbeat,
    /// When it said it.
    pub seen: Duration,
}

impl Host {
    /// Whether the host has heartbeated within [`DOWN_AFTER`] of `now`.
    #[must_use]
    pub fn is_live(&self, now: Duration) -> bool {
        now.saturating_sub(self.seen) < DOWN_AFTER
    }

    /// Whether the host counts as up: live, healthy, and not in the middle of an update.
    #[must_use]
    pub fn is_up(&self, now: Duration) -> bool {
        self.is_live(now) && self.beat.health == Health::Healthy && self.beat.updating.is_none()
    }

    /// Whether the host holds one of its role's update slots: it is updating, or its last
    /// update failed and it has not yet heartbeated healthy.
    #[must_use]
    pub fn holds_slot(&self) -> bool {
        let failed = self
            .beat
            .last_update
            .as_ref()
            .is_some_and(|r| r.outcome == Outcome::Failed);
        self.beat.updating.is_some() || (failed && self.beat.health != Health::Healthy)
    }
}

/// Every host the coordinator has heard from since it started.
#[derive(Clone, Debug)]
pub struct Fleet {
    started: Duration,
    hosts: BTreeMap<NodeId, Host>,
    orders: BTreeMap<NodeId, Order>,
}

impl Fleet {
    /// A fleet that knows no host, for a coordinator that started at `started`.
    #[must_use]
    pub fn new(started: Duration) -> Self {
        Self {
            started,
            hosts: BTreeMap::new(),
            orders: BTreeMap::new(),
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

    /// Notes a heartbeat: the host's latest word replaces its earlier one.
    ///
    /// An order for the host ends when the beat shows it updating (the beat counts from
    /// then), runs the ordered version, or reports that update failed.
    pub fn record(&mut self, id: NodeId, role: HostRole, beat: Heartbeat, now: Duration) {
        if let Some(order) = self.orders.get(&id) {
            let failed = beat
                .last_update
                .as_ref()
                .is_some_and(|r| r.outcome == Outcome::Failed && r.target == order.target);
            if beat.updating.is_some() || beat.version == order.target || failed {
                self.orders.remove(&id);
            }
        }
        self.hosts.insert(
            id,
            Host {
                role,
                beat,
                seen: now,
            },
        );
    }

    /// Notes that the host `id` was told at `now` to update to `target`. An order for the
    /// same target that has not aged out stays as it was, so a host that asks again before it
    /// reports is told again without its slot being held longer.
    pub fn order(&mut self, id: NodeId, target: Version, now: Duration) {
        if self.is_ordered(&id, now) && self.orders.get(&id).is_some_and(|o| o.target == target) {
            return;
        }
        self.orders.insert(id, Order { target, at: now });
    }

    /// Whether `id` has an order outstanding at `now`.
    #[must_use]
    pub fn is_ordered(&self, id: &NodeId, now: Duration) -> bool {
        self.orders
            .get(id)
            .is_some_and(|o| now.saturating_sub(o.at) < ORDER_AGE)
    }

    /// Whether the host `id` holds an update slot: [`Host::holds_slot`], or an order is
    /// outstanding for it.
    #[must_use]
    pub fn holds_slot(&self, id: &NodeId, host: &Host, now: Duration) -> bool {
        host.holds_slot() || self.is_ordered(id, now)
    }

    /// Whether the host `id` counts as up: [`Host::is_up`] and no order outstanding for it.
    #[must_use]
    pub fn is_up(&self, id: &NodeId, host: &Host, now: Duration) -> bool {
        host.is_up(now) && !self.is_ordered(id, now)
    }

    /// The host `id`, if it has heartbeated.
    #[must_use]
    pub fn get(&self, id: &NodeId) -> Option<&Host> {
        self.hosts.get(id)
    }

    /// Every host, by node id.
    pub fn hosts(&self) -> impl Iterator<Item = (&NodeId, &Host)> {
        self.hosts.iter()
    }
}
