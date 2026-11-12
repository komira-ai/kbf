//! Laying out a task pack from its records' digests, then writing it from sources read once.
//!
//! [`write_pack_with`] builds a pack in memory from every blob's bytes. [`PackLayout`] builds the
//! same pack in two steps, for a writer that must not hold an output in memory:
//!
//! - [`PackLayout::plan`] decides, from each record's digest and CRC-32C alone (a [`BlobMeta`],
//!   which the writer has from hashing its outputs), where every record goes and what the footer
//!   is. It refuses what the pack's caps refuse, before a byte is written.
//! - [`PackLayout::write_to`] then streams each record from its source into a sink, a buffer at a
//!   time, checks that the source still had the bytes of its digest, and appends the footer. It
//!   returns the digest of the whole object, hashed as the bytes went out.
//!
//! Given the same records in the same order, the bytes are those [`write_pack_with`] writes for a
//! manifest whose record is the first: records back to back from offset 0 in the order given, each
//! digest once, no record of size 0, then the footer that `kbf_segments::layout::encode_footer`
//! builds, the code the segment writer's footer comes from too. So the readers
//! ([`read_manifest`], [`check_shape`], [`check_records`]) read a pack written either way.
//!
//! A pack 0 passes its manifest record (the bytes of [`TaskManifest::to_json_with`]) first; the
//! other packs of a run pass blobs only.
//!
//! [`write_pack_with`]: crate::write_pack_with
//! [`read_manifest`]: crate::read_manifest
//! [`check_shape`]: crate::check_shape
//! [`check_records`]: crate::check_records
//! [`TaskManifest::to_json_with`]: crate::TaskManifest::to_json_with

use std::collections::BTreeSet;
use std::io::{self, Read, Write};

use kbf_segments::layout::{encode_footer, footer_len};
use kbf_segments::{IndexEntry, RecordHasher};
use kbf_types::{Digest, DigestFunction};
use sha2::{Digest as _, Sha256};

use crate::task_pack::{MAX_PACK_BYTES, MAX_PACK_RECORDS, MAX_RECORD_BYTES};

/// The bytes of a source read and written at one time.
const COPY_BYTES: usize = 64 << 10;

/// What a layout knows of one record before its bytes are read: its digest (and so its size) and
/// its CRC-32C, the two a footer entry holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobMeta {
    /// The blob's SHA-256 digest; its size is the record's length.
    pub digest: Digest,
    /// The CRC-32C of the blob's bytes.
    pub crc32c: u32,
}

impl BlobMeta {
    /// The meta of `bytes`, a blob held in memory (the manifest record of pack 0).
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        Self {
            digest: kbf_segments::sha256(bytes),
            crc32c: kbf_segments::crc32c(bytes),
        }
    }
}

/// Where each record of one pack goes, and the pack's footer: everything about the pack but the
/// records' bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackLayout {
    /// The records in offset order, back to back from 0.
    records: Vec<IndexEntry>,
    /// The length of the records, which is where the footer starts.
    records_len: u64,
    /// The footer's bytes.
    footer: Vec<u8>,
}

/// Why records cannot be laid out as one task pack.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    /// Every blob given has size 0, or none was given: the pack would hold no record.
    #[error("the pack would hold no record")]
    NoRecords,
    /// A blob is longer than [`MAX_RECORD_BYTES`].
    #[error("a blob of {size} bytes is more than the {MAX_RECORD_BYTES} a record may hold")]
    RecordTooLarge {
        /// The blob's size.
        size: u64,
    },
    /// The distinct blobs are more than [`MAX_PACK_RECORDS`].
    #[error("the pack would hold {count} records, more than {MAX_PACK_RECORDS}")]
    TooManyRecords {
        /// The number of distinct blobs of size 1 or more.
        count: usize,
    },
    /// The records and the footer together are longer than [`MAX_PACK_BYTES`].
    #[error("the pack would have {len} bytes, more than {MAX_PACK_BYTES}")]
    TooLarge {
        /// The pack's length, footer included.
        len: u64,
    },
}

/// Why [`PackLayout::write_to`] did not write the whole pack. The sink then holds part of a pack,
/// which the caller discards.
#[derive(Debug, thiserror::Error)]
pub enum LayoutWriteError {
    /// The source of a record could not be opened or read.
    #[error("reading the record {digest}: {source}")]
    Source {
        /// The record's digest.
        digest: Digest,
        /// The source's error.
        #[source]
        source: io::Error,
    },
    /// The source of a record ended before the record's size, or its bytes are not those of the
    /// record's digest and CRC-32C: the blob changed after it was hashed.
    #[error("the source of the record {digest} no longer has its bytes")]
    Changed {
        /// The record's digest.
        digest: Digest,
    },
    /// The sink refused a write.
    #[error("writing the pack: {0}")]
    Sink(#[source] io::Error),
}

impl PackLayout {
    /// Lays out `blobs` as the records of one pack, in the order given: the first at offset 0, each
    /// after the one before. A blob of size 0 is skipped (it is implicit in every store and readers
    /// refuse a zero-size record), and a blob whose digest an earlier one has is skipped, as
    /// [`write_pack_with`](crate::write_pack_with) does. The footer indexes the records in digest
    /// order.
    ///
    /// # Errors
    /// [`LayoutError::RecordTooLarge`] for the first blob over [`MAX_RECORD_BYTES`]; then
    /// [`LayoutError::NoRecords`], [`LayoutError::TooManyRecords`] or [`LayoutError::TooLarge`] for
    /// records past [`MAX_PACK_RECORDS`] or, footer included, [`MAX_PACK_BYTES`].
    pub fn plan(blobs: &[BlobMeta]) -> Result<Self, LayoutError> {
        let mut seen = BTreeSet::new();
        let mut records = Vec::new();
        let mut records_len = 0u64;
        for blob in blobs {
            let size = blob.digest.size_bytes;
            if size == 0 || seen.contains(&blob.digest) {
                continue;
            }
            if size > MAX_RECORD_BYTES {
                return Err(LayoutError::RecordTooLarge { size });
            }
            seen.insert(blob.digest);
            records.push(IndexEntry {
                digest: blob.digest,
                offset: records_len,
                crc32c: blob.crc32c,
            });
            // At most 64 MiB a record, so the sum passes u64 only past 2^38 records.
            records_len += size;
        }
        if records.is_empty() {
            return Err(LayoutError::NoRecords);
        }
        if records.len() > MAX_PACK_RECORDS {
            return Err(LayoutError::TooManyRecords {
                count: records.len(),
            });
        }
        let len = records_len + footer_len(records.len() as u64);
        if len > MAX_PACK_BYTES {
            return Err(LayoutError::TooLarge { len });
        }
        let mut by_digest = records.clone();
        by_digest.sort_by_key(|e| e.digest);
        let footer = encode_footer(records_len, &by_digest);
        Ok(Self {
            records,
            records_len,
            footer,
        })
    }

    /// Writes the pack to `out`: for each record in offset order, the bytes `open` gives for its
    /// digest, then the footer. `open` is called once per record, never for a blob `plan` skipped,
    /// and exactly the record's size is read from what it returns, a buffer at a time; bytes past
    /// that size are not read. `out` is not flushed.
    ///
    /// Each record is checked as it goes out: a source that ends early, or whose bytes are not
    /// those of the record's digest and CRC-32C, stops the write after that record's bytes. A
    /// source that answers [`io::ErrorKind::Interrupted`] is read again.
    ///
    /// Returns the SHA-256 digest of every byte written, the whole object.
    ///
    /// # Errors
    /// [`LayoutWriteError::Source`] when `open` or a read fails;
    /// [`LayoutWriteError::Changed`] when a source does not have its record's bytes;
    /// [`LayoutWriteError::Sink`] when `out` refuses a write. `out` then holds part of the pack.
    pub fn write_to<'r, W, R>(
        &self,
        mut out: &mut W,
        mut open: impl FnMut(&Digest) -> io::Result<R>,
    ) -> Result<Digest, LayoutWriteError>
    where
        W: Write + ?Sized,
        R: Read + 'r,
    {
        // One body for every sink and source type, so each of its branches is one branch.
        self.write_dyn(&mut out, &mut |digest| {
            open(digest).map(|r| Box::new(r) as Box<dyn Read + 'r>)
        })
    }

    fn write_dyn<'r>(
        &self,
        out: &mut dyn Write,
        open: &mut dyn FnMut(&Digest) -> io::Result<Box<dyn Read + 'r>>,
    ) -> Result<Digest, LayoutWriteError> {
        let mut object = Sha256::new();
        let mut buf = vec![0u8; COPY_BYTES];
        for entry in &self.records {
            let digest = entry.digest;
            let source = |source| LayoutWriteError::Source { digest, source };
            let mut reader = open(&digest).map_err(source)?.take(digest.size_bytes);
            let mut record = RecordHasher::new();
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(source(e)),
                };
                let bytes = &buf[..n];
                record.update(bytes);
                object.update(bytes);
                out.write_all(bytes).map_err(LayoutWriteError::Sink)?;
            }
            if record.finish() != (digest, entry.crc32c) {
                return Err(LayoutWriteError::Changed { digest });
            }
        }
        object.update(&self.footer);
        out.write_all(&self.footer)
            .map_err(LayoutWriteError::Sink)?;
        Ok(Digest::new(
            DigestFunction::Sha256,
            object.finalize().into(),
            self.records_len + self.footer.len() as u64,
        ))
    }
}
