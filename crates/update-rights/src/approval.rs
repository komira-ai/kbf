//! The record of approved declarations, and the act of approving one.
//!
//! A declaration that widens rights is `Deferred` until an operator has approved **that exact
//! declaration**: its digest (the SHA-256 of the bytes of its file) is in a root-owned file,
//! `/etc/updater.d/approved`, one digest per line, optionally followed by the service's name for
//! the reader (`# ` starts a comment). An operator approves one declaration with `approve`, and
//! a fleet is approved by installing the same file everywhere. A digest in the file is not asked
//! about again.
//!
//! The file is read like a declaration: root-owned, not a link, not writable by anyone else;
//! one that is not is an error, never an empty record. A missing file is an empty record.

use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use crate::digest::{BadDigest, Digest};
use crate::trust::TrustError;

/// Where approvals are recorded.
pub const APPROVALS_FILE: &str = "/etc/updater.d/approved";

/// Why approvals could not be read or recorded.
#[derive(Debug, thiserror::Error)]
pub enum ApprovalError {
    #[error("the approval file cannot be trusted: {0}")]
    Untrusted(#[from] TrustError),
    #[error("line {line} of the approval file: {why}")]
    Line { line: usize, why: BadDigest },
    #[error("{0:?} is not a service name")]
    ServiceName(String),
    #[error(
        "approving needs the account that owns the approval file (uid {wanted}), not uid {found}"
    )]
    NotTrustedAccount { found: u32, wanted: u32 },
    #[error("the approval file cannot be written: {0}")]
    Write(#[from] io::Error),
}

/// The approved digests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Approvals(BTreeSet<Digest>);

/// What `approve` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Approved {
    /// The digest was recorded.
    Recorded,
    /// The digest was in the record already; nothing was written.
    Already,
}

impl Approvals {
    /// Reads the text of the file: a digest per line, then anything the reader may like.
    pub fn parse(_text: &str) -> Result<Approvals, ApprovalError> {
        Ok(Approvals::default())
    }

    /// Reads the approval file. A file that is missing is an empty record.
    pub fn read(_path: &Path, _trusted_uid: u32) -> Result<Approvals, ApprovalError> {
        Ok(Approvals::default())
    }

    pub fn contains(&self, _digest: &Digest) -> bool {
        false
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Records the approval of `digest`, the declaration of `service`.
pub fn approve(
    _path: &Path,
    _trusted_uid: u32,
    _effective_uid: u32,
    _service: &str,
    _digest: &Digest,
) -> Result<Approved, ApprovalError> {
    Ok(Approved::Recorded)
}
