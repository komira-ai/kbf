//! The crate's own input types: what the server tells the estimator about a finished run.

use crate::GIB;

/// How a run ended, as far as sizing cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Exit 0.
    Success,
    /// Killed at its own cgroup memory limit.
    OwnLimitKill,
    /// Killed because the node was short of memory (the peak reflects a busy node).
    NodePressureKill,
    /// Any other failure (an early compile error peaks low).
    Failed,
    /// The worker or lease was lost.
    Lost,
    /// Cancelled by the client.
    Cancelled,
}

/// One finished attempt of an action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// How it ended.
    pub outcome: Outcome,
    /// Wall time in microseconds.
    pub wall_us: u64,
    /// The whole-tree peak memory of the run on the key's basis, if it was measured
    /// (for a kill: the memory in use when it died).
    pub peak_memory_bytes: Option<u64>,
    /// The memory it was booked at.
    pub booked_memory_bytes: u64,
    /// The memory limit it ran under.
    pub limit_bytes: u64,
    /// For an own-limit kill: the booking the caller's ladder retries with.
    pub raised_memory_bytes: Option<u64>,
    /// CPU time used (user plus system) in microseconds.
    pub cpu_usage_us: u64,
    /// The node's CPU pressure (`avg10`, in tenths of a percent) when the run started,
    /// if known.
    pub node_cpu_pressure_permille: Option<u16>,
}

/// What the caller can book at most and books by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    /// What an action with no estimate books (today: 4 GiB).
    pub default_memory: u64,
    /// The most memory any node offers; estimates and floors never exceed it.
    pub memory_cap: u64,
    /// The most CPU any node offers, in millicores.
    pub node_cpu_millis: u32,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            default_memory: 4 * GIB,
            memory_cap: 32 * GIB,
            node_cpu_millis: 64_000,
        }
    }
}

/// What the client asked for. The estimate never gives less.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hint {
    /// Memory the client asked for, in bytes.
    pub memory_bytes: Option<u64>,
    /// CPU the client asked for, in millicores.
    pub cpu_millis: Option<u32>,
}

/// Why a run did not become a memory sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discard {
    /// A failed (not memory-killed), lost or cancelled run, or a node-pressure kill.
    NotASample,
    /// No measurement, or a wall time under one second.
    Untrusted,
    /// A success whose peak is above the node cap.
    OverCap,
}

/// What [`crate::Summaries::observe`] did with a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observed {
    /// Whether the run entered the memory maxima.
    pub memory: Result<(), Discard>,
    /// Whether the run entered the CPU samples.
    pub cpu: bool,
}
