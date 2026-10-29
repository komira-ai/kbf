//! Building a task's pack with the shared task pack types of `kbf-task-pack`, so the frontend
//! that ingests it reads exactly what this tool wrote.
//!
//! [`build`] refuses a manifest ingest would refuse ([`TaskManifest::validate`]), writes the
//! pack ([`write_pack`]: the manifest as the record at offset 0, then each non-empty blob once)
//! and reads it back through [`read_manifest`], which applies [`check_shape`] (the 128 MiB, 16,384
//! record, record and manifest caps, and contiguity). A refusal is never a truncated or split
//! pack: the task fails and nothing is stored.
//!
//! [`check_shape`]: kbf_task_pack::check_shape

use std::fmt;

use kbf_task_pack::{
    ManifestError, ShapeError, TaskManifest, TaskPackError, WritePackError, read_manifest,
    write_pack,
};

pub use kbf_task_pack::MAX_PACK_BYTES;

/// The detail of a refusal for passing a size cap: the pack, a record or the manifest is too big,
/// or there are too many records.
pub const OUTPUT_TOO_LARGE: &str = "output-too-large";

/// The detail of a refusal of the manifest for any other reason: a path, a target, a digest, a
/// number or the lease that ingest would refuse.
pub const INVALID_MANIFEST: &str = "invalid-manifest";

/// The detail of a refusal of the written pack for any other reason than a size cap.
pub const INVALID_PACK: &str = "invalid-pack";

/// Why a pack was not built: a short `detail` and the refusal's own text.
#[derive(Debug)]
pub struct PackError {
    /// [`OUTPUT_TOO_LARGE`], [`INVALID_MANIFEST`] or [`INVALID_PACK`].
    pub detail: &'static str,
    message: String,
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

/// Writes the pack of `manifest` and `blobs` and checks it as ingest does before it fetches a
/// record.
///
/// The pack is not checked against the manifest: that every digest the manifest names is among
/// `blobs` is the caller's to know, as it collected both.
///
/// # Errors
/// A [`PackError`] for a manifest [`TaskManifest::validate`] refuses, a blob or a total past a
/// size cap, or a pack [`read_manifest`] would not read back.
pub fn build(manifest: &TaskManifest, blobs: &[&[u8]]) -> Result<Vec<u8>, PackError> {
    manifest.validate()?;
    let pack = write_pack(manifest, blobs)?;
    read_manifest(&pack)?;
    Ok(pack)
}

#[cfg(test)]
#[path = "pack_tests.rs"]
mod tests;
