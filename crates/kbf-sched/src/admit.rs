//! Admission by measured CPU: the clause of [`crate::scheduler::Worker::has_room`] that
//! keeps a node from being handed more work while its CPUs are already used up.
//!
//! Placement books one core and little memory per action, so a node with 80 cores takes
//! 80 leases whatever they do, and actions that each start many threads then compete for
//! the cores far beyond what was booked. The daemon measures the CPU time its actions
//! used over each heartbeat window (`Heartbeat.busy_millis`, [`Event::Busy`]); with
//! [`Scheduler::with_cpu_admission`] the scheduler admits a new action to a node only
//! while that reading is below a share of the node's CPUs, and at most
//! [`CpuAdmission::ramp`] actions per reading, since a reading says nothing about the
//! leases admitted after it.
//!
//! The clause only ever delays a placement, and never refuses one, so it is not part of
//! the test of whether a worker could run a request at all (`servable.rs`): nothing is
//! refused for it. It lets an action through when it cannot judge, or when waiting must
//! end:
//!
//! - no reading, or one older than [`STALE_AFTER`] heartbeat intervals (a daemon that
//!   predates the field, a node without the cgroup, a silent node: unknown is not idle,
//!   but it is not busy either);
//! - the node holds no lease (it cannot be oversubscribed by its own work). This is what
//!   exempts an action at the whole host's memory too, which is placed only on a node
//!   with no lease ([`crate::scheduler::Worker::exclusive`]); VM and whole-machine leases
//!   are not actions and the clause is not part of their placement;
//! - the action has been queued for [`CpuAdmission::max_wait`], so nothing starves.
//!
//! [`Event::Busy`]: crate::Event::Busy
//! [`Scheduler::with_cpu_admission`]: crate::Scheduler::with_cpu_admission

use std::time::Duration;

use kbf_types::FarmTime;

/// How many heartbeat intervals a reading stays current: one older than this is stale
/// and the clause lets the action through.
pub const STALE_AFTER: u32 = 3;

/// The default of [`CpuAdmission::ramp`].
pub const DEFAULT_RAMP: u32 = 8;

/// The default of [`CpuAdmission::max_wait`]: five minutes, so an action waits no longer
/// than the unservable wait for a node that is merely busy. Longer than the time a
/// healthy batch of CPU-hungry actions takes to drain, so that aging does not release a
/// whole queue at once while the node is working through it.
pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(300);

/// What the CPU admission clause is told: see the module documentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuAdmission {
    /// Admit an action to a node only while the node's busy reading is below this
    /// percentage of its CPUs. Between 1 and 100.
    pub busy_percent: u32,
    /// At most this many new leases are admitted to a node per reading. At least 1.
    pub ramp: u32,
    /// An action queued this long is admitted whatever the node's reading (farm time).
    pub max_wait: Duration,
    /// How often daemons heartbeat, which paces the readings: one older than
    /// [`STALE_AFTER`] of these is stale.
    pub interval: Duration,
}

impl CpuAdmission {
    /// The clause for nodes that heartbeat every `interval`, below `busy_percent` of
    /// their CPUs, with [`DEFAULT_RAMP`] and [`DEFAULT_MAX_WAIT`].
    #[must_use]
    pub const fn new(busy_percent: u32, interval: Duration) -> Self {
        Self {
            busy_percent,
            ramp: DEFAULT_RAMP,
            max_wait: DEFAULT_MAX_WAIT,
            interval,
        }
    }

    /// This clause with at most `ramp` leases per reading (at least 1).
    #[must_use]
    pub const fn with_ramp(mut self, ramp: u32) -> Self {
        self.ramp = if ramp == 0 { 1 } else { ramp };
        self
    }

    /// This clause admitting an action queued for `max_wait` whatever the reading.
    #[must_use]
    pub const fn with_max_wait(mut self, max_wait: Duration) -> Self {
        self.max_wait = max_wait;
        self
    }

    /// The busy reading, in CPU-milliseconds per second, below which a node with
    /// `cpu_millis` of CPU takes a new action.
    fn threshold(&self, cpu_millis: u64) -> u64 {
        let share = u128::from(cpu_millis) * u128::from(self.busy_percent) / 100;
        u64::try_from(share).unwrap_or(u64::MAX)
    }
}

/// A node's newest busy reading and what has been admitted since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Busy {
    /// CPU-milliseconds per second over the window that ended at `at`.
    millis: u64,
    /// When the scheduler took it.
    at: FarmTime,
    /// The leases placed on the node since.
    admitted: u32,
}

impl Busy {
    /// The reading `millis`, taken at `at`, with nothing admitted since.
    pub(crate) const fn new(millis: u64, at: FarmTime) -> Self {
        Self {
            millis,
            at,
            admitted: 0,
        }
    }

    /// Notes one more lease placed on the node.
    pub(crate) fn admitted_one(&mut self) {
        self.admitted = self.admitted.saturating_add(1);
    }
}

/// What a placement round tells [`crate::scheduler::Worker::has_room`] about the request:
/// the clause (`None` when the feature is off), the farm time, and how long the request
/// has been queued.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Gate<'a> {
    pub(crate) policy: Option<&'a CpuAdmission>,
    pub(crate) now: FarmTime,
    pub(crate) waited: Duration,
}

impl Gate<'_> {
    /// Whether a node of `cpu_millis` CPUs holding `leases` leases, whose newest reading
    /// is `busy`, may take another action.
    pub(crate) fn admits(&self, busy: Option<&Busy>, cpu_millis: u64, leases: usize) -> bool {
        let Some(policy) = self.policy else {
            return true;
        };
        if leases == 0 || self.waited >= policy.max_wait {
            return true;
        }
        let Some(busy) = busy else {
            return true;
        };
        let age = self.now.saturating_duration_since(busy.at);
        if age > policy.interval.saturating_mul(STALE_AFTER) {
            return true;
        }
        busy.millis < policy.threshold(cpu_millis) && busy.admitted < policy.ramp
    }
}
