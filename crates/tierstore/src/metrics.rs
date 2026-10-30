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
    /// The hot tier is at or above its critical mark, or was just told it is full.
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
    /// Bytes of object data in the answers of gets, by the tier that gave them.
    served: [AtomicU64; TIERS.len()],
    /// Reads that went on to the cold tier because the hot tier did not have the object.
    cold_reads: [AtomicU64; READS.len()],
    /// Bytes gets read from the cold tier, whole objects, whatever range was asked.
    cold_bytes_read: AtomicU64,
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

    /// A read went on to the cold tier.
    pub(crate) fn cold_read(&self, kind: Read) {
        bump(&self.cold_reads[kind as usize]);
    }

    /// A get read `bytes` bytes from the cold tier.
    pub(crate) fn cold_bytes_read(&self, bytes: usize) {
        add(&self.cold_bytes_read, bytes);
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

    /// The text of the counters. The names follow those of the local cache: the tier is in the
    /// name (`tierstore_hot_hits_total`), as `cache` is in `tierstore_cache_hits_total`, and a
    /// label has the few values of a kind of read or a reason.
    pub(crate) fn render(&self) -> String {
        let mut text = String::new();
        for (tier, (hits, misses)) in TIERS.into_iter().zip(self.hits.iter().zip(&self.misses)) {
            for (kind, (hit, miss)) in READS.into_iter().zip(hits.iter().zip(misses)) {
                sample(&mut text, &format!("{tier}_hits_total"), ("op", kind), hit);
                sample(
                    &mut text,
                    &format!("{tier}_misses_total"),
                    ("op", kind),
                    miss,
                );
            }
        }
        for (tier, counter) in TIERS.into_iter().zip(&self.served) {
            plain(&mut text, &format!("{tier}_bytes_served_total"), counter);
        }
        for (kind, counter) in READS.into_iter().zip(&self.cold_reads) {
            sample(&mut text, "cold_reads_total", ("op", kind), counter);
        }
        plain(&mut text, "cold_bytes_read_total", &self.cold_bytes_read);
        plain(&mut text, "promotions_total", &self.promotions);
        plain(&mut text, "promoted_bytes_total", &self.promoted_bytes);
        for (why, counter) in SKIPPED.into_iter().zip(&self.skipped) {
            sample(
                &mut text,
                "promotions_skipped_total",
                ("reason", why),
                counter,
            );
        }
        text
    }
}

/// The line of `tierstore_<name>`.
fn plain(text: &mut String, name: &str, counter: &AtomicU64) {
    let _ = writeln!(text, "tierstore_{name} {}", read(counter));
}

/// The line of `tierstore_<name>` with the label `(key, value)`.
fn sample(text: &mut String, name: &str, label: (&str, &str), counter: &AtomicU64) {
    let (key, value) = label;
    let _ = writeln!(
        text,
        "tierstore_{name}{{{key}=\"{value}\"}} {}",
        read(counter)
    );
}
