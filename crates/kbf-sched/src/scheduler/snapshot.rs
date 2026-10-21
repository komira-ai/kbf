//! A copy of what the scheduler holds and queues, for the operator API's `GET
//! /v1/leases` and `GET /v1/queue`: plain values, taken in one call, so the caller
//! releases the scheduler's lock before it renders anything.
//!
//! Ages are as of the scheduler's newest input ([`Snapshot::now`]), which is at most a
//! tick old, so they never run ahead of what the scheduler has seen. The copy is
//! bounded: the leases are as many as the farm holds (never more than its capacity
//! allows), the queue is summarised per platform and QoS, and the lists of examples
//! and avoided workers are capped ([`MAX_AVOIDED`]).

use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::time::Duration;

use kbf_types::{Digest, FarmTime, LeaseId, OperationId, Qos, WorkerId};

use super::{OpState, Scheduler};

/// The most operations [`QueueView::avoided`] lists; [`QueueView::avoided_total`] is
/// the count before the cap.
pub const MAX_AVOIDED: usize = 100;

/// What a lease is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeasePhase {
    /// The grant is proposed to the control log and not committed: no `Start` was sent.
    Proposed,
    /// The grant is committed and its `Start` sent; the worker has not said it started.
    Granted,
    /// The worker reported it started.
    Running,
}

impl LeasePhase {
    /// The name the operator API shows.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Granted => "granted",
            Self::Running => "running",
        }
    }
}

/// One lease the scheduler holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseView {
    /// The lease id.
    pub lease: LeaseId,
    /// The operation it runs.
    pub operation: OperationId,
    /// The instance name and action digest of the operation.
    pub instance: String,
    /// The action digest.
    pub action: Digest,
    /// The worker that holds it.
    pub node: WorkerId,
    /// The platform the action asked for ([`platform_label`]).
    pub platform: String,
    /// The operation's QoS level.
    pub qos: Qos,
    /// What it is doing.
    pub phase: LeasePhase,
    /// When it was granted, and when it started if it has.
    pub granted_at: FarmTime,
    /// When the worker reported it started; `None` until it did.
    pub started_at: Option<FarmTime>,
    /// How long ago it was granted.
    pub age: Duration,
    /// How long ago its current phase began: the grant until it started, then the start.
    pub phase_age: Duration,
}

/// The queued operations at one platform and QoS level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueGroup {
    /// The platform they asked for ([`platform_label`]).
    pub platform: String,
    /// Their QoS level.
    pub qos: Qos,
    /// How many wait.
    pub depth: usize,
    /// How long the longest has waited in the queue.
    pub oldest_age: Duration,
}

/// Queued operations that no live worker can run, grouped by the reason they wait.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitingGroup {
    /// Why, as their callers are told.
    pub reason: String,
    /// How many wait for it.
    pub count: usize,
    /// The first of them in queue order, as one to look at.
    pub example: OperationId,
    /// Its instance name and action digest.
    pub example_instance: String,
    /// The action digest.
    pub example_action: Digest,
}

/// A queued operation placement keeps off some workers: its runs ended there as the
/// farm's fault, or the worker dropped its `Start`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Avoided {
    /// The operation.
    pub operation: OperationId,
    /// Its instance name.
    pub instance: String,
    /// Its action digest.
    pub action: Digest,
    /// The workers placement prefers it not run on, in name order.
    pub nodes: Vec<WorkerId>,
}

/// The queue, summarised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueView {
    /// How many operations wait for a lease ([`Scheduler::queued_len`]).
    pub depth: usize,
    /// How long the longest has waited in the queue; `None` when none waits.
    pub oldest_age: Option<Duration>,
    /// Per platform, then QoS most urgent first.
    pub groups: Vec<QueueGroup>,
    /// Per reason, in reason order.
    pub waiting: Vec<WaitingGroup>,
    /// Queued operations with workers to avoid, in operation order, at most
    /// [`MAX_AVOIDED`].
    pub avoided: Vec<Avoided>,
    /// How many there are, before the cap.
    pub avoided_total: usize,
}

/// The scheduler's leases and queue at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The leader term of this scheduler: every lease's `term`.
    pub term: u64,
    /// The scheduler's newest input time, which the ages are measured at.
    pub now: FarmTime,
    /// Every lease held, in lease order.
    pub leases: Vec<LeaseView>,
    /// The queue.
    pub queue: QueueView,
}

/// A platform request as one label: its properties as `key=value`, comma-separated, in
/// the request's fixed order; `any` for a request that asks for nothing.
#[must_use]
pub fn platform_label(needs: &kbf_caps::Request) -> String {
    let properties = needs.properties();
    if properties.is_empty() {
        return "any".to_owned();
    }
    let parts: Vec<String> = properties
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    parts.join(",")
}

impl Scheduler {
    /// The leases and the queue, copied.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            term: self.term,
            now: self.now,
            leases: self.lease_snapshot(None),
            queue: self.queue_snapshot(),
        }
    }

    /// The leases held, in lease order: all of them, or only `node`'s.
    #[must_use]
    pub fn lease_snapshot(&self, node: Option<&WorkerId>) -> Vec<LeaseView> {
        self.held
            .iter()
            .filter_map(|(lease, held)| {
                let op = self.ops.get(&held.operation)?;
                let (_, holder) = op.state.holding()?;
                if node.is_some_and(|wanted| wanted != holder) {
                    return None;
                }
                let phase = match &op.state {
                    OpState::Running { .. } => LeasePhase::Running,
                    OpState::Leased {
                        committed: false, ..
                    } => LeasePhase::Proposed,
                    _ => LeasePhase::Granted,
                };
                let began = held.started_at.unwrap_or(held.granted_at);
                Some(LeaseView {
                    lease: *lease,
                    operation: held.operation,
                    instance: op.request.key.instance.clone(),
                    action: op.request.key.action,
                    node: holder.clone(),
                    platform: platform_label(&op.request.needs),
                    qos: op.request.qos.clone(),
                    phase,
                    granted_at: held.granted_at,
                    started_at: held.started_at,
                    age: self.now.saturating_duration_since(held.granted_at),
                    phase_age: self.now.saturating_duration_since(began),
                })
            })
            .collect()
    }

    /// The queue, summarised.
    #[must_use]
    pub fn queue_snapshot(&self) -> QueueView {
        let mut groups: BTreeMap<(String, Reverse<Qos>), (usize, FarmTime)> = BTreeMap::new();
        let mut waiting: BTreeMap<&str, WaitingGroup> = BTreeMap::new();
        let mut avoided = Vec::new();
        let mut oldest: Option<FarmTime> = None;
        for id in self.queued() {
            let Some(op) = self.ops.get(&id) else {
                continue;
            };
            oldest = Some(oldest.map_or(op.queued_at, |o| o.min(op.queued_at)));
            let key = (
                platform_label(&op.request.needs),
                Reverse(op.request.qos.clone()),
            );
            let group = groups.entry(key).or_insert((0, op.queued_at));
            group.0 += 1;
            group.1 = group.1.min(op.queued_at);
            if let Some(u) = &op.unservable {
                waiting
                    .entry(u.reason.as_str())
                    .and_modify(|w| w.count += 1)
                    .or_insert_with(|| WaitingGroup {
                        reason: u.reason.clone(),
                        count: 1,
                        example: id,
                        example_instance: op.request.key.instance.clone(),
                        example_action: op.request.key.action,
                    });
            }
            if !op.avoid.is_empty() {
                avoided.push(Avoided {
                    operation: id,
                    instance: op.request.key.instance.clone(),
                    action: op.request.key.action,
                    nodes: op.avoid.iter().cloned().collect(),
                });
            }
        }
        avoided.sort_by_key(|a| a.operation);
        let avoided_total = avoided.len();
        avoided.truncate(MAX_AVOIDED);
        let age = |since: FarmTime| self.now.saturating_duration_since(since);
        QueueView {
            depth: self.queue.len(),
            oldest_age: oldest.map(age),
            groups: groups
                .into_iter()
                .map(|((platform, Reverse(qos)), (depth, since))| QueueGroup {
                    platform,
                    qos,
                    depth,
                    oldest_age: age(since),
                })
                .collect(),
            waiting: waiting.into_values().collect(),
            avoided,
            avoided_total,
        }
    }
}
