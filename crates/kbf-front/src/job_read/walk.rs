//! The scope of an Action, worked out by walking its input tree, and the byte budget that the
//! walk and the reads of blobs share.
//!
//! The walk reads the Action and then the Directories under its input root a level at a time,
//! and keeps only the digests they name: a Directory is dropped as soon as it is read, and the
//! bound [`super::Limits::scope_digests`] is checked as each Directory is folded in, so the
//! walk holds the bound, the sixteen Directories being read (each of at most
//! [`MAX_SCOPE_BLOB_BYTES`]) and the digests of one more. Each read takes its blob's size from the budget first, and a blob
//! over [`MAX_SCOPE_BLOB_BYTES`] is refused before it is charged or read.

use std::collections::HashSet;
use std::sync::Arc;

use futures::stream::{self, StreamExt as _, TryStreamExt as _};
use kbf_proto::reapi;
use kbf_storage::StorageError;
use kbf_types::Digest;
use prost::Message as _;
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};

use super::BlobSource;
use crate::cache::is_empty_blob;
use crate::grants::Scope;
use crate::wire;

/// The largest Action or Directory blob the scope walk reads: a tenant writes the sizes in
/// the Directories it is asked to walk, and a read of one of 10^12 bytes would take the whole
/// budget.
pub const MAX_SCOPE_BLOB_BYTES: u64 = 4 << 20;

/// The Directory blobs of one level of an input tree read at once.
const DIRECTORIES_IN_FLIGHT: usize = 16;

/// Why an Action has no scope.
pub(super) enum ScopeError {
    /// Its blobs are not in the store, are not what they must be, or name more than
    /// [`super::Limits::scope_digests`].
    Unusable,
    /// The store could not answer.
    Store,
}

impl From<StorageError> for ScopeError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::NotFound(_) => Self::Unusable,
            _ => Self::Store,
        }
    }
}

/// The bytes of reads open at once ([`super::Limits::read_budget_bytes`]), in units of 1 KiB.
pub(super) struct Budget {
    room: Arc<Semaphore>,
    units: u32,
}

/// The size of a unit of the budget.
const UNIT: u64 = 1024;

impl Budget {
    pub(super) fn new(bytes: u64) -> Self {
        let units = u32::try_from((bytes / UNIT).max(1)).unwrap_or(u32::MAX);
        Self {
            room: Arc::new(Semaphore::new(units as usize)),
            units,
        }
    }

    /// Room for `bytes` (rounded up to a unit, and at most the whole budget), waited for as
    /// long as it takes; given back when the permit is dropped.
    pub(super) async fn take(&self, bytes: u64) -> Result<OwnedSemaphorePermit, AcquireError> {
        let want = u32::try_from(bytes.div_ceil(UNIT).max(1)).unwrap_or(u32::MAX);
        Arc::clone(&self.room)
            .acquire_many_owned(want.min(self.units))
            .await
    }
}

/// The Action or Directory blob `digest` names is not over [`MAX_SCOPE_BLOB_BYTES`].
fn within_cap(digest: &Digest) -> Result<(), ScopeError> {
    if digest.size_bytes > MAX_SCOPE_BLOB_BYTES {
        return Err(ScopeError::Unusable);
    }
    Ok(())
}

/// The digests of the files and of the subdirectories one Directory names.
type Entries = (Vec<Digest>, Vec<Digest>);

/// The scope of `action`: the Action, its Command, and the Directory blobs and files of the
/// tree under its input root, read from `source` a level at a time.
pub(super) async fn walk(
    source: &dyn BlobSource,
    budget: &Budget,
    action: Digest,
    most: usize,
) -> Result<Scope, ScopeError> {
    within_cap(&action)?;
    let bytes = {
        let _room = budget.take(action.size_bytes).await;
        source.read_blob(&action).await?
    };
    let decoded = reapi::Action::decode(bytes).map_err(|_| ScopeError::Unusable)?;
    let command = proto(decoded.command_digest.as_ref())?;
    let root = proto(decoded.input_root_digest.as_ref())?;
    let mut seen = HashSet::from([root]);
    let mut level = vec![root];
    while !level.is_empty() {
        let mut next = Vec::new();
        let mut entries = stream::iter(level)
            .map(|digest| directory(source, budget, digest))
            .buffer_unordered(DIRECTORIES_IN_FLIGHT);
        // Each Directory is folded in as it arrives and the bound checked at once, so that what
        // the walk holds is the bound, the Directories being read and one more.
        while let Some((files, directories)) = entries.try_next().await? {
            seen.extend(files);
            for digest in directories {
                if seen.insert(digest) {
                    next.push(digest);
                }
            }
            if seen.len() > most {
                return Err(ScopeError::Unusable);
            }
        }
        level = next;
    }
    Ok(Scope::new(action, command, seen))
}

/// The entries of the Directory blob `digest`; the empty blob is the empty Directory. The
/// Directory itself is dropped here, so only its digests outlive the read.
async fn directory(
    source: &dyn BlobSource,
    budget: &Budget,
    digest: Digest,
) -> Result<Entries, ScopeError> {
    if is_empty_blob(&digest) {
        return Ok((Vec::new(), Vec::new()));
    }
    within_cap(&digest)?;
    let bytes = {
        let _room = budget.take(digest.size_bytes).await;
        source.read_blob(&digest).await?
    };
    let directory = reapi::Directory::decode(bytes).map_err(|_| ScopeError::Unusable)?;
    Ok((files_of(&directory)?, directories_of(&directory)?))
}

fn files_of(directory: &reapi::Directory) -> Result<Vec<Digest>, ScopeError> {
    directory
        .files
        .iter()
        .map(|file| proto(file.digest.as_ref()))
        .collect()
}

fn directories_of(directory: &reapi::Directory) -> Result<Vec<Digest>, ScopeError> {
    directory
        .directories
        .iter()
        .map(|child| proto(child.digest.as_ref()))
        .collect()
}

fn proto(digest: Option<&reapi::Digest>) -> Result<Digest, ScopeError> {
    wire::digest(digest).map_err(|_| ScopeError::Unusable)
}
