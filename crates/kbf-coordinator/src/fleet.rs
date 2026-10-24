//! What the coordinator knows of the hosts: the last heartbeat of each, and when it came.
//!
//! Nothing here is stored. After a restart the coordinator knows no host until it
//! heartbeats, which is why the minimums are counted against the heartbeats it has seen and
//! never against a host list (`docs/design/updates.md`, section 3). Time is a `Duration`
//! since the coordinator started, so the decision replays exactly in a test.

use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::{HostRole, NodeId};
use kbf_updater::wire::{Health, Heartbeat, Outcome};

/// A host with no heartbeat for this long is down.
pub const DOWN_AFTER: Duration = Duration::from_secs(45);

/// After it starts, the coordinator orders no update for this long, to hear the hosts first.
pub const QUIET_START: Duration = Duration::from_secs(30);

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
}

impl Fleet {
    /// A fleet that knows no host, for a coordinator that started at `started`.
    #[must_use]
    pub fn new(started: Duration) -> Self {
        Self {
            started,
            hosts: BTreeMap::new(),
        }
    }

    /// When the coordinator started.
    #[must_use]
    pub const fn started(&self) -> Duration {
        self.started
    }

    /// Notes a heartbeat: the host's latest word replaces its earlier one.
    pub fn record(&mut self, id: NodeId, role: HostRole, beat: Heartbeat, now: Duration) {
        self.hosts.insert(
            id,
            Host {
                role,
                beat,
                seen: now,
            },
        );
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
