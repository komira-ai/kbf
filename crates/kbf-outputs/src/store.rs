//! Where outputs go: the [`Store`] trait a driver implements over its CAS client.

use std::future::Future;
use std::sync::Arc;

use kbf_proto::reapi::Digest;
use sha2::{Digest as _, Sha256};

/// A file of at most this many bytes is read whole and stored with [`Store::put`];
/// a larger one is hashed in chunks of this size and stored with
/// [`Store::put_file`].
pub const CHUNK_BYTES: usize = 1 << 20;

/// Why a store refused or failed a blob.
pub type StoreError = Box<dyn std::error::Error + Send + Sync>;

/// A content-addressed blob store, SHA-256 only, as [`crate::collect`] writes to it.
pub trait Store: Sync {
    /// Stores `bytes` and returns their digest.
    fn put(&self, bytes: Vec<u8>) -> impl Future<Output = Result<Digest, StoreError>> + Send;

    /// Stores the bytes of `file`, read from its current offset to its end, which
    /// hash to `digest`. A store streams them rather than holding them whole, and
    /// fails if they do not hash to `digest`.
    fn put_file(
        &self,
        file: std::fs::File,
        digest: Digest,
    ) -> impl Future<Output = Result<Digest, StoreError>> + Send;

    /// How many requests the store runs at once (the transfer engine's limit for the
    /// whole process): [`crate::collect`] queues no more than this many blobs ahead of
    /// it, so a walk holds that many open files, not the tree's.
    fn in_flight(&self) -> usize;

    /// Told the bytes of an output directory's Tree just before [`Store::put`] stores them, for
    /// a store that cannot read back what it wrote. The default keeps nothing.
    fn keep_tree(&self, _bytes: &[u8]) {}

    /// Where the bytes of a file larger than [`CHUNK_BYTES`] go while they are read and
    /// hashed, if this store takes them so: the file is then read once, and
    /// [`Store::put_file`] is not called for it. `None` (the default) is the store that is
    /// handed the open file after it was hashed.
    fn tee(&self) -> Option<Arc<dyn Tee>> {
        None
    }
}

/// A place that takes the bytes of a large file as they are read: [`Store::tee`]. One file is
/// open at a time (the walk reads one file at a time).
pub trait Tee: Send + Sync {
    /// Starts taking one file's bytes.
    ///
    /// # Errors
    /// The tee cannot take a file.
    fn open(&self) -> std::io::Result<Box<dyn TeeFile>>;
}

/// One file's bytes, as the tee takes them.
pub trait TeeFile: Send {
    /// Takes the next bytes of the file.
    ///
    /// # Errors
    /// The tee refuses the bytes.
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()>;

    /// Ends the file: every byte was given, and they hash to `digest`. A tee that was dropped
    /// without this holds a file that failed.
    ///
    /// # Errors
    /// The tee refuses the end.
    fn end(self: Box<Self>, digest: &Digest) -> std::io::Result<()>;
}

/// The SHA-256 digest of `bytes`.
#[must_use]
pub fn digest_of(bytes: &[u8]) -> Digest {
    Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        // A slice is never longer than `isize::MAX`.
        size_bytes: bytes.len() as i64,
    }
}
