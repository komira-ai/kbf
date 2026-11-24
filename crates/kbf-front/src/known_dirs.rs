//! What a frontend knows of Directories by their content, shared by the input walks of its
//! Executes and the CAS writes of its clients: [`KnownDirs`].
//!
//! A client uploads the Directories of an input tree, then sends the Execute that names it.
//! The walk of that Execute (`input_walk`) wants each Directory decoded; when the frontend
//! wrote them into its packs moments before, reading them back is a round trip per level of
//! the tree for bytes it already had. So the writes into packs ([`crate::Packs`]) give each
//! small blob they stored to [`KnownDirs::uploaded`], and one that decodes as a Directory is
//! put where a walk that read it would have put it: in the content cache of decoded
//! Directories ([`DirCache`]) and, with its names, in the run preparation's cache
//! ([`DirectoryCache`]).
//!
//! Nothing here says a blob is held. A walk asks the store about every Directory it takes
//! from the content cache (FindMissing, with the files), so an uploaded Directory the store
//! lost since is reported missing exactly as before. And a digest names its content: a blob
//! kept here as a Directory is the bytes a walk that reads that digest gets, decoded the same
//! way, so whether a blob was kept changes no walk's answer, only whether it reads. A blob
//! that is not a Directory but decodes as one (prost skips what it does not know) is kept as
//! harmlessly: it is only consulted if a client names its digest as a Directory, and then the
//! walk would have read and decoded the very same bytes. What such blobs can cost is room,
//! bounded by the cache's bytes and by [`UPLOADED_DIRECTORY_BYTES`] a blob.

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use kbf_types::Digest;

use crate::cache::is_empty_blob;
use crate::dir_cache::{DirCache, Lookup};
use crate::input_tree::decode;
use crate::prepare_run::{DirectoryCache, KeptDirectory};

/// The largest uploaded blob looked at as a Directory: the largest the pack index keeps a
/// Directory row for (`kbf_pack_index::directory::MAX_DIRECTORY_BYTES`, 4 MiB), some
/// 30 thousand entries. A larger Directory is read by the walk that needs it, as before.
pub(crate) const UPLOADED_DIRECTORY_BYTES: usize = kbf_pack_index::directory::MAX_DIRECTORY_BYTES;

/// The first byte of a Directory as clients encode it (fields in order, none empty first): the
/// key of its `files` (1), `directories` (2), `symlinks` (3) or `node_properties` (5), each a
/// length-delimited field. A blob starting with anything else is not decoded at all.
const DIRECTORY_KEYS: [u8; 4] = [0x0a, 0x12, 0x1a, 0x2a];

/// The decoded Directories a frontend knows by content, and where a walk also keeps them with
/// their names for the run preparation.
pub(crate) struct KnownDirs {
    /// What the Directories decode to, whatever became of their blobs.
    pub(crate) dirs: DirCache,
    /// Where each Directory a walk reads, or a client uploads, is also kept with its names;
    /// none until a preparer gives it ([`KnownDirs::keep_into`]).
    keep: Mutex<Option<Arc<DirectoryCache>>>,
}

impl Default for KnownDirs {
    fn default() -> Self {
        Self::new(DirCache::default())
    }
}

impl KnownDirs {
    pub(crate) fn new(dirs: DirCache) -> Self {
        Self {
            dirs,
            keep: Mutex::new(None),
        }
    }

    /// From now on, every Directory a walk reads or a client uploads is also kept in `cache`.
    /// A later cache replaces it.
    pub(crate) fn keep_into(&self, cache: Arc<DirectoryCache>) {
        *self.keep.lock().unwrap_or_else(PoisonError::into_inner) = Some(cache);
    }

    pub(crate) fn kept(&self) -> Option<Arc<DirectoryCache>> {
        // Set or read in one statement: nothing can panic with the lock held.
        self.keep
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// A client uploaded `bytes` as `digest`, and the store holds them now. If they may be a
    /// Directory ([`may_be_directory`]), nobody has them cached or is reading them, and they
    /// decode as one, they are cached as a walk that read them would have, and `true` is
    /// returned; otherwise nothing changes.
    pub(crate) fn uploaded(&self, digest: &Digest, bytes: &Bytes) -> bool {
        if !may_be_directory(digest, bytes) {
            return false;
        }
        // A Directory cached already (a hit is a use) or being read is left as it is: the read
        // ends with these same bytes.
        let Lookup::Lead(lead) = self.dirs.lookup(digest) else {
            return false;
        };
        // Dropped without finishing, the lead ends its flight and caches nothing.
        let Ok(decoded) = decode(digest, bytes) else {
            return false;
        };
        lead.finish(Arc::new(decoded));
        if let Some(keep) = self.kept() {
            keep_named(&keep, *digest, bytes.clone());
        }
        true
    }
}

/// Whether an uploaded blob is worth decoding as a Directory: no larger than
/// [`UPLOADED_DIRECTORY_BYTES`], not the empty blob (the empty Directory names nothing, and the
/// packs serve it without asking the index or the store), and starting with a key a Directory starts with ([`DIRECTORY_KEYS`]).
pub(crate) fn may_be_directory(digest: &Digest, bytes: &[u8]) -> bool {
    bytes.len() <= UPLOADED_DIRECTORY_BYTES
        && !is_empty_blob(digest)
        && bytes
            .first()
            .is_some_and(|key| DIRECTORY_KEYS.contains(key))
}

/// Keeps `dir`, whose bytes are `blob`, in `keep` with its names, for the preparation's walk of
/// the same tree: not the empty Directory, which that walk never caches.
pub(crate) fn keep_named(keep: &DirectoryCache, dir: Digest, blob: Bytes) {
    if is_empty_blob(&dir) {
        return;
    }
    // `decode` has checked every digest the blob names, so the parse finds no fault; a
    // Directory parsed with one would be partial, and is not kept.
    let parsed = KeptDirectory::parse(&dir, blob).ok();
    let whole = parsed.filter(|parsed| parsed.fault.is_none());
    whole
        .into_iter()
        .for_each(|parsed| keep.remember(dir, &parsed.kept));
}

#[cfg(test)]
#[path = "known_dirs_tests.rs"]
mod tests;
