//! Building a task's pack: one kbf-segments object holding the manifest at offset 0 and
//! every non-empty blob the manifest names, each blob once.
//!
//! The empty blob is never a record: every store has it implicitly, so the manifest names it by
//! its digest and the pack does not carry it. A pack that would pass a limit (the object, the
//! record count, one record, the manifest) or whose manifest has a path that is too long or
//! repeated is refused whole, never truncated or split.
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

/// The most bytes one record may have.
pub const MAX_RECORD_BYTES: u64 = 64 << 20;

/// The most bytes the manifest record may have.
pub const MAX_MANIFEST_BYTES: u64 = 4 << 20;

/// The most bytes one path in the manifest may have.
pub const MAX_PATH_BYTES: usize = 4096;

/// The limits [`pack_within`] enforces; a parameter so tests reach them with small inputs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub(crate) pack_bytes: u64,
    pub(crate) entries: usize,
    pub(crate) record_bytes: u64,
    pub(crate) manifest_bytes: u64,
    pub(crate) path_bytes: usize,
}

impl Limits {
    pub(crate) const DEFAULT: Self = Self {
        pack_bytes: MAX_PACK_BYTES,
        entries: MAX_ENTRIES,
        record_bytes: MAX_RECORD_BYTES,
        manifest_bytes: MAX_MANIFEST_BYTES,
        path_bytes: MAX_PATH_BYTES,
    };
}

/// One blob to put in the pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Blob {
    /// Bytes already in memory.
    Bytes(Vec<u8>),
    /// A file, read when the pack is built. It is read at most up to the pack limit,
    /// so a file larger than the pack is refused without being held whole.
    File(PathBuf),
}

/// Why a pack was not built. The size refusals all say `output-too-large`.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    /// The pack would exceed the byte limit.
    #[error("output-too-large: the pack would exceed the {limit}-byte limit of one segment")]
    TooLarge {
        /// The limit in bytes.
        limit: u64,
    },
    /// The pack would hold more records than the limit.
    #[error("output-too-large: the pack would hold more than {limit} distinct records")]
    TooManyEntries {
        /// The limit in records.
        limit: usize,
    },
    /// One blob is longer than a record may be.
    #[error("output-too-large: a blob is longer than the {limit}-byte limit of one record")]
    RecordTooLarge {
        /// The limit in bytes.
        limit: u64,
    },
    /// The encoded manifest is longer than the manifest record may be.
    #[error("output-too-large: the manifest is longer than the {limit}-byte limit")]
    ManifestTooLarge {
        /// The limit in bytes.
        limit: u64,
    },
    /// A path in the manifest is longer than a path may be.
    #[error("a manifest path is {len} bytes, over the {limit}-byte limit")]
    PathTooLong {
        /// The path's length in bytes.
        len: usize,
        /// The limit in bytes.
        limit: usize,
    },
    /// Two entries of the manifest name the same path.
    #[error("the manifest names the path {0:?} twice")]
    DuplicatePath(String),
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

/// Builds the pack: `manifest` at offset 0, then each non-empty one of `blobs` once (blobs with
/// equal content share one record, and an empty blob gets none).
///
/// # Errors
/// [`PackError`]: over a size limit, a path too long or repeated, a blob the manifest names
/// but `blobs` lacks, or an unreadable file.
pub fn pack(
    manifest: &Manifest,
    blobs: impl IntoIterator<Item = Blob>,
) -> Result<Vec<u8>, PackError> {
    pack_within(Limits::DEFAULT, manifest, blobs)
}

/// [`pack`] with the limits as parameters, so tests reach them with small inputs.
pub(crate) fn pack_within(
    limits: Limits,
    manifest: &Manifest,
    blobs: impl IntoIterator<Item = Blob>,
) -> Result<Vec<u8>, PackError> {
    check_paths(manifest, limits.path_bytes)?;
    let encoded = manifest.encode();
    if encoded.len() as u64 > limits.manifest_bytes {
        return Err(PackError::ManifestTooLarge {
            limit: limits.manifest_bytes,
        });
    }
    let mut writer = SegmentWriter::new(limits.pack_bytes);
    let mut held: BTreeSet<BlobId> = BTreeSet::new();
    let mut add = |writer: &mut SegmentWriter, bytes: &[u8]| -> Result<(), PackError> {
        if bytes.is_empty() {
            return Ok(());
        }
        if bytes.len() as u64 > limits.record_bytes {
            return Err(PackError::RecordTooLarge {
                limit: limits.record_bytes,
            });
        }
        let id = BlobId::of(bytes);
        if !held.contains(&id) {
            if held.len() >= limits.entries {
                return Err(PackError::TooManyEntries {
                    limit: limits.entries,
                });
            }
            // `Full` and `TooLarge` are both a size refusal.
            writer
                .push(bytes)
                .map_err(|_: WriteError| PackError::TooLarge {
                    limit: limits.pack_bytes,
                })?;
            held.insert(id);
        }
        Ok(())
    };
    add(&mut writer, &encoded)?;
    for blob in blobs {
        match blob {
            Blob::Bytes(bytes) => add(&mut writer, &bytes)?,
            Blob::File(path) => add(&mut writer, &read_capped(&path, limits.record_bytes)?)?,
        }
    }
    if let Some(missing) = manifest.blobs().find(|id| !held.contains(id)) {
        return Err(PackError::Missing(missing));
    }
    Ok(writer.finish())
}

/// Refuses a manifest with a path over `max` bytes or named twice, whichever list it is in.
fn check_paths(manifest: &Manifest, max: usize) -> Result<(), PackError> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for path in manifest.paths() {
        if path.len() > max {
            return Err(PackError::PathTooLong {
                len: path.len(),
                limit: max,
            });
        }
        if !seen.insert(path) {
            return Err(PackError::DuplicatePath(path.to_owned()));
        }
    }
    Ok(())
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
        return Err(PackError::RecordTooLarge { limit });
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
    /// A record has no bytes: the empty blob is implicit and is never a record.
    #[error("the pack holds a zero-size record")]
    EmptyRecord,
    /// The record at offset 0 is not a manifest.
    #[error(transparent)]
    Manifest(#[from] ParseError),
    /// The manifest names a blob that is not a record.
    #[error("the manifest names {0}, which is not a record of the pack")]
    Missing(BlobId),
}

/// Checks a pack as the frontend does: the footer and the record at offset 0, which
/// must parse as a manifest, and every non-empty blob the manifest names must be a record. A
/// zero-size record is refused; the empty digest, named without a record, is accepted.
///
/// # Errors
/// [`VerifyError`].
pub fn verify(bytes: &[u8]) -> Result<Manifest, VerifyError> {
    let reader = SegmentReader::open(bytes)?;
    let entries = reader.footer().entries();
    if entries.iter().any(|e| e.digest.size_bytes == 0) {
        return Err(VerifyError::EmptyRecord);
    }
    let first = entries
        .iter()
        .find(|e| e.offset == 0)
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
    use crate::manifest::OutputFile;
    use crate::manifest::tests::sample;
    use kbf_testkit::dir::TestDir;

    /// The blobs `sample()` names (it leaves stderr out as empty).
    fn blobs() -> Vec<Blob> {
        ["out", "file", "tree"]
            .iter()
            .map(|s| Blob::Bytes(s.as_bytes().to_vec()))
            .collect()
    }

    fn limits() -> Limits {
        Limits::DEFAULT
    }

    /// The manifest is the record at offset 0, and every named blob can be read back
    /// by its digest.
    #[test]
    fn manifest_sits_at_offset_zero() {
        let m = sample();
        let bytes = pack(&m, blobs()).unwrap();
        assert_eq!(&bytes[..m.encode().len()], m.encode().as_slice());
        let reader = SegmentReader::open(&bytes).unwrap();
        assert_eq!(reader.footer().entries().len(), 4);
        for (id, content) in [
            (m.stdout.unwrap(), "out"),
            (m.output_files[0].digest, "file"),
        ] {
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

    /// An empty blob is never a record, wherever it arrives, and does not displace the
    /// manifest from offset 0.
    #[test]
    fn empty_blob_gets_no_record() {
        let m = sample();
        let mut b = blobs();
        b.insert(0, Blob::Bytes(Vec::new()));
        b.push(Blob::Bytes(Vec::new()));
        let (_dir, path) = scratch("empty");
        std::fs::write(&path, b"").unwrap();
        b.push(Blob::File(path.clone()));
        let bytes = pack(&m, b).unwrap();
        std::fs::remove_file(&path).unwrap();
        let reader = SegmentReader::open(&bytes).unwrap();
        assert!(
            reader
                .footer()
                .entries()
                .iter()
                .all(|e| e.digest.size_bytes > 0)
        );
        assert_eq!(reader.footer().entries().len(), 4);
        assert_eq!(verify(&bytes).unwrap(), m);
    }

    /// An empty output file is listed with the empty digest and has no record; the pack
    /// verifies and the empty digest is not reported missing.
    #[test]
    fn empty_output_file_is_listed_without_a_record() {
        let mut m = sample();
        m.output_files.push(OutputFile {
            path: "empty".to_owned(),
            digest: BlobId::of(b""),
            is_executable: false,
        });
        let mut b = blobs();
        b.push(Blob::Bytes(Vec::new()));
        let bytes = pack(&m, b).unwrap();
        let reader = SegmentReader::open(&bytes).unwrap();
        assert_eq!(reader.footer().entries().len(), 4);
        let verified = verify(&bytes).unwrap();
        assert_eq!(
            verified.output_files[1].digest.to_string(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855/0"
        );
        // Not supplying the empty blob at all is fine too.
        assert_eq!(verify(&pack(&m, blobs()).unwrap()).unwrap(), m);
    }

    /// Equal content is one record, whether it arrives twice or from memory and a file.
    #[test]
    fn duplicate_blobs_share_a_record() {
        let m = sample();
        let (_dir, path) = scratch("dup");
        std::fs::write(&path, b"out").unwrap();
        let mut b = blobs();
        b.push(Blob::Bytes(b"out".to_vec()));
        b.push(Blob::File(path.clone()));
        let bytes = pack(&m, b).unwrap();
        std::fs::remove_file(&path).unwrap();
        let reader = SegmentReader::open(&bytes).unwrap();
        assert_eq!(reader.footer().entries().len(), 4);
    }

    #[test]
    fn file_blob_is_read() {
        let m = sample();
        let (_dir, path) = scratch("file");
        std::fs::write(&path, b"file").unwrap();
        let mut b = blobs();
        b.retain(|x| *x != Blob::Bytes(b"file".to_vec()));
        b.push(Blob::File(path.clone()));
        let bytes = pack(&m, b).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(verify(&bytes).unwrap(), m);
    }

    /// Catches: a file that opens but cannot be read (a directory named as a blob) not being
    /// reported as a read error naming it.
    #[test]
    fn unreadable_file_is_a_read_error() {
        let (_dir, sub) = scratch("a-directory");
        std::fs::create_dir(&sub).unwrap();
        let err = pack(&sample(), [Blob::File(sub)]).unwrap_err();
        assert!(matches!(err, PackError::Read { .. }));
        assert!(err.to_string().contains("cannot read"));
    }

    #[test]
    fn missing_file_is_a_read_error() {
        let err = pack(&sample(), [Blob::File(scratch("absent").1)]).unwrap_err();
        assert!(matches!(err, PackError::Read { .. }));
        assert!(err.to_string().contains("cannot read"));
    }

    #[test]
    fn unlisted_blob_is_refused() {
        let mut b = blobs();
        b.pop();
        let m = sample();
        let err = pack(&m, b).unwrap_err();
        assert_eq!(
            format!("{err:?}"),
            format!("{:?}", PackError::Missing(m.output_directories[0].tree))
        );
    }

    /// The byte limit names itself, for bytes and for a manifest alone that does not fit.
    #[test]
    fn byte_limit_is_refused() {
        let m = sample();
        let manifest_len = m.encode().len() as u64;
        // Room for the manifest, the small blobs and the footer, not for a 10 000-byte one.
        let pack_bytes = manifest_len + 100 + kbf_segments::layout::footer_len(6);
        let tight = Limits {
            pack_bytes,
            entries: 10,
            ..limits()
        };
        let mut b = blobs();
        b.push(Blob::Bytes(vec![7; 10_000]));
        let err = pack_within(tight, &m, b).unwrap_err();
        assert_eq!(
            format!("{err:?}"),
            format!("{:?}", PackError::TooLarge { limit: pack_bytes })
        );
        assert!(err.to_string().contains(&pack_bytes.to_string()));
        assert!(err.to_string().contains("output-too-large"));

        let (_dir, path) = scratch("big");
        std::fs::write(&path, vec![1u8; 2000]).unwrap();
        let err = pack_within(tight, &m, [Blob::File(path.clone())]).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(err, PackError::TooLarge { .. }));

        let tiny = Limits {
            pack_bytes: 10,
            ..tight
        };
        let err = pack_within(tiny, &m, blobs()).unwrap_err();
        assert!(matches!(err, PackError::TooLarge { limit: 10 }));

        // Many small blobs overflowing what is left after the first fit.
        let many = (0u32..200).map(|i| Blob::Bytes(format!("blob {i}").into_bytes()));
        let err = pack_within(
            Limits {
                entries: 1000,
                ..tight
            },
            &m,
            many,
        )
        .unwrap_err();
        assert!(matches!(err, PackError::TooLarge { .. }));
    }

    /// The record limit counts distinct records, manifest included, and a repeated blob
    /// does not count twice.
    #[test]
    fn record_limit_is_refused() {
        let m = sample();
        let four = Limits {
            entries: 4,
            ..limits()
        };
        let err = pack_within(four, &m, {
            let mut b = blobs();
            b.push(Blob::Bytes(b"one more".to_vec()));
            b
        })
        .unwrap_err();
        assert!(matches!(err, PackError::TooManyEntries { limit: 4 }));
        assert!(err.to_string().contains("output-too-large"));
        assert!(err.to_string().contains('4'));

        let mut again = blobs();
        again.extend(blobs());
        pack_within(four, &m, again).unwrap();
    }

    /// One record over the record limit is refused, from memory and from a file, and a
    /// record exactly at the limit is accepted.
    #[test]
    fn record_size_limit_is_refused() {
        let m = sample();
        // The manifest is a record too, so the limit under test is the manifest's own size.
        let len = m.encode().len();
        let limit = len as u64;
        let small = Limits {
            record_bytes: limit,
            ..limits()
        };
        let mut b = blobs();
        b.push(Blob::Bytes(vec![1; len]));
        pack_within(small, &m, b).unwrap();
        let mut b = blobs();
        b.push(Blob::Bytes(vec![1; len + 1]));
        let err = pack_within(small, &m, b).unwrap_err();
        assert_eq!(
            format!("{err:?}"),
            format!("{:?}", PackError::RecordTooLarge { limit })
        );
        assert!(err.to_string().contains("output-too-large"));

        let (_dir, path) = scratch("record");
        std::fs::write(&path, vec![1u8; len + 1]).unwrap();
        let err = pack_within(small, &m, [Blob::File(path.clone())]).unwrap_err();
        assert_eq!(
            format!("{err:?}"),
            format!("{:?}", PackError::RecordTooLarge { limit })
        );
        std::fs::write(&path, vec![1u8; len]).unwrap();
        let mut b = blobs();
        b.push(Blob::File(path.clone()));
        pack_within(small, &m, b).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(MAX_RECORD_BYTES, 64 * 1024 * 1024);
    }

    /// The manifest record has its own, smaller limit.
    #[test]
    fn manifest_size_limit_is_refused() {
        let m = sample();
        let len = m.encode().len() as u64;
        let at = Limits {
            manifest_bytes: len,
            ..limits()
        };
        pack_within(at, &m, blobs()).unwrap();
        let under = Limits {
            manifest_bytes: len - 1,
            ..limits()
        };
        let err = pack_within(under, &m, blobs()).unwrap_err();
        assert_eq!(
            format!("{err:?}"),
            format!("{:?}", PackError::ManifestTooLarge { limit: len - 1 })
        );
        assert!(err.to_string().contains("output-too-large"));
        assert_eq!(MAX_MANIFEST_BYTES, 4 * 1024 * 1024);
    }

    /// A path over the limit, or named twice in any list, is refused.
    #[test]
    fn paths_are_bounded_and_unique() {
        let mut m = sample();
        m.output_files[0].path = "p".repeat(MAX_PATH_BYTES);
        pack(&m, blobs()).unwrap();
        m.output_files[0].path.push('p');
        let err = pack(&m, blobs()).unwrap_err();
        assert_eq!(
            format!("{err:?}"),
            format!(
                "{:?}",
                PackError::PathTooLong {
                    len: 4097,
                    limit: 4096
                }
            )
        );
        assert!(err.to_string().contains("4097"));

        // Each list's path set to another list's path.
        let own = ["bin/a", "d", "l"];
        for list in 0..3 {
            for other in (0..3).filter(|o| *o != list) {
                let mut m = sample();
                let dup = own[other];
                match list {
                    0 => m.output_files[0].path = dup.to_owned(),
                    1 => m.output_directories[0].path = dup.to_owned(),
                    _ => m.output_symlinks[0].path = dup.to_owned(),
                }
                let err = pack(&m, blobs()).unwrap_err();
                assert_eq!(
                    format!("{err:?}"),
                    format!("{:?}", PackError::DuplicatePath(dup.to_owned()))
                );
                assert!(err.to_string().contains(dup));
            }
        }
    }

    #[test]
    fn verify_refuses_what_it_should() {
        // Not a segment.
        assert!(matches!(verify(b"junk"), Err(VerifyError::Segment(_))));
        let err = verify(b"junk").unwrap_err();
        assert!(err.to_string().contains("not a valid segment"));

        // A zero-size record is refused, even behind a valid manifest.
        let m = sample();
        let mut w = SegmentWriter::new(MAX_PACK_BYTES);
        w.push(&m.encode()).unwrap();
        w.push(b"").unwrap();
        let err = verify(&w.finish()).unwrap_err();
        assert!(matches!(err, VerifyError::EmptyRecord));
        assert!(err.to_string().contains("zero-size"));

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
        assert_eq!(
            format!("{err:?}"),
            format!("{:?}", VerifyError::Missing(m.stdout.unwrap()))
        );
        assert!(err.to_string().contains("not a record"));

        // A manifest that is not first is not found at offset 0.
        let mut w = SegmentWriter::new(MAX_PACK_BYTES);
        w.push(b"first").unwrap();
        w.push(&m.encode()).unwrap();
        assert!(matches!(verify(&w.finish()), Err(VerifyError::Manifest(_))));
    }

    /// A fresh directory, and the path `name` inside it (no file there yet).
    fn scratch(name: &str) -> (TestDir, PathBuf) {
        let dir = TestDir::under(std::env::temp_dir(), "kbf-task-pack");
        let path = dir.join(name);
        (dir, path)
    }
}
