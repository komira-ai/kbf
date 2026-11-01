//! The shape of a task pack, and reading and writing one.
//!
//! A task pack is one `kbf-segments` object (`kbf_segments::layout`) whose first record, at
//! offset 0, is the [`TaskManifest`] and whose other records are the blobs the manifest names.
//! [`check_shape`] is the part of the rules that needs only the footer: the same function for
//! the task that writes a pack before it sends it and for the frontend that reads the object's
//! footer before it fetches a byte of a record. It is not the frontend's verification: every
//! record's hash, the manifest's claims against the lease and the closure check are ingest's.

use kbf_segments::{
    Footer, IndexEntry, MAX_SEGMENT_BYTES, SegmentError, SegmentReader, SegmentWriter, WriteError,
};

use crate::task_manifest::{MAX_MANIFEST_BYTES, ManifestError, TaskManifest};

/// The longest pack object, footer included.
pub const MAX_PACK_BYTES: u64 = MAX_SEGMENT_BYTES;

/// The most records a pack may hold, the manifest among them.
pub const MAX_PACK_RECORDS: usize = 16_384;

/// The longest record. A larger blob is never read from a pack by a daemon, so an output over
/// this is a task failure, not a bigger pack.
pub const MAX_RECORD_BYTES: u64 = 64 << 20;

/// Why the footer of a pack is not the footer of a task pack.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ShapeError {
    /// The object is longer than [`MAX_PACK_BYTES`].
    #[error("the pack has {len} bytes, more than {MAX_PACK_BYTES}")]
    TooLarge {
        /// The object's length.
        len: u64,
    },
    /// The footer indexes no record, so there is no manifest.
    #[error("the pack holds no record, so no manifest")]
    NoRecords,
    /// The footer indexes more than [`MAX_PACK_RECORDS`] records.
    #[error("the pack holds {count} records, more than {MAX_PACK_RECORDS}")]
    TooManyRecords {
        /// The number of records.
        count: usize,
    },
    /// A record of size 0. The empty blob is implicit in every store and is never a record;
    /// one at offset 0 would also leave the manifest ambiguous.
    #[error("the record at offset {offset} has size 0")]
    ZeroSizeRecord {
        /// Where the record starts.
        offset: u64,
    },
    /// A record longer than [`MAX_RECORD_BYTES`].
    #[error("the record at offset {offset} has {size} bytes, more than {MAX_RECORD_BYTES}")]
    RecordTooLarge {
        /// Where the record starts.
        offset: u64,
        /// The record's size.
        size: u64,
    },
    /// Records are not back to back from offset 0 to the footer: a gap, an overlap, a first
    /// record that does not start at 0 or a last one that does not end at the footer.
    #[error("records are not contiguous: expected one at offset {expected}, found {found}")]
    NotContiguous {
        /// Where the next record had to start, or where the records had to end.
        expected: u64,
        /// Where the next record starts, or where the footer starts.
        found: u64,
    },
    /// The record at offset 0 is longer than [`MAX_MANIFEST_BYTES`].
    #[error("the manifest record has {size} bytes, more than {MAX_MANIFEST_BYTES}")]
    ManifestTooLarge {
        /// The record's size.
        size: u64,
    },
}

/// Why a pack could not be read as a task pack.
#[derive(Debug, thiserror::Error)]
pub enum TaskPackError {
    /// The object is not a segment, or its footer or the manifest record is damaged.
    #[error(transparent)]
    Segment(#[from] SegmentError),
    /// The footer is not that of a task pack.
    #[error(transparent)]
    Shape(#[from] ShapeError),
    /// The record at offset 0 is not a valid manifest.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

/// Why a pack could not be written.
#[derive(Debug, thiserror::Error)]
pub enum WritePackError {
    /// The manifest is refused.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// A blob is longer than [`MAX_RECORD_BYTES`].
    #[error("a blob of {size} bytes is more than the {MAX_RECORD_BYTES} a record may hold")]
    RecordTooLarge {
        /// The blob's size.
        size: u64,
    },
    /// The records together do not fit [`MAX_PACK_BYTES`].
    #[error(transparent)]
    Segment(#[from] WriteError),
}

/// Checks that `footer`, the footer of an object of `pack_len` bytes, is that of a task pack, and
/// returns the entry of the manifest: the record at offset 0.
///
/// The rules of [`check_records`], and the manifest at most [`MAX_MANIFEST_BYTES`]. This is the
/// check of pack 0, whose first record is the manifest.
///
/// # Errors
/// The first rule that fails, as a [`ShapeError`].
pub fn check_shape(footer: &Footer, pack_len: u64) -> Result<&IndexEntry, ShapeError> {
    let first = check_records(footer, pack_len)?;
    if first.digest.size_bytes > MAX_MANIFEST_BYTES {
        return Err(ShapeError::ManifestTooLarge {
            size: first.digest.size_bytes,
        });
    }
    Ok(first)
}

/// Checks that `footer`, the footer of an object of `pack_len` bytes, lays out records as a task
/// pack does, and returns the entry at offset 0. This is the whole check of a pack that holds no
/// manifest (packs 1.. of a run), whose first record may be as large as any.
///
/// The rules: at most [`MAX_PACK_BYTES`]; 1 to [`MAX_PACK_RECORDS`] records; none of size 0 and
/// none over [`MAX_RECORD_BYTES`]; records back to back in offset order from 0, the last ending
/// where the footer starts. A zero-size record is reported before a gap, so one defect has one
/// reason.
///
/// # Errors
/// The first rule that fails, as a [`ShapeError`] (never [`ShapeError::ManifestTooLarge`]).
pub fn check_records(footer: &Footer, pack_len: u64) -> Result<&IndexEntry, ShapeError> {
    if pack_len > MAX_PACK_BYTES {
        return Err(ShapeError::TooLarge { len: pack_len });
    }
    let mut by_offset: Vec<&IndexEntry> = footer.entries().iter().collect();
    if by_offset.is_empty() {
        return Err(ShapeError::NoRecords);
    }
    if by_offset.len() > MAX_PACK_RECORDS {
        return Err(ShapeError::TooManyRecords {
            count: by_offset.len(),
        });
    }
    by_offset.sort_by_key(|e| e.offset);
    let mut next = 0u64;
    for entry in &by_offset {
        let size = entry.digest.size_bytes;
        if size == 0 {
            return Err(ShapeError::ZeroSizeRecord {
                offset: entry.offset,
            });
        }
        if size > MAX_RECORD_BYTES {
            return Err(ShapeError::RecordTooLarge {
                offset: entry.offset,
                size,
            });
        }
        if entry.offset != next {
            return Err(ShapeError::NotContiguous {
                expected: next,
                found: entry.offset,
            });
        }
        next = entry.offset + size;
    }
    if next != footer.records_len() {
        return Err(ShapeError::NotContiguous {
            expected: next,
            found: footer.records_len(),
        });
    }
    Ok(by_offset[0])
}

/// Reads the manifest of `pack`, the whole object: checks the footer, the shape, the manifest
/// record's CRC and digest, and the manifest itself. The other records are not read.
///
/// # Errors
/// [`TaskPackError::Segment`], [`TaskPackError::Shape`] or [`TaskPackError::Manifest`].
pub fn read_manifest(pack: &[u8]) -> Result<TaskManifest, TaskPackError> {
    let manifest = read_manifest_with(pack, 0)?;
    manifest.check_single_pack()?;
    Ok(manifest)
}

/// [`read_manifest`] for a reader that accepts large objects of up to `large_max` bytes.
///
/// # Errors
/// Those of [`read_manifest`].
pub fn read_manifest_with(pack: &[u8], large_max: u64) -> Result<TaskManifest, TaskPackError> {
    let reader = SegmentReader::open(pack)?;
    let entry = check_shape(reader.footer(), pack.len() as u64)?;
    let range = entry.range();
    // The footer checked that every record lies inside the object, so the slice is in bounds.
    let record = &pack[range.start as usize..range.end as usize];
    entry.verify(record)?;
    Ok(TaskManifest::parse_with(record, large_max)?)
}

/// Writes a task pack: the manifest first, so it is the record at offset 0, then `blobs`, each
/// once. An empty blob is skipped (it is implicit in every store and a zero-size record is
/// refused by readers), and a blob equal to one already written is a no-op.
///
/// The pack is not checked against the manifest: whether every digest the manifest names is in
/// `blobs` or already in the store is the caller's to know and the frontend's to check.
///
/// # Errors
/// [`WritePackError::Manifest`] for a manifest that [`TaskManifest::to_json`] refuses;
/// [`WritePackError::RecordTooLarge`] for a blob over [`MAX_RECORD_BYTES`];
/// [`WritePackError::Segment`] when the records pass [`MAX_PACK_BYTES`].
pub fn write_pack(manifest: &TaskManifest, blobs: &[&[u8]]) -> Result<Vec<u8>, WritePackError> {
    manifest.validate()?;
    write_pack_with(manifest, blobs, 0)
}

/// [`write_pack`] for a writer whose manifest may name large objects of up to `large_max` bytes.
///
/// # Errors
/// Those of [`write_pack`].
pub fn write_pack_with(
    manifest: &TaskManifest,
    blobs: &[&[u8]],
    large_max: u64,
) -> Result<Vec<u8>, WritePackError> {
    let mut writer = SegmentWriter::new(MAX_PACK_BYTES);
    writer.push(&manifest.to_json_with(large_max)?)?;
    for blob in blobs.iter().filter(|b| !b.is_empty()) {
        let size = blob.len() as u64;
        if size > MAX_RECORD_BYTES {
            return Err(WritePackError::RecordTooLarge { size });
        }
        writer.push(blob)?;
    }
    Ok(writer.finish())
}
