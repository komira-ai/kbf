//! The export format: one JSON object per line, one finished attempt (a run record) each.
//!
//! The reader tolerates what an older server does not know: every field beyond the ones
//! the first export carried is optional, and absent is not zero. A line of a file read
//! back (`Row::parse`) that holds an unknown key is accepted, so a newer export can be read
//! by an older replay.

use serde::{Deserialize, Serialize};

/// What the run's cgroup measured. A field the server did not report is `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// CPU time, user plus system, in microseconds.
    pub cpu_usage_us: u64,
    /// Wall time in microseconds. `None` when the read API gave 0 (it writes 0 for a wall
    /// that was not measured, and a run of zero microseconds is not a sample either way).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_us: Option<u64>,
    /// `memory.peak`, in bytes; `None` when the read API gave 0 (same reason).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_peak_bytes: Option<u64>,
    /// `memory.events` `max`: times the run reached its limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_events_max: Option<u64>,
    /// `memory.events` `oom_kill`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_events_oom_kill: Option<u64>,
    /// `memory.swap.peak`, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_swap_peak_bytes: Option<u64>,
    /// The largest anon + swap sampled while the run lived, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_anon_swap_peak_bytes: Option<u64>,
    /// `cpu.pressure` `some` total, in microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pressure_some_us: Option<u64>,
}

/// What a node offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capacity {
    /// CPU in thousandths of a core.
    pub cpu_millis: u64,
    /// Memory in bytes.
    pub memory_bytes: u64,
}

/// One finished attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// The run id (24 hex digits).
    pub run_id: String,
    /// The REAPI instance.
    pub instance: String,
    /// The OS class of the node it ran on (`linux`, `macos`), `-` when the node is not
    /// known. CPU architecture is not in the read API.
    pub platform: String,
    /// The node that ran it.
    pub node: String,
    /// The shape key, 16 hex digits; `-` for a run with no shape.
    pub shape: String,
    /// The configuration hash; `-` for none.
    pub cfg: String,
    /// The action digest, `<hash>-<size>`; `-` when absent.
    pub digest: String,
    /// The run's class (`ok`, `failed`, `timed_out`, `memory_killed`, `infra`, `lost`,
    /// `cancelled`).
    pub outcome: String,
    /// `none`, `own_limit` or `node_pressure`.
    pub memory_kill: String,
    /// The booking, when the record keeps it (a killed run's does; others may not).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub booked_memory_bytes: Option<u64>,
    /// For an own-limit kill, the booking the ladder retried with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raised_memory_bytes: Option<u64>,
    /// For a killed run, the memory in use when it died (0 when it carried no usage).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_used_bytes: Option<u64>,
    /// The cgroup block, if the run reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup: Option<Usage>,
    /// When the attempt was queued, in microseconds after the epoch.
    pub queued_us: u64,
    /// When it started; `None` for a run that never did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_us: Option<u64>,
    /// When it ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_us: Option<u64>,
    /// The node's capacity, as the operator API reports it now (a point in time, not the
    /// node's capacity when the run ran).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_capacity: Option<Capacity>,
    /// `cgroup` or `mac_footprint`, when the server says which instrument measured; absent
    /// from a server that predates the tag (the replay then decides what to assume).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<String>,
    /// The operation the attempt belonged to, when the server says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// The CPU booked, in thousandths of a core, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub booked_millicpus: Option<u64>,
    /// The node's CPU pressure when the lease started, in hundredths of a percent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_cpu_pressure_centi: Option<u64>,
    /// The invocation the attempt's request belonged to (the client's
    /// `tool_invocation_id`), when the server records it; absent from a server that
    /// predates the field, and an older export line, which has no such key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invocation_id: Option<String>,
    /// The group of invocations this one is part of (`correlated_invocations_id`, a CI job),
    /// when the client sent one and the server records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlated_invocations_id: Option<String>,
}

impl Row {
    /// The UTC day the attempt was queued on, as `YYYY-MM-DD`.
    pub fn day(&self) -> String {
        day_of(self.queued_us)
    }

    /// One line of an export.
    ///
    /// # Errors
    /// The text is not a row.
    pub fn parse(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }

    /// The row as one line, without the newline.
    pub fn line(&self) -> String {
        // A row holds only strings, integers and options of them: it always serializes.
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// The UTC day of a time in microseconds after the epoch, as `YYYY-MM-DD`.
pub fn day_of(us: u64) -> String {
    // At most u64::MAX / 10^6, which fits an i64.
    let secs = (us / 1_000_000) as i64;
    time::OffsetDateTime::from_unix_timestamp(secs)
        .map_or_else(|_| "9999-12-31".to_owned(), |t| t.date().to_string())
}
