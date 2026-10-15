//! Reading a lease's [`ResourceUsage`] out of an `ActionResult`, and the `key=value`
//! fields both the daemon's `lease phases:` line and the server's `action usage:`
//! line print for its [`CgroupUsage`]. One formatter, so the two lines agree.
//!
//! The derived numbers are integers in hundredths (`parallelism_centi`), so they stay
//! comparable and exact; they are printed with two decimals.

use std::fmt::Write as _;

use prost::Message;
use prost_types::Any;

use crate::reapi::ActionResult;
use crate::worker::{CgroupUsage, ResourceUsage};

impl ResourceUsage {
    /// The type URL of a `ResourceUsage` in `ExecutedActionMetadata.auxiliary_metadata`.
    pub const TYPE_URL: &'static str = "type.googleapis.com/kbf.worker.v1.ResourceUsage";

    /// This usage as the `auxiliary_metadata` entry that carries it in a result.
    #[must_use]
    pub fn to_any(&self) -> Any {
        Any {
            type_url: Self::TYPE_URL.to_owned(),
            value: self.encode_to_vec(),
        }
    }

    /// The usage `result` carries, if it carries one that decodes.
    #[must_use]
    pub fn in_result(result: &ActionResult) -> Option<Self> {
        let any = result
            .execution_metadata
            .as_ref()?
            .auxiliary_metadata
            .iter()
            .find(|any| any.type_url == Self::TYPE_URL)?;
        Self::decode(any.value.as_slice()).ok()
    }
}

impl CgroupUsage {
    /// CPU time over wall time: how many CPUs the action kept busy on average, in
    /// hundredths (150 is 1.50). `None` without the cgroup's CPU time or without a
    /// wall time to divide by.
    #[must_use]
    pub fn parallelism_centi(&self, wall_micros: u64) -> Option<u64> {
        let usage = u128::from(self.cpu_usage_micros?);
        let wall = u128::from(wall_micros);
        if wall == 0 {
            return None;
        }
        u64::try_from((usage * 100 + wall / 2) / wall).ok()
    }

    /// The share of the action's memory that is not on the node holding most of it, in
    /// hundredths (0 is all on one node). `None` when no node's memory is known or it
    /// is all zero.
    #[must_use]
    pub fn numa_spread_centi(&self) -> Option<u64> {
        let total: u128 = self.numa.iter().map(|n| u128::from(n.bytes)).sum();
        let most = u128::from(self.numa.iter().map(|n| n.bytes).max()?);
        if total == 0 {
            return None;
        }
        u64::try_from(((total - most) * 100 + total / 2) / total).ok()
    }

    /// The fields of a log line, in a fixed order, `-` for one not known:
    /// `cpu_usage_us cpu_user_us cpu_system_us cpu_periods cpu_throttled throttled_us
    /// mem_peak parallelism numa_bytes numa_spread`. `wall_micros` is the action's run
    /// time, for `parallelism`; `numa_bytes` is `node:bytes` pairs joined by commas.
    #[must_use]
    pub fn log_fields(&self, wall_micros: u64) -> String {
        let n = |v: Option<u64>| v.map_or_else(|| "-".to_owned(), |v| v.to_string());
        let hundredths = |v: Option<u64>| {
            v.map_or_else(|| "-".to_owned(), |c| format!("{}.{:02}", c / 100, c % 100))
        };
        let mut numa = String::new();
        for (i, node) in self.numa.iter().enumerate() {
            let comma = if i == 0 { "" } else { "," };
            let _ = write!(numa, "{comma}{}:{}", node.node, node.bytes);
        }
        if numa.is_empty() {
            numa.push('-');
        }
        format!(
            "cpu_usage_us={} cpu_user_us={} cpu_system_us={} cpu_periods={} cpu_throttled={} \
             throttled_us={} mem_peak={} parallelism={} numa_bytes={numa} numa_spread={}",
            n(self.cpu_usage_micros),
            n(self.cpu_user_micros),
            n(self.cpu_system_micros),
            n(self.cpu_periods),
            n(self.cpu_throttled_periods),
            n(self.cpu_throttled_micros),
            n(self.memory_peak_bytes),
            hundredths(self.parallelism_centi(wall_micros)),
            hundredths(self.numa_spread_centi()),
        )
    }
}

/// The fields for a lease that has no cgroup counters: every one `-`.
#[must_use]
pub fn no_cgroup_fields() -> String {
    CgroupUsage::default().log_fields(0)
}
