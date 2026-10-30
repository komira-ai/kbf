//! Building a task's packs with the shared task pack types of `kbf-task-pack`, so the frontend
//! that ingests them reads exactly what this tool wrote.
//!
//! [`build`] builds pack 0. It refuses a manifest ingest would refuse
//! ([`TaskManifest::validate_with`], with the large cap [`MAX_LARGE_BYTES`]), writes the pack
//! ([`write_pack_with`]: the manifest as the record at offset 0, then each non-empty blob once)
//! and reads it back through [`read_manifest_with`], which applies [`check_shape`] (the 128 MiB,
//! 16,384 record, record and manifest caps, and contiguity). A run that stores one pack and no
//! large object has a version 1 manifest, and its pack is the bytes the v1 tool wrote.
//!
//! [`build_extra`] builds one of packs 1.. which hold records only: no manifest, the same pack
//! and record caps. A refusal is never a truncated or split pack: the task fails and nothing is
//! stored.
//!
//! [`check_shape`]: kbf_task_pack::check_shape

use std::collections::BTreeSet;
use std::fmt;

use kbf_segments::SegmentWriter;
use kbf_task_pack::{
    MAX_PACK_RECORDS, MAX_RECORD_BYTES, ManifestError, ShapeError, TaskManifest, TaskPackError,
    WritePackError, read_manifest_with, write_pack_with,
};

use crate::plan::PlanError;
pub use crate::put::MAX_LARGE_BYTES;
pub use kbf_task_pack::MAX_PACK_BYTES;

/// The detail of a refusal for passing a size cap: the pack, a record or the manifest is too big,
/// or there are too many records.
pub const OUTPUT_TOO_LARGE: &str = "output-too-large";

/// The detail of a refusal because an output file no longer has the bytes it was hashed with.
pub const OUTPUT_CHANGED: &str = "output-changed";

/// The detail of a refusal of the manifest for any other reason: a path, a target, a digest, a
/// number or the lease that ingest would refuse.
pub const INVALID_MANIFEST: &str = "invalid-manifest";

/// The detail of a refusal of the written pack for any other reason than a size cap.
pub const INVALID_PACK: &str = "invalid-pack";

/// Why a pack was not built: a short `detail` and the refusal's own text.
#[derive(Debug)]
pub struct PackError {
    /// [`OUTPUT_TOO_LARGE`], [`OUTPUT_CHANGED`], [`INVALID_MANIFEST`] or [`INVALID_PACK`].
    pub detail: &'static str,
    message: String,
}

impl PackError {
    /// The refusal of a blob whose bytes are no longer those of `digest`.
    #[must_use]
    pub fn changed(digest: &kbf_types::Digest) -> Self {
        Self {
            detail: OUTPUT_CHANGED,
            message: format!("the output {digest} changed after it was hashed"),
        }
    }
}

impl fmt::Display for PackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.detail, self.message)
    }
}

impl std::error::Error for PackError {}

impl From<ManifestError> for PackError {
    fn from(e: ManifestError) -> Self {
        let detail = match e {
            ManifestError::TooLarge { .. } => OUTPUT_TOO_LARGE,
            _ => INVALID_MANIFEST,
        };
        Self {
            detail,
            message: e.to_string(),
        }
    }
}

impl From<WritePackError> for PackError {
    fn from(e: WritePackError) -> Self {
        match e {
            WritePackError::Manifest(m) => m.into(),
            // A record or the sum of the records passed a cap.
            WritePackError::RecordTooLarge { .. } | WritePackError::Segment(_) => Self {
                detail: OUTPUT_TOO_LARGE,
                message: e.to_string(),
            },
        }
    }
}

impl From<PlanError> for PackError {
    fn from(e: PlanError) -> Self {
        let detail = match e {
            PlanError::TooLarge(_) => OUTPUT_TOO_LARGE,
            PlanError::SizeMismatch { .. } => INVALID_MANIFEST,
        };
        Self {
            detail,
            message: e.to_string(),
        }
    }
}

impl From<TaskPackError> for PackError {
    fn from(e: TaskPackError) -> Self {
        let detail = match &e {
            TaskPackError::Shape(
                ShapeError::TooLarge { .. }
                | ShapeError::TooManyRecords { .. }
                | ShapeError::RecordTooLarge { .. }
                | ShapeError::ManifestTooLarge { .. },
            ) => OUTPUT_TOO_LARGE,
            _ => INVALID_PACK,
        };
        Self {
            detail,
            message: e.to_string(),
        }
    }
}

/// Writes pack 0, the pack of `manifest` and `blobs`, and checks it as ingest does before it
/// fetches a record.
///
/// The pack is not checked against the manifest: that every digest the manifest names is in
/// `blobs`, in another pack or in a large object is the caller's to know, as it collected them.
///
/// # Errors
/// A [`PackError`] for a manifest [`TaskManifest::validate_with`] refuses, a blob or a total past
/// a size cap, or a pack [`read_manifest_with`] would not read back.
pub fn build(manifest: &TaskManifest, blobs: &[&[u8]]) -> Result<Vec<u8>, PackError> {
    manifest.validate_with(MAX_LARGE_BYTES)?;
    let pack = write_pack_with(manifest, blobs, MAX_LARGE_BYTES)?;
    read_manifest_with(&pack, MAX_LARGE_BYTES)?;
    Ok(pack)
}

/// Writes one of packs 1..: `blobs` as records, each once (an empty blob is skipped), with no
/// manifest, within the caps.
///
/// # Errors
/// A [`PackError`] with detail [`OUTPUT_TOO_LARGE`] for a blob over the record cap or records past
/// the pack's caps.
pub fn build_extra(blobs: &[&[u8]]) -> Result<Vec<u8>, PackError> {
    let too_large = |message: String| PackError {
        detail: OUTPUT_TOO_LARGE,
        message,
    };
    let mut writer = SegmentWriter::new(MAX_PACK_BYTES);
    let mut records = BTreeSet::new();
    for blob in blobs.iter().filter(|b| !b.is_empty()) {
        if blob.len() as u64 > MAX_RECORD_BYTES {
            return Err(too_large(format!(
                "a blob of {} bytes is more than the {MAX_RECORD_BYTES} a record may hold",
                blob.len()
            )));
        }
        records.insert(writer.push(blob).map_err(|e| too_large(e.to_string()))?);
    }
    if records.len() > MAX_PACK_RECORDS {
        return Err(too_large(format!(
            "the pack holds {} records, more than {MAX_PACK_RECORDS}",
            records.len()
        )));
    }
    Ok(writer.finish())
}

#[cfg(test)]
#[path = "pack_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "pack_extra_tests.rs"]
mod extra_tests;
