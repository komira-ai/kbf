//! The writes of a cache with `write_through` on: each goes to the upstream, and none is kept.
//!
//! A write is acknowledged only after the upstream has it, so nothing is only local and there is
//! nothing to recover. The cache keeps no copy of what it writes: the first read of an object
//! fills it, as for any other.
//!
//! - **Create-only put** ([`put_through`](Inner::put_through)): one HEAD of the key goes to the
//!   upstream before the body is read. An object that is there is
//!   [`AlreadyExists`](Error::AlreadyExists) at once, with no byte of the body read and no put
//!   sent (the S3 backend would otherwise upload all the parts of a large body and learn it at the
//!   completion). The HEAD only saves work: the upstream's create-only put is the one arbiter of
//!   two writers of one key, and its `AlreadyExists` is passed on as it is. An upstream that
//!   cannot answer the HEAD is the error of the put.
//! - **Multipart** (create, part, complete, abort): passed to the upstream as they are. A
//!   completion that succeeds drops the cache's entry for the key, if it has one.
//! - **Delete** ([`delete_through`](Inner::delete_through)): the entry is dropped, then the
//!   upstream deletes. A fill that is running for the key does not keep the object it read (see
//!   [`forget`](Inner::forget)).
//! - **A put that replaces on a tag** (`put_if_match`) is not done, as on every front.
//!
//! The keys under `_journal/` and `_tiering/` belong to a storage node and are refused with
//! [`Error::Reserved`], as a storage node does.

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;

use crate::journal::is_reserved;
use crate::{Error, PartBody};

use super::{Inner, READ_ONLY, locked};

impl Inner {
    /// Whether the cache takes writes of `key`: it was told to, and the key is not one a storage
    /// node keeps for itself.
    fn writable(&self, key: &str) -> Result<(), Error> {
        if !self.config.write_through {
            return Err(Error::Unsupported(READ_ONLY.to_owned()));
        }
        if is_reserved(key) {
            return Err(Error::Reserved(key.to_owned()));
        }
        Ok(())
    }

    /// A create-only put of a body that arrives as chunks.
    pub(super) async fn put_through(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.writable(key)?;
        match self.upstream.head(key).await {
            Ok(_) => return Err(Error::AlreadyExists(key.to_owned())),
            Err(Error::NotFound(_)) => {}
            Err(other) => return Err(other),
        }
        self.upstream.put_stream(key, len, body).await?;
        // The key was not there, so an entry for it is of an object that was deleted behind the
        // cache's back.
        self.forget(key);
        Ok(())
    }

    /// A create-only put of a body held whole: the streamed put of a body of one chunk.
    pub(super) async fn put_whole(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let len = body.len() as u64;
        let once: PartBody<'_> = stream::once(async move { Ok(body) }).boxed();
        self.put_through(key, Some(len), once).await
    }

    /// Deletes the object: the entry first, then the upstream's. A read that fills the key
    /// between the two reads the object before the upstream deletes it, so the entry is dropped
    /// once more when the upstream has deleted it, and any fill still running is told not to
    /// keep what it read.
    pub(super) async fn delete_through(&self, key: &str) -> Result<(), Error> {
        self.writable(key)?;
        self.forget(key);
        let deleted = self.upstream.delete(key).await;
        // Also when the upstream answers with an error: a timeout, or a 5xx after the commit,
        // may have deleted the object, and a fill that began in between must not keep it. An
        // object that is still there is read in again by the next miss.
        self.forget(key);
        deleted
    }

    /// Starts a multipart upload of `key` at the upstream.
    pub(super) async fn create_through(&self, key: &str) -> Result<String, Error> {
        self.writable(key)?;
        self.upstream.create_upload(key).await
    }

    /// Sends a part to the upstream.
    pub(super) async fn part_through(
        &self,
        key: &str,
        id: &str,
        number: u32,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.writable(key)?;
        let sent = self
            .upstream
            .upload_part_with_len(key, id, number, len, body);
        sent.await
    }

    /// Completes the upload at the upstream, create-only; when the object is made, the entry for
    /// the key (of an object that was deleted behind the cache's back) is dropped.
    pub(super) async fn complete_through(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        self.writable(key)?;
        let tag = self.upstream.complete_upload(key, id, parts).await?;
        self.forget(key);
        Ok(tag)
    }

    /// Aborts the upload at the upstream.
    pub(super) async fn abort_through(&self, key: &str, id: &str) -> Result<(), Error> {
        self.writable(key)?;
        self.upstream.abort_upload(key, id).await
    }

    /// Drops the entry for `key`, and marks the fill that is running for it, if there is one, so
    /// that it does not put what it read in the cache.
    ///
    /// The mark is made before the entry is dropped. A fill puts its object in the index and then
    /// looks for the mark (see `fetch`), so whichever comes first, either the fill sees the mark
    /// and removes its own entry, or the entry is in the index when it is dropped here.
    pub(super) fn forget(&self, key: &str) {
        let Some(name) = self.cacheable(key) else {
            return;
        };
        {
            let flights = locked(&self.flights);
            if flights.contains_key(&name.hex) {
                locked(&self.invalidated).insert(name.hex.clone());
            }
        }
        self.drop_entry(&name.hex);
    }

    /// Whether the fill of `hex` was marked by [`forget`](Self::forget).
    pub(super) fn is_invalidated(&self, hex: &str) -> bool {
        locked(&self.invalidated).contains(hex)
    }
}
