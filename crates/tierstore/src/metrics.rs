//! What a [`Proxy`](crate::Proxy) counts of its reads, in the Prometheus text format.
//!
//! The counters go up and never down while the proxy runs. Their labels have a few fixed
//! values each (the tier, the kind of read, the reason a copy was not made), never a key, a
//! bucket or a client.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// The tier that answered, or was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    Hot,
    Cold,
}

/// The label of each [`Tier`], in the order of the enum.
const TIERS: [&str; 2] = ["hot", "cold"];

/// The kind of read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Read {
    Get,
    Head,
}

/// The label of each [`Read`], in the order of the enum.
const READS: [&str; 2] = ["get", "head"];

/// Why a get that found an object in the cold tier did not copy it into the hot tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Skipped {
    /// The hot tier is at or above its critical mark by the usage the reconciler published, or
    /// is taken to be full because it answered an earlier copy with "full" a moment ago.
    Critical,
    /// The hot tier answered the copy with "full".
    Full,
    /// The hot tier already held the object: another read copied it first.
    Raced,
    /// The copy failed for another reason.
    Failed,
}

/// The label of each [`Skipped`], in the order of the enum.
const SKIPPED: [&str; 4] = ["critical", "full", "raced", "failed"];

/// The counters of one proxy.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    /// Reads a tier found the object for, by tier and kind of read.
    hits: [[AtomicU64; READS.len()]; TIERS.len()],
    /// Reads a tier did not find the object for, by tier and kind of read.
    misses: [[AtomicU64; READS.len()]; TIERS.len()],
    /// Reads a tier failed with something other than "not found", by tier and kind of read.
    errors: [[AtomicU64; READS.len()]; TIERS.len()],
    /// Bytes of object data in the answers of gets, by the tier that gave them.
    served: [AtomicU64; TIERS.len()],
    /// Bytes of object data gets read from a tier: the whole object of a cold read, whatever
    /// range was asked.
    read: [AtomicU64; TIERS.len()],
    /// Objects copied from the cold tier into the hot tier by a get.
    promotions: AtomicU64,
    /// Bytes of those objects.
    promoted_bytes: AtomicU64,
    /// Copies that were not made, by reason.
    skipped: [AtomicU64; SKIPPED.len()],
}

fn bump(counter: &AtomicU64) {
    let _ = counter.fetch_add(1, Ordering::Relaxed);
}

fn add(counter: &AtomicU64, by: usize) {
    let _ = counter.fetch_add(by as u64, Ordering::Relaxed);
}

fn read(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

impl Counters {
    /// `tier` found the object.
    pub(crate) fn hit(&self, tier: Tier, kind: Read) {
        bump(&self.hits[tier as usize][kind as usize]);
    }

    /// `tier` did not have the object.
    pub(crate) fn miss(&self, tier: Tier, kind: Read) {
        bump(&self.misses[tier as usize][kind as usize]);
    }

    /// A get answered `bytes` bytes from `tier`.
    pub(crate) fn served(&self, tier: Tier, bytes: usize) {
        add(&self.served[tier as usize], bytes);
    }

    /// A read of `tier` failed with something other than "not found".
    pub(crate) fn error(&self, tier: Tier, kind: Read) {
        bump(&self.errors[tier as usize][kind as usize]);
    }

    /// A get read `bytes` bytes of object data from `tier`.
    pub(crate) fn bytes_read(&self, tier: Tier, bytes: usize) {
        add(&self.read[tier as usize], bytes);
    }

    /// An object of `bytes` bytes was copied into the hot tier.
    pub(crate) fn promoted(&self, bytes: usize) {
        bump(&self.promotions);
        add(&self.promoted_bytes, bytes);
    }

    /// A copy into the hot tier was not made, for `why`.
    pub(crate) fn skipped(&self, why: Skipped) {
        bump(&self.skipped[why as usize]);
    }

    /// The text of the counters, one family for both tiers, the tier a label (`tier` is `hot`
    /// or `cold` here; the local cache of a build node uses `local`). A read that reached a tier
    /// is a hit, a miss or an error there, so `reads` is the sum of the three.
    pub(crate) fn render(&self) -> String {
        let mut text = String::new();
        let tiers = TIERS.into_iter().enumerate();
        for (tier, label) in tiers.clone() {
            for (kind, op) in READS.into_iter().enumerate() {
                let labels = [("tier", label), ("op", op)];
                sample(
                    &mut text,
                    "hits_total",
                    &labels,
                    read(&self.hits[tier][kind]),
                );
                sample(
                    &mut text,
                    "misses_total",
                    &labels,
                    read(&self.misses[tier][kind]),
                );
                let failed = read(&self.errors[tier][kind]);
                sample(&mut text, "read_errors_total", &labels, failed);
                let reads = read(&self.hits[tier][kind]) + read(&self.misses[tier][kind]) + failed;
                sample(&mut text, "reads_total", &labels, reads);
            }
        }
        for (tier, label) in tiers {
            let labels = [("tier", label)];
            sample(
                &mut text,
                "bytes_served_total",
                &labels,
                read(&self.served[tier]),
            );
            sample(
                &mut text,
                "bytes_read_total",
                &labels,
                read(&self.read[tier]),
            );
        }
        sample(&mut text, "promotions_total", &[], read(&self.promotions));
        sample(
            &mut text,
            "promoted_bytes_total",
            &[],
            read(&self.promoted_bytes),
        );
        for (why, counter) in SKIPPED.into_iter().zip(&self.skipped) {
            let labels = [("reason", why)];
            sample(
                &mut text,
                "promotions_skipped_total",
                &labels,
                read(counter),
            );
        }
        text
    }
}

/// The line of `tierstore_<name>` with `labels`.
fn sample(text: &mut String, name: &str, labels: &[(&str, &str)], value: u64) {
    let pairs: Vec<String> = labels.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect();
    if pairs.is_empty() {
        let _ = writeln!(text, "tierstore_{name} {value}");
    } else {
        let _ = writeln!(text, "tierstore_{name}{{{}}} {value}", pairs.join(","));
    }
}
