//! Planning where a run's output blobs go, before anything is stored.
//!
//! A v2 run is given a fixed set of PUT URLs (see [`crate::put_urls`]): `N` packs and `M` large
//! objects. [`plan`] decides, from the blobs' digests and sizes alone, which blob goes to which
//! slot, or that the outputs do not fit:
//!
//! - a blob over the record cap goes alone to a large slot (more than `M` of them, or one over
//!   the large cap, is [`PlanError::TooLarge`]);
//! - an empty blob goes nowhere: it is implicit in every store and is never a record;
//! - every other blob goes to a pack, once however many outputs name it, in the order of its
//!   digest, so the same outputs give the same plan whatever order they were collected in;
//! - the packs are filled one after another, packs 1, 2, ... to the byte and record limits
//!   (footer included), and pack 0 last. Pack 0 holds the manifest, so it keeps room for the
//!   manifest record and its index entry. As soon as everything that is left fits pack 0, it
//!   all goes there: a run whose blobs fit one pack uses pack 0 alone, as a v1 run does;
//! - more packs than `N` is [`PlanError::TooLarge`].
//!
//! The footer is counted exactly: a pack of `r` record bytes and `e` records is
//! `r + footer_len(e)` bytes ([`kbf_segments::layout::footer_len`]: 256 fan-out words of 4 bytes,
//! 52 bytes per index entry and a 32-byte trailer), the same count the segment writer applies,
//! so a planned pack is never refused by it.
//!
//! This is a pure function of its arguments; the run calls it once the outputs are collected
//! ([`crate::task`]).

use std::collections::BTreeSet;

use kbf_segments::MAX_SEGMENT_BYTES;
use kbf_segments::layout::footer_len;
use kbf_task_pack::{MAX_PACK_BYTES, MAX_PACK_RECORDS, MAX_RECORD_BYTES};
use kbf_types::Digest;

use crate::put::MAX_LARGE_BYTES;

/// How many pack slots a run is issued (`N`), with the design's value.
pub const PACK_SLOTS: usize = 8;

/// How many large-object slots a run is issued (`M`), with the design's value.
pub const LARGE_SLOTS: usize = 8;

// A pack is a segment; the two caps must be the same number.
const _: () = assert!(MAX_PACK_BYTES == MAX_SEGMENT_BYTES);

/// The limits a plan keeps to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most bytes of a pack, footer included.
    pub pack_bytes: u64,
    /// The most records of a pack, the manifest among them in pack 0.
    pub pack_records: usize,
    /// The longest record; a larger blob goes to a large slot.
    pub record_bytes: u64,
    /// The longest large object.
    pub large_bytes: u64,
    /// `N`, the pack slots.
    pub packs: usize,
    /// `M`, the large slots.
    pub large: usize,
}

impl Limits {
    /// The limits of a real run: the pack and record caps of `kbf-task-pack`, the large cap
    /// [`MAX_LARGE_BYTES`] and the design's slot counts.
    #[must_use]
    pub const fn real() -> Self {
        Self {
            pack_bytes: MAX_PACK_BYTES,
            pack_records: MAX_PACK_RECORDS,
            record_bytes: MAX_RECORD_BYTES,
            large_bytes: MAX_LARGE_BYTES,
            packs: PACK_SLOTS,
            large: LARGE_SLOTS,
        }
    }
}

/// Where each blob goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The digests of each pack's blobs, in digest order, indexed by slot. Slot 0 always exists
    /// (it holds the manifest, and may hold no blob); the packs used are slots `0..packs.len()`,
    /// and every slot of that range is used.
    pub packs: Vec<Vec<Digest>>,
    /// The digests of the large objects, in digest order; entry `i` goes to large slot `i`.
    pub large: Vec<Digest>,
}

/// Why the outputs cannot be placed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    /// The outputs do not fit the slots the run was issued.
    #[error("output-too-large: {0}")]
    TooLarge(String),
    /// A blob's size is not its digest's size.
    #[error("invalid-manifest: a blob of size {given} has a digest of size {digest}")]
    SizeMismatch {
        /// The size the caller gave.
        given: u64,
        /// The size the digest names.
        digest: u64,
    },
}

/// The pack being filled: its blobs, their bytes, and what it must keep room for.
struct Fill {
    digests: Vec<Digest>,
    bytes: u64,
    /// Bytes and records already spoken for (the manifest, in pack 0).
    reserve_bytes: u64,
    reserve_records: usize,
}

impl Fill {
    fn new(reserve_bytes: u64, reserve_records: usize) -> Self {
        Self {
            digests: Vec::new(),
            bytes: 0,
            reserve_bytes,
            reserve_records,
        }
    }

    /// Whether one more blob of `size` bytes keeps the pack within `limits`.
    fn fits(&self, size: u64, limits: &Limits) -> bool {
        let records = self.digests.len() + self.reserve_records + 1;
        let bytes = self
            .bytes
            .saturating_add(self.reserve_bytes)
            .saturating_add(size)
            .saturating_add(footer_len(records as u64));
        records <= limits.pack_records && bytes <= limits.pack_bytes
    }

    /// Adds the blobs of `rest`, in order, up to the first that does not fit; returns how many.
    fn take_from(&mut self, rest: &[Digest], limits: &Limits) -> usize {
        let mut taken = 0;
        for digest in rest {
            if !self.fits(digest.size_bytes, limits) {
                break;
            }
            self.bytes += digest.size_bytes;
            self.digests.push(*digest);
            taken += 1;
        }
        taken
    }
}

/// Places `blobs`, each a digest and its size in bytes, into `limits.packs` packs and
/// `limits.large` large objects (`limits.packs` is at least 1); `manifest_reserve` is the most bytes the manifest record can
/// take, which pack 0 keeps room for, with one record.
///
/// # Errors
/// [`PlanError::TooLarge`] when a blob is over the large cap, there are more large blobs than
/// slots, a blob does not fit an empty pack, or the packs needed pass `limits.packs`;
/// [`PlanError::SizeMismatch`] when a size is not its digest's.
pub fn plan(
    blobs: &[(Digest, u64)],
    manifest_reserve: u64,
    limits: &Limits,
) -> Result<Plan, PlanError> {
    // Sorted and deduplicated by digest: the plan does not depend on the input order.
    let mut unique = BTreeSet::new();
    for (digest, size) in blobs {
        if *size != digest.size_bytes {
            return Err(PlanError::SizeMismatch {
                given: *size,
                digest: digest.size_bytes,
            });
        }
        if *size != 0 {
            unique.insert(*digest);
        }
    }
    let (large, small): (Vec<Digest>, Vec<Digest>) = unique
        .into_iter()
        .partition(|d| d.size_bytes > limits.record_bytes);
    if let Some(big) = large.iter().find(|d| d.size_bytes > limits.large_bytes) {
        return Err(PlanError::TooLarge(format!(
            "a blob of {} bytes is more than the {} a large object may hold",
            big.size_bytes, limits.large_bytes
        )));
    }
    if large.len() > limits.large {
        return Err(PlanError::TooLarge(format!(
            "{} blobs are over the {} byte record cap, and the run has {} large slots",
            large.len(),
            limits.record_bytes,
            limits.large
        )));
    }

    let too_many = || {
        PlanError::TooLarge(format!(
            "the outputs need more than the {} packs the run has",
            limits.packs
        ))
    };
    let mut rest = small.as_slice();
    // Pack 0 is placed last, but is slot 0; `filled` holds slots 1.. in order.
    let mut filled: Vec<Vec<Digest>> = Vec::new();
    let pack0 = loop {
        let mut last = Fill::new(manifest_reserve, 1);
        if last.take_from(rest, limits) == rest.len() {
            break last.digests;
        }
        // What is left does not fit pack 0: fill the next pack with what it can take.
        if filled.len() + 1 >= limits.packs {
            return Err(too_many());
        }
        let mut next = Fill::new(0, 0);
        let count = next.take_from(rest, limits);
        if count == 0 {
            return Err(PlanError::TooLarge(format!(
                "a blob of {} bytes does not fit an empty pack of {} bytes",
                rest[0].size_bytes, limits.pack_bytes
            )));
        }
        rest = &rest[count..];
        filled.push(next.digests);
    };
    let mut packs = vec![pack0];
    packs.extend(filled);
    Ok(Plan { packs, large })
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
