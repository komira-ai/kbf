//! Building a task's pack: one kbf-segments object holding the manifest at offset 0 and
//! every blob the manifest names, each blob once.
//!
//! [`kbf_segments::SegmentWriter`] lays records out in the order they are pushed, so the
//! manifest is pushed first and its record starts at offset 0. [`verify`] is the reader's
//! side of the same contract: the frontend's check, in a form tests can use.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use kbf_segments::{MAX_SEGMENT_BYTES, SegmentError, SegmentReader, SegmentWriter, WriteError};

use crate::manifest::{BlobId, Manifest, ParseError};

/// The most distinct records (the manifest included) one pack may hold.
pub const MAX_ENTRIES: usize = 16384;

/// The most bytes one pack may have, footer included: the segment limit.
pub const MAX_PACK_BYTES: u64 = MAX_SEGMENT_BYTES;

/// One blob to put in the pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blob {
    /// Bytes already in memory.
    Bytes(Vec<u8>),
    /// A file, read when the pack is built. It is read at most up to the pack limit,
    /// so a file larger than the pack is refused without being held whole.
    File(PathBuf),
}

/// Why a pack was not built.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    /// The pack would exceed the byte limit.
    #[error("the pack would exceed the {limit}-byte limit of one segment")]
    TooLarge {
        /// The limit in bytes.
        limit: u64,
    },
    /// The pack would hold more records than the limit.
    #[error("the pack would hold more than {limit} distinct records")]
    TooManyEntries {
        /// The limit in records.
        limit: usize,
    },
    /// The manifest names a blob that was not supplied.
    #[error("the manifest names {0}, which is not among the blobs")]
    Missing(BlobId),
    /// A file blob could not be read.
    #[error("cannot read {}: {source}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// The failure.
        source: std::io::Error,
    },
}

/// Builds the pack: `manifest` at offset 0, then each of `blobs` once (blobs with equal
/// content share one record).
///
/// # Errors
/// [`PackError`]: over the byte or record limit, a blob the manifest names but
/// `blobs` lacks, or an unreadable file.
pub fn pack(
    manifest: &Manifest,
    blobs: impl IntoIterator<Item = Blob>,
) -> Result<Vec<u8>, PackError> {
    pack_within(MAX_PACK_BYTES, MAX_ENTRIES, manifest, blobs)
}

/// [`pack`] with the limits as parameters, so tests reach them with small inputs.
pub(crate) fn pack_within(
    limit: u64,
    max_entries: usize,
    manifest: &Manifest,
    blobs: impl IntoIterator<Item = Blob>,
) -> Result<Vec<u8>, PackError> {
    let mut writer = SegmentWriter::new(limit);
    let mut held: BTreeSet<BlobId> = BTreeSet::new();
    let mut add = |writer: &mut SegmentWriter, bytes: &[u8]| -> Result<(), PackError> {
        let id = BlobId::of(bytes);
        if !held.contains(&id) {
            if held.len() >= max_entries {
                return Err(PackError::TooManyEntries { limit: max_entries });
            }
            // `Full` and `TooLarge` are both a size refusal.
            writer
                .push(bytes)
                .map_err(|_: WriteError| PackError::TooLarge { limit })?;
            held.insert(id);
        }
        Ok(())
    };
    add(&mut writer, &manifest.encode())?;
    for blob in blobs {
        match blob {
            Blob::Bytes(bytes) => add(&mut writer, &bytes)?,
            Blob::File(path) => add(&mut writer, &read_capped(&path, limit)?)?,
        }
    }
    if let Some(missing) = manifest.blobs().find(|id| !held.contains(id)) {
        return Err(PackError::Missing(missing));
    }
    Ok(writer.finish())
}

/// The contents of `path`, refusing one longer than `limit` after reading `limit + 1`.
fn read_capped(path: &PathBuf, limit: u64) -> Result<Vec<u8>, PackError> {
    let read = |source| PackError::Read {
        path: path.clone(),
        source,
    };
    let mut out = Vec::new();
    File::open(path)
        .map_err(read)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(|source| PackError::Read {
            path: path.clone(),
            source,
        })?;
    if out.len() as u64 > limit {
        return Err(PackError::TooLarge { limit });
    }
    Ok(out)
}

/// Why a pack does not satisfy the contract.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// The footer or a record is damaged.
    #[error("not a valid segment: {0}")]
    Segment(#[from] SegmentError),
    /// No non-empty record starts at offset 0.
    #[error("no record at offset 0")]
    NoManifest,
    /// The record at offset 0 is not a manifest.
    #[error(transparent)]
    Manifest(#[from] ParseError),
    /// The manifest names a blob that is not a record.
    #[error("the manifest names {0}, which is not a record of the pack")]
    Missing(BlobId),
}

/// Checks a pack as the frontend does: the footer and the record at offset 0, which
/// must parse as a manifest, and every blob the manifest names must be a record.
///
/// # Errors
/// [`VerifyError`].
pub fn verify(bytes: &[u8]) -> Result<Manifest, VerifyError> {
    let reader = SegmentReader::open(bytes)?;
    let entries = reader.footer().entries();
    let first = entries
        .iter()
        .find(|e| e.offset == 0 && e.digest.size_bytes > 0)
        .ok_or(VerifyError::NoManifest)?;
    let record = reader.get(&first.digest)?.ok_or(VerifyError::NoManifest)?;
    let manifest = Manifest::parse(record)?;
    let held: BTreeSet<BlobId> = entries
        .iter()
        .map(|e| BlobId {
            hash: e.digest.hash,
            size: e.digest.size_bytes,
        })
        .collect();
    if let Some(missing) = manifest.blobs().find(|id| !held.contains(id)) {
        return Err(VerifyError::Missing(missing));
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::tests::sample;

    /// The blobs `sample()` names.
    fn blobs() -> Vec<Blob> {
        ["out", "", "file", "tree"]
            .iter()
            .map(|s| Blob::Bytes(s.as_bytes().to_vec()))
            .collect()
    }

    /// The manifest is the record at offset 0, and every named blob can be read back
    /// by its digest.
    #[test]
    fn manifest_sits_at_offset_zero() {
        let m = sample();
        let bytes = pack(&m, blobs()).unwrap();
        assert_eq!(&bytes[..m.encode().len()], m.encode().as_slice());
        let reader = SegmentReader::open(&bytes).unwrap();
        assert_eq!(reader.footer().entries().len(), 5);
        for (id, content) in [(m.stdout, "out"), (m.output_files[0].digest, "file")] {
            let e = reader
                .footer()
                .entries()
                .iter()
                .find(|e| e.digest.hash == id.hash)
                .unwrap();
            assert_eq!(reader.get(&e.digest).unwrap().unwrap(), content.as_bytes());
        }
        assert_eq!(verify(&bytes).unwrap(), m);
    }

    /// The manifest stays at offset 0 whatever order blobs arrive in, even when the
    /// first blob is empty.
    #[test]
    fn empty_first_blob_does_not_displace_the_manifest() {
        let m = sample();
        let mut b = blobs();
        b.insert(0, Blob::Bytes(Vec::new()));
        let bytes = pack(&m, b).unwrap();
        assert_eq!(verify(&bytes).unwrap(), m);
    }

    /// Equal content is one record, whether it arrives twice or from memory and a file.
    #[test]
    fn duplicate_blobs_share_a_record() {
        let m = sample();
        let path = scratch("dup");
        std::fs::write(&path, b"out").unwrap();
        let mut b = blobs();
        b.push(Blob::Bytes(b"out".to_vec()));
        b.push(Blob::File(path.clone()));
        let bytes = pack(&m, b).unwrap();
        std::fs::remove_file(&path).unwrap();
        let reader = SegmentReader::open(&bytes).unwrap();
        assert_eq!(reader.footer().entries().len(), 5);
    }

    #[test]
    fn file_blob_is_read() {
        let m = sample();
        let path = scratch("file");
        std::fs::write(&path, b"file").unwrap();
        let mut b = blobs();
        b.retain(|x| *x != Blob::Bytes(b"file".to_vec()));
        b.push(Blob::File(path.clone()));
        let bytes = pack(&m, b).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(verify(&bytes).unwrap(), m);
    }

    #[test]
    fn missing_file_is_a_read_error() {
        let err = pack(&sample(), [Blob::File(scratch("absent"))]).unwrap_err();
        assert!(matches!(err, PackError::Read { .. }));
        assert!(err.to_string().contains("cannot read"));
    }

    #[test]
    fn unlisted_blob_is_refused() {
        let mut b = blobs();
        b.pop();
        let m = sample();
        let err = pack(&m, b).unwrap_err();
        assert!(matches!(err, PackError::Missing(id) if id == m.output_directories[0].tree));
    }

    /// The byte limit names itself, for bytes, for a file longer than the limit, and for
    /// a manifest alone that does not fit.
    #[test]
    fn byte_limit_is_refused() {
        let m = sample();
        let manifest_len = m.encode().len() as u64;
        // Room for the manifest, the small blobs and the footer, not for a 10 000-byte one.
        let limit = manifest_len + 100 + kbf_segments::layout::footer_len(6);
        let mut b = blobs();
        b.push(Blob::Bytes(vec![7; 10_000]));
        let err = pack_within(limit, 10, &m, b).unwrap_err();
        assert!(matches!(err, PackError::TooLarge { limit: l } if l == limit));
        assert!(err.to_string().contains(&limit.to_string()));

        let path = scratch("big");
        std::fs::write(&path, vec![1u8; 2000]).unwrap();
        let err = pack_within(limit, 10, &m, [Blob::File(path.clone())]).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(err, PackError::TooLarge { .. }));

        let err = pack_within(10, 10, &m, blobs()).unwrap_err();
        assert!(matches!(err, PackError::TooLarge { limit: 10 }));

        // Many small blobs overflowing what is left after the first fit.
        let many = (0u32..200).map(|i| Blob::Bytes(format!("blob {i}").into_bytes()));
        let err = pack_within(limit, 1000, &m, many).unwrap_err();
        assert!(matches!(err, PackError::TooLarge { .. }));
    }

    /// The record limit counts distinct records, manifest included, and a repeated blob
    /// does not count twice.
    #[test]
    fn record_limit_is_refused() {
        let m = sample();
        let err = pack_within(MAX_PACK_BYTES, 5, &m, {
            let mut b = blobs();
            b.push(Blob::Bytes(b"one more".to_vec()));
            b
        })
        .unwrap_err();
        assert!(matches!(err, PackError::TooManyEntries { limit: 5 }));
        assert!(err.to_string().contains('5'));

        let mut again = blobs();
        again.extend(blobs());
        pack_within(MAX_PACK_BYTES, 5, &m, again).unwrap();
    }

    #[test]
    fn verify_refuses_what_it_should() {
        // Not a segment.
        assert!(matches!(verify(b"junk"), Err(VerifyError::Segment(_))));
        let err = verify(b"junk").unwrap_err();
        assert!(err.to_string().contains("not a valid segment"));

        // Empty segment: nothing at offset 0.
        let empty = SegmentWriter::new(MAX_PACK_BYTES).finish();
        assert!(matches!(verify(&empty), Err(VerifyError::NoManifest)));
        assert_eq!(
            verify(&empty).unwrap_err().to_string(),
            "no record at offset 0"
        );

        // Offset 0 is not a manifest.
        let mut w = SegmentWriter::new(MAX_PACK_BYTES);
        w.push(b"not a manifest").unwrap();
        assert!(matches!(verify(&w.finish()), Err(VerifyError::Manifest(_))));

        // A manifest first, but a named blob absent.
        let m = sample();
        let mut w = SegmentWriter::new(MAX_PACK_BYTES);
        w.push(&m.encode()).unwrap();
        let err = verify(&w.finish()).unwrap_err();
        assert!(matches!(err, VerifyError::Missing(id) if id == m.stdout));
        assert!(err.to_string().contains("not a record"));

        // A manifest that is not first is not found at offset 0.
        let mut w = SegmentWriter::new(MAX_PACK_BYTES);
        w.push(b"first").unwrap();
        w.push(&m.encode()).unwrap();
        assert!(matches!(verify(&w.finish()), Err(VerifyError::Manifest(_))));
    }

    /// A path in the system temp directory, unique to this process and name.
    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kbf-task-pack-{}-{name}", std::process::id()))
    }
}
