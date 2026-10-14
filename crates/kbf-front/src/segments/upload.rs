//! One segment upload: what `Begin` declared, the ranges received, and the check that
//! turns received records into segments ready to store (`docs/design/write-path.md`,
//! section 4.1).

use std::collections::BTreeMap;

use bytes::Bytes;
use kbf_segments::layout::encode_footer;
use kbf_segments::{Footer, IndexEntry, RecordHasher};
use kbf_types::Digest;
use tonic::Status;

use crate::cache::BuiltSegment;

/// One segment as `Begin` declared it, with its footer parsed and checked.
#[derive(Debug)]
pub(super) struct Declared {
    /// The whole segment's length: records, then footer.
    pub length: u64,
    /// The length of the record area (the footer's `index_offset`).
    pub records_len: u64,
    /// The index, in digest order.
    pub entries: Vec<IndexEntry>,
}

/// Parses and checks `Begin`'s segments: each footer must parse for its segment's
/// length, its records must not overlap, and every record must be at most `max_blob`
/// bytes. Returns them with the upload's total length and record count.
///
/// # Errors
/// INVALID_ARGUMENT naming the segment and why.
pub(super) fn declare(
    segments: &[(u64, &[u8])],
    max_blob: u64,
) -> Result<(Vec<Declared>, u64, usize), Status> {
    if segments.is_empty() {
        return Err(Status::invalid_argument(
            "an upload needs at least one segment",
        ));
    }
    let mut declared = Vec::with_capacity(segments.len());
    let (mut total, mut records) = (0u64, 0usize);
    for (i, &(length, footer)) in segments.iter().enumerate() {
        let parsed = Footer::parse(footer, length)
            .map_err(|e| Status::invalid_argument(format!("segment {i}: {e}")))?;
        // `parse` read the footer from the end of `footer` and checked that records
        // plus footer are `length`; anything in front of it is refused, not ignored.
        let footer_len = length - parsed.records_len();
        if footer.len() as u64 != footer_len {
            return Err(Status::invalid_argument(format!(
                "segment {i}: {} footer bytes sent for a {footer_len}-byte footer",
                footer.len()
            )));
        }
        let mut by_offset: Vec<&IndexEntry> = parsed.entries().iter().collect();
        by_offset.sort_by_key(|e| e.offset);
        for pair in by_offset.windows(2) {
            if pair[0].range().end > pair[1].offset {
                return Err(Status::invalid_argument(format!(
                    "segment {i}: records {} and {} overlap",
                    pair[0].digest, pair[1].digest
                )));
            }
        }
        if let Some(e) = parsed
            .entries()
            .iter()
            .find(|e| e.digest.size_bytes > max_blob)
        {
            return Err(Status::invalid_argument(format!(
                "segment {i}: {} is larger than the largest blob the cache accepts \
                 ({max_blob} bytes)",
                e.digest
            )));
        }
        total = total.saturating_add(length);
        records += parsed.entries().len();
        declared.push(Declared {
            length,
            records_len: parsed.records_len(),
            entries: parsed.entries().to_vec(),
        });
    }
    Ok((declared, total, records))
}

/// The ranges of one segment received so far, and the bytes they carried, written into
/// a buffer the segment's length (records, then room for the footer the server builds).
///
/// Each record is checked as its bytes arrive: its SHA-256 and CRC-32C advance over
/// the bytes received contiguously from its start, whichever order the ranges came in,
/// so at the last range only the records it completed have anything left to hash.
#[derive(Debug)]
pub(super) struct Receiving {
    buffer: Vec<u8>,
    /// Received ranges, start to end; they never overlap.
    ranges: BTreeMap<u64, u64>,
    /// Indices into the declared entries, by record offset.
    by_offset: Vec<usize>,
    /// One per declared entry.
    records: Vec<Record>,
    /// The first record found not to hash to its digest.
    bad: Option<String>,
}

/// How far one record has been checked.
#[derive(Debug, Default)]
struct Record {
    /// The hash of the first `done` bytes, absent while a task advances it.
    hasher: Option<RecordHasher>,
    done: u64,
    /// A task is advancing this record.
    busy: bool,
    /// The record's CRC-32C once all its bytes hashed to its digest.
    crc: Option<u32>,
}

/// What a task holds while it hashes one step of a record.
pub(super) type Held = (RecordHasher, Vec<u8>);

/// What a `PutRange` did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Put {
    /// The bytes were written.
    Written,
    /// The same range was received before; nothing was written.
    Resent,
}

fn mismatch(entry: &IndexEntry, actual: &Digest) -> String {
    format!(
        "record at {} sent as {} hashes to {actual}",
        entry.offset, entry.digest
    )
}

/// Runs the records `claimed` by a `put` to the end of the bytes received so far.
/// `next` is [`Receiving::next`] under whatever lock holds the segment.
pub(super) fn advance<F>(claimed: &[usize], mut next: F)
where
    F: FnMut(usize, Option<(RecordHasher, usize)>) -> Option<Held>,
{
    for &i in claimed {
        let mut finished = None;
        while let Some((mut hasher, bytes)) = next(i, finished.take()) {
            hasher.update(&bytes);
            finished = Some((hasher, bytes.len()));
        }
    }
}

impl Receiving {
    pub(super) fn new(declared: &Declared) -> Self {
        let mut by_offset: Vec<usize> = (0..declared.entries.len()).collect();
        by_offset.sort_by_key(|&i| declared.entries[i].offset);
        let mut records: Vec<Record> = declared.entries.iter().map(|_| Record::default()).collect();
        let mut bad = None;
        // An empty record has no bytes to wait for.
        for (entry, record) in declared.entries.iter().zip(&mut records) {
            if entry.digest.size_bytes == 0 {
                let (actual, crc) = RecordHasher::new().finish();
                if actual == entry.digest {
                    record.crc = Some(crc);
                } else {
                    bad.get_or_insert_with(|| mismatch(entry, &actual));
                }
            }
        }
        Self {
            // Zeroed pages are allocated lazily: memory follows the bytes received.
            buffer: vec![0; usize::try_from(declared.length).unwrap_or(usize::MAX)],
            ranges: BTreeMap::new(),
            by_offset,
            records,
            bad,
        }
    }

    /// Writes `bytes` at `offset` of the record area, unless they lie outside it or
    /// overlap a received range with other bounds. A range received before, with the
    /// same bounds, is acknowledged and nothing is written: its bytes may already have
    /// been read by the check.
    ///
    /// With a write, returns the records whose check can go on, claimed for the caller,
    /// who must run them with [`advance`].
    ///
    /// # Errors
    /// INVALID_ARGUMENT for an empty range, one outside the records, or an overlap.
    pub(super) fn put(
        &mut self,
        declared: &Declared,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(Put, Vec<usize>), Status> {
        let records_len = declared.records_len;
        let end = offset
            .checked_add(bytes.len() as u64)
            .filter(|&end| end <= records_len && end > offset)
            .ok_or_else(|| {
                Status::invalid_argument(format!(
                    "range of {} bytes at {offset} is empty or outside the {records_len}-byte \
                     record area",
                    bytes.len()
                ))
            })?;
        if self.ranges.get(&offset) == Some(&end) {
            return Ok((Put::Resent, Vec::new()));
        }
        let before = self.ranges.range(..end).next_back();
        if let Some((&start, &stop)) = before
            && stop > offset
        {
            return Err(Status::invalid_argument(format!(
                "range {offset}..{end} overlaps the received range {start}..{stop}"
            )));
        }
        // `end <= records_len <= buffer.len()`, both fit in usize.
        let (from, to) = (offset as usize, end as usize);
        self.buffer[from..to].copy_from_slice(bytes);
        self.ranges.insert(offset, end);
        Ok((Put::Written, self.claim(declared, offset, end)))
    }

    /// Claims the records overlapping `start..end` that no task is advancing and that
    /// have bytes to hash.
    fn claim(&mut self, declared: &Declared, start: u64, end: u64) -> Vec<usize> {
        let first = self
            .by_offset
            .partition_point(|&i| declared.entries[i].range().end <= start);
        let mut claimed = Vec::new();
        for k in first..self.by_offset.len() {
            let i = self.by_offset[k];
            let entry = &declared.entries[i];
            if entry.offset >= end {
                break;
            }
            let record = &self.records[i];
            if record.busy || record.crc.is_some() || entry.digest.size_bytes == 0 {
                continue;
            }
            let at = entry.offset + record.done;
            if self.covered_until(at, entry.range().end) > at {
                self.records[i].busy = true;
                claimed.push(i);
            }
        }
        claimed
    }

    /// How far the received bytes run unbroken from `from`, up to `cap`.
    fn covered_until(&self, from: u64, cap: u64) -> u64 {
        let mut at = from;
        while at < cap {
            match self.ranges.range(..=at).next_back() {
                Some((_, &stop)) if stop > at => at = stop.min(cap),
                _ => break,
            }
        }
        at
    }

    /// One step of the check of record `i`, which the caller claimed: takes back the
    /// hasher and length `finished` of the last step, and returns the hasher with the
    /// next bytes received unbroken, or `None` when there are none (the record is then
    /// no longer claimed) or the record is complete (its CRC-32C is kept if it hashed to
    /// its digest; if not, [`Receiving::refusal`] names it).
    pub(super) fn next(
        &mut self,
        declared: &Declared,
        i: usize,
        finished: Option<(RecordHasher, usize)>,
    ) -> Option<Held> {
        let entry = &declared.entries[i];
        let record = &mut self.records[i];
        if let Some((hasher, len)) = finished {
            record.done += len as u64;
            record.hasher = Some(hasher);
        }
        if record.done == entry.digest.size_bytes {
            record.busy = false;
            let (actual, crc) = record.hasher.take().unwrap_or_default().finish();
            if actual == entry.digest {
                record.crc = Some(crc);
            } else {
                self.bad.get_or_insert_with(|| mismatch(entry, &actual));
            }
            return None;
        }
        let from = entry.offset + record.done;
        let upto = self.covered_until(from, entry.range().end);
        let record = &mut self.records[i];
        if upto == from {
            record.busy = false;
            return None;
        }
        let hasher = record.hasher.take().unwrap_or_default();
        // Inside the record area, which `declare` checked lies inside the buffer.
        Some((hasher, self.buffer[from as usize..upto as usize].to_vec()))
    }

    /// The refusal for the first record found not to hash to its digest, if any.
    pub(super) fn refusal(&self) -> Option<Status> {
        self.bad.as_deref().map(Status::invalid_argument)
    }

    /// Whether a task is still checking some record.
    pub(super) fn checking(&self) -> bool {
        self.records.iter().any(|r| r.busy)
    }

    /// The first record of `declared` not wholly covered by received ranges.
    pub(super) fn first_missing<'a>(&self, declared: &'a Declared) -> Option<&'a Digest> {
        declared
            .entries
            .iter()
            .find(|e| self.covered_until(e.offset, e.range().end) < e.range().end)
            .map(|e| &e.digest)
    }

    /// Builds the stored segment from records already checked as they arrived: the
    /// received records, then a footer built here from the declared (digest, offset)
    /// entries and the CRC-32C the check computed. The daemon's CRC-32C values are
    /// never used.
    ///
    /// # Errors
    /// INVALID_ARGUMENT for a record that did not hash to its digest; FAILED_PRECONDITION
    /// for one not checked (a commit that raced the ranges it follows).
    pub(super) fn check(self, declared: &Declared) -> Result<BuiltSegment, Status> {
        if let Some(refused) = self.refusal() {
            return Err(refused);
        }
        let mut buffer = self.buffer;
        let mut entries = Vec::with_capacity(declared.entries.len());
        for (i, entry) in declared.entries.iter().enumerate() {
            let crc32c = self.records[i].crc.ok_or_else(|| {
                Status::failed_precondition(format!("record {} was not checked", entry.digest))
            })?;
            entries.push(IndexEntry {
                digest: entry.digest,
                offset: entry.offset,
                crc32c,
            });
        }
        let footer = encode_footer(declared.records_len, &entries);
        // The same entry count as the declared footer, so the same length: the buffer's
        // tail after the records.
        buffer[declared.records_len as usize..].copy_from_slice(&footer);
        Ok(BuiltSegment {
            body: Bytes::from(buffer),
            entries,
        })
    }
}

#[cfg(test)]
#[path = "upload_tests.rs"]
mod tests;
