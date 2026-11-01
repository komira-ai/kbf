//! Why a pack, a read or a cache call failed.

use std::path::PathBuf;

use kbf_objstore::{KeyError, ObjectStoreError};
use kbf_pack_index::{BlobDigest, PackIndexError};

/// Why writing blobs into packs failed.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    /// The store does not claim create-only writes, which the write rule needs: a pack key
    /// must never be replaced.
    #[error("the object store does not claim create-only writes")]
    NotCreateOnly,
    /// The store took a second write of a key it holds: it claims create-only writes and
    /// does not refuse to replace.
    #[error("the object store took a second write of a key it holds")]
    CreateOnlySelfTest,
    /// A name of the key scheme holds a `/`, which would put the pack outside its writer's
    /// prefix.
    #[error("the name {0:?} of the pack key scheme holds a `/`")]
    Name(String),
    /// The key scheme does not make object keys.
    #[error("the pack key scheme: {0}")]
    Key(#[from] KeyError),
    /// The bytes given for a blob do not hash to the digest given with them.
    #[error("the bytes given for blob {claimed:?} hash to another digest")]
    DigestMismatch {
        /// The digest the caller named.
        claimed: BlobDigest,
    },
    /// The store failed a call outside the write of a pack.
    #[error("object store: {0}")]
    Store(#[from] ObjectStoreError),
    /// Every attempt to store the pack failed, and nothing was recorded.
    #[error("pack {key} was not stored after {attempts} attempts: {source}")]
    Put {
        /// The key of the last attempt.
        key: String,
        /// How many attempts were made.
        attempts: u32,
        /// Why the last attempt failed.
        #[source]
        source: ObjectStoreError,
    },
    /// The pack is stored but the index did not record it, in any attempt. No row points at
    /// it: it is an unreferenced object that garbage collection finds and deletes. The blobs
    /// of the pack are still missing from the index, so packing them again writes a new pack.
    #[error("pack {key} is stored but not recorded: {source}")]
    Record {
        /// The key of the stored pack.
        key: String,
        /// Why the index failed.
        #[source]
        source: PackIndexError,
    },
    /// The index failed a lookup.
    #[error(transparent)]
    Index(#[from] PackIndexError),
}

/// Why reading blobs failed. A blob that is missing, lost or damaged is not an error of the
/// call: it is the blob's [`Fetch`](crate::Fetch).
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The index names a pack by a string that is not an object key.
    #[error("the pack key of an index row: {0}")]
    Key(#[from] KeyError),
    /// The index failed.
    #[error(transparent)]
    Index(#[from] PackIndexError),
    /// The store failed a read for a reason other than a missing object or range.
    #[error("reading pack {pack}: {source}")]
    Store {
        /// The key of the pack.
        pack: String,
        /// Why the read failed.
        #[source]
        source: ObjectStoreError,
    },
}

/// Why a call of the read cache failed.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// A file operation failed.
    #[error("read cache: cannot {op} {}: {source}", .path.display())]
    Io {
        /// What was being done.
        op: &'static str,
        /// The file or directory.
        path: PathBuf,
        /// What the system said.
        #[source]
        source: std::io::Error,
    },
    /// The bytes offered are not the blob they were offered as.
    #[error("read cache: the bytes offered do not hash to {digest:?}")]
    Mismatch {
        /// The digest the bytes were offered under.
        digest: BlobDigest,
    },
}
