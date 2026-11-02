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
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};

use crate::digest::{BadDigest, Digest};
use crate::names::is_service_name;
use crate::trust::{TrustError, check_dir, read_trusted};

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
    pub fn parse(text: &str) -> Result<Approvals, ApprovalError> {
        let mut digests = BTreeSet::new();
        for (index, line) in text.lines().enumerate() {
            let content = line.split('#').next().unwrap_or_default();
            if let Some(word) = content.split_whitespace().next() {
                let digest = Digest::parse(word).map_err(|why| ApprovalError::Line {
                    line: index + 1,
                    why,
                })?;
                digests.insert(digest);
            }
        }
        Ok(Approvals(digests))
    }

    /// Reads the approval file. A file that is missing is an empty record.
    pub fn read(path: &Path, trusted_uid: u32) -> Result<Approvals, ApprovalError> {
        Approvals::parse(&String::from_utf8_lossy(&read_bytes(path, trusted_uid)?))
    }

    pub fn contains(&self, digest: &Digest) -> bool {
        self.0.contains(digest)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The bytes of the approval file, none if it is missing.
fn read_bytes(path: &Path, trusted_uid: u32) -> Result<Vec<u8>, ApprovalError> {
    match read_trusted(path, trusted_uid) {
        Ok(bytes) => Ok(bytes),
        Err(TrustError::Io(e)) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// Records the approval of `digest`, the declaration of `service`.
///
/// This is a root act: the process must run as `trusted_uid`, the account that owns the
/// approval file, because the file it writes is only as good as its owner. The existing file is
/// read first under the same rules as everyone else reads it, so an untrusted file is never
/// copied into a trusted one. The new file is written beside the old and renamed over it.
pub fn approve(
    path: &Path,
    trusted_uid: u32,
    effective_uid: u32,
    service: &str,
    digest: &Digest,
) -> Result<Approved, ApprovalError> {
    if !is_service_name(service) {
        return Err(ApprovalError::ServiceName(service.to_owned()));
    }
    if effective_uid != trusted_uid {
        return Err(ApprovalError::NotTrustedAccount {
            found: effective_uid,
            wanted: trusted_uid,
        });
    }
    let directory = path.parent().unwrap_or(Path::new("/"));
    check_dir(directory, trusted_uid)?;
    // One approval at a time: each reads the record and replaces it, and two at once would lose one.
    let mut lock_name = path.as_os_str().to_owned();
    lock_name.push(".lock");
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(PathBuf::from(lock_name))?;
    flock(&lock, FlockOperation::LockExclusive).map_err(io::Error::from)?;
    let bytes = read_bytes(path, trusted_uid)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if Approvals::parse(&text)?.contains(digest) {
        return Ok(Approved::Already);
    }
    // The new line starts on a line of its own.
    text.push_str(&"\n".repeat(usize::from(!text.is_empty() & !text.ends_with('\n'))));
    text.push_str(&format!("{digest} {service}\n"));
    replace(path, text.as_bytes())?;
    Ok(Approved::Recorded)
}

/// Writes `bytes` to a new file beside `path`, syncs it and renames it over `path`, then syncs
/// the directory.
fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".new");
    let temporary = PathBuf::from(temporary);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    let directory = path.parent().unwrap_or(Path::new("/"));
    std::fs::File::open(directory)?.sync_all()
}
