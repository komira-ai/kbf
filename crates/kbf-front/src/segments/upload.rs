//! One segment upload: what `Begin` declared, the ranges received, and the check that
//! turns received records into segments ready to store (`docs/design/write-path.md`,
//! section 4.1).

use std::collections::BTreeMap;

use bytes::Bytes;
use kbf_segments::layout::encode_footer;
use kbf_segments::{Footer, IndexEntry};
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
#[derive(Debug)]
pub(super) struct Receiving {
    buffer: Vec<u8>,
    /// Received ranges, start to end; they never overlap.
    ranges: BTreeMap<u64, u64>,
}

/// What a `PutRange` did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Put {
    /// The bytes were written.
    Written,
    /// The same range was received before; nothing was written.
    Resent,
}

impl Receiving {
    pub(super) fn new(declared: &Declared) -> Self {
        Self {
            // Zeroed pages are allocated lazily: memory follows the bytes received.
            buffer: vec![0; usize::try_from(declared.length).unwrap_or(usize::MAX)],
            ranges: BTreeMap::new(),
        }
    }

    /// Writes `bytes` at `offset` of the record area, unless they lie outside it or
    /// overlap a received range with other bounds. A range received before, with the
    /// same bounds, is acknowledged and nothing is written: its bytes may already have
    /// been read by a commit's check.
    ///
    /// # Errors
    /// INVALID_ARGUMENT for an empty range, one outside the records, or an overlap.
    pub(super) fn put(
        &mut self,
        records_len: u64,
        offset: u64,
        bytes: &[u8],
    ) -> Result<Put, Status> {
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
            return Ok(Put::Resent);
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
        Ok(Put::Written)
    }

    /// The first record of `declared` not wholly covered by received ranges.
    pub(super) fn first_missing<'a>(&self, declared: &'a Declared) -> Option<&'a Digest> {
        declared
            .entries
            .iter()
            .find(|e| !self.covers(e.offset, e.range().end))
            .map(|e| &e.digest)
    }

    /// Whether every byte of `start..end` was received.
    fn covers(&self, start: u64, end: u64) -> bool {
        let mut at = start;
        for (&from, &to) in self.ranges.range(..end) {
            if to <= at {
                continue;
            }
            if from > at {
                return false;
            }
            at = to;
            if at >= end {
                return true;
            }
        }
        at >= end
    }

    /// Checks every record against its digest and builds the stored segment: the
    /// received records, then a footer built here from the declared (digest, offset)
    /// entries and the CRC-32C of the received bytes. The daemon's CRC-32C values are
    /// never used.
    ///
    /// # Errors
    /// INVALID_ARGUMENT naming the first record whose bytes do not hash to its digest.
    pub(super) fn check(self, declared: &Declared) -> Result<BuiltSegment, Status> {
        let mut buffer = self.buffer;
        let mut entries = Vec::with_capacity(declared.entries.len());
        for entry in &declared.entries {
            let range = entry.range();
            // Inside the record area, which `declare` checked lies inside the buffer.
            let record = &buffer[range.start as usize..range.end as usize];
            let actual = kbf_segments::sha256(record);
            if actual != entry.digest {
                return Err(Status::invalid_argument(format!(
                    "record at {} sent as {} hashes to {actual}",
                    entry.offset, entry.digest
                )));
            }
            entries.push(IndexEntry {
                digest: entry.digest,
                offset: entry.offset,
                crc32c: kbf_segments::crc32c(record),
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
