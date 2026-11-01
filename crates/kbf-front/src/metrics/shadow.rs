//! The learned-sizes shadow's counters: how each finished attempt fell against the
//! estimate for its action, and why the shadow skipped what it skipped. Rendered with the
//! cache's families ([`super::CacheMetrics::render`]).

use std::fmt::Write as _;

use super::{Families, Level};

/// The level of the size estimate a finished attempt was compared with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadowLevel {
    /// The summary of the action with its configuration.
    L1,
    /// The summary of the action without it.
    L2,
    /// No estimate: too few samples, or none.
    None,
}

/// How a finished attempt fell against the estimate for its action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadowBucket {
    /// It used at most 95% of the limit the estimate would have given.
    Covered,
    /// It used more than that limit: the estimate would have had it killed.
    WouldOom,
    /// Its peak is a lower bound only (it ran against its own limit, or was killed at
    /// it) or came within 5% of the estimate's limit: no claim either way.
    Censored,
    /// There was no estimate.
    Cold,
}

const SHADOW_LEVELS: [(ShadowLevel, &str); 3] = [
    (ShadowLevel::L1, "l1"),
    (ShadowLevel::L2, "l2"),
    (ShadowLevel::None, "none"),
];
const SHADOW_BUCKETS: [(ShadowBucket, &str); 4] = [
    (ShadowBucket::Covered, "covered"),
    (ShadowBucket::WouldOom, "would_oom"),
    (ShadowBucket::Censored, "censored"),
    (ShadowBucket::Cold, "cold"),
];

/// Why the shadow did not show an attempt or an operation to the estimator, as the label
/// of `kbf_estimator_shadow_skipped_total` spells it.
pub const SHADOW_SKIPS: [&str; 5] = ["kind", "no_os", "no_booking", "no_basis", "no_subject"];

/// One count for each level and bucket the shadow reports, and for each reason it skipped
/// something, set by the owner of the tally each time it publishes.
#[derive(Debug, Default)]
pub struct ShadowTotals {
    buckets: [[Level; 4]; 3],
    skipped: [Level; 5],
}

impl ShadowTotals {
    /// Sets the count of the skips of reason number `reason` ([`SHADOW_SKIPS`]).
    pub fn set_skipped(&self, reason: usize, total: u64) {
        if let Some(level) = self.skipped.get(reason) {
            level.set(total);
        }
    }

    /// The count of the skips of reason number `reason`.
    #[must_use]
    pub fn skipped(&self, reason: usize) -> u64 {
        self.skipped.get(reason).map_or(0, Level::get)
    }

    /// Sets the count of `level` and `bucket`.
    pub fn set(&self, level: ShadowLevel, bucket: ShadowBucket, total: u64) {
        self.buckets[level as usize][bucket as usize].set(total);
    }

    /// The count of `level` and `bucket`.
    #[must_use]
    pub fn get(&self, level: ShadowLevel, bucket: ShadowBucket) -> u64 {
        self.buckets[level as usize][bucket as usize].get()
    }

    pub(super) fn render(&self, out: &mut Families) {
        let name = "kbf_estimator_shadow_total";
        out.head(
            name,
            "counter",
            "Finished attempts compared with the learned-sizes shadow's estimate for their \
             action, by the estimate's level and how the attempt fell",
        );
        for (level, level_name) in SHADOW_LEVELS {
            for (bucket, bucket_name) in SHADOW_BUCKETS {
                let _ = writeln!(
                    out.0,
                    "{name}{{level=\"{level_name}\",bucket=\"{bucket_name}\"}} {}",
                    self.get(level, bucket)
                );
            }
        }
        let name = "kbf_estimator_shadow_skipped_total";
        out.head(
            name,
            "counter",
            "Attempts and operations the learned-sizes shadow did not show the estimator, by why",
        );
        for (reason, label) in SHADOW_SKIPS.iter().enumerate() {
            let _ = writeln!(
                out.0,
                "{name}{{reason=\"{label}\"}} {}",
                self.skipped(reason)
            );
        }
    }
}
