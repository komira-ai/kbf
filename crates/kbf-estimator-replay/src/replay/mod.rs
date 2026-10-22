//! A memory-only replay of an exported history through the real scheduler.
//!
//! The replay submits each operation of a [`trace::Trace`] to a real
//! [`kbf_sched::Scheduler`] at the time it arrived, with the booking the arm under test
//! chooses; it gives the scheduler nodes of the recorded capacity, ends each attempt after
//! the recorded time, and kills one whose booking's limit is below the operation's need.
//! The scheduler, not the replay, climbs the ladder, spreads leases and queues. Nothing is
//! written anywhere and no clock is read: farm time is the trace's.
//!
//! It is an upper bound on what a change could gain and a rough check that it does not
//! kill more, not a measurement. [`report::LIMITS`] lists what it cannot see.

pub mod report;
pub mod sim;
pub mod stats;
pub mod trace;

use kbf_estimator::GIB;

/// Every number the replay takes from its caller. They are fixed before any scored day
/// is looked at: the default is what a report is made with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Params {
    /// The memory ladder in GiB, as the farm runs it; the first rung is what an action
    /// books by default.
    pub ladder_gib: Vec<u64>,
    /// Days that only teach the estimator (days 1 to this).
    pub train_days: u32,
    /// Days that are scored (the days after the training days).
    pub score_days: u32,
    /// How long after a kill ended an attempt of the same digest still counts as its rerun.
    pub chain_gap_us: u64,
    /// How often a node reports its leases while work is in flight, in milliseconds.
    pub heartbeat_ms: u64,
    /// Events a replay handles before it gives up (a loop that does not end).
    pub max_events: u64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            ladder_gib: vec![4, 16, 32],
            train_days: 20,
            score_days: 10,
            chain_gap_us: 60_000_000,
            heartbeat_ms: 30_000,
            max_events: 200_000_000,
        }
    }
}

impl Params {
    /// What an action books by default, in bytes.
    pub fn first_booking(&self) -> u64 {
        self.ladder_gib.first().copied().unwrap_or(4) * GIB
    }
}

/// What books an operation's memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Arm {
    /// The estimator is off: every operation books what the record's first attempt did.
    Null,
    /// Phase 1: a kill-driven floor keyed `(instance, platform, basis, shape)`, applied
    /// when the operation is submitted, strictly below the host, to an action that did not
    /// name its memory.
    KeyFloor,
    /// Not phase 1, an upper bound: the estimator's own estimate, which can book below
    /// the default once a key has samples, with the floor on top.
    SampleUpperBound,
}

impl Arm {
    /// The arm's name in a report.
    pub fn name(self) -> &'static str {
        match self {
            Self::Null => "null (estimator off)",
            Self::KeyFloor => "key floor (phase 1)",
            Self::SampleUpperBound => "sample lowering (NOT phase 1, upper bound)",
        }
    }
}
