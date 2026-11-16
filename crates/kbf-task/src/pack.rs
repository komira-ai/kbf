//! Why the run's object was refused: the manifest or the object is one the frontend that ingests
//! it would refuse.
//!
//! A [`PackError`] carries a short `detail` (the words the failure line starts with) and the
//! refusal's own text. The large cap [`MAX_LARGE_BYTES`] is the one the manifest is validated
//! with: the same rule the ingest applies.

use std::fmt;

use kbf_task_pack::ManifestError;

/// The most bytes of one blob the manifest may name, the large-object cap the ingest holds a
/// blob to: 512 MiB.
pub const MAX_LARGE_BYTES: u64 = 512 << 20;

/// The detail of a refusal for passing a size cap: the manifest is too big.
pub const OUTPUT_TOO_LARGE: &str = "output-too-large";

/// The detail of a refusal of the manifest for any other reason: a path, a target, a digest, a
/// number or the lease that ingest would refuse.
pub const INVALID_MANIFEST: &str = "invalid-manifest";

/// The detail of a refusal of the written object for any other reason than a size cap.
pub const INVALID_PACK: &str = "invalid-pack";

/// Why the object was not written: a short `detail` and the refusal's own text.
#[derive(Debug)]
pub struct PackError {
    /// [`OUTPUT_TOO_LARGE`], [`INVALID_MANIFEST`] or [`INVALID_PACK`].
    pub detail: &'static str,
    message: String,
}

impl PackError {
    /// The refusal of a manifest that names a blob the object does not hold.
    #[must_use]
    pub fn missing(digest: &kbf_types::Digest) -> Self {
        Self {
            detail: INVALID_PACK,
            message: format!("the manifest names {digest}, which the object does not hold"),
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

#[cfg(test)]
#[path = "pack_tests.rs"]
mod tests;
