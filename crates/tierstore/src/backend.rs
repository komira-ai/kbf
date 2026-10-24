//! The minimal API of an S3-compatible store, as this crate uses it.

use async_trait::async_trait;
use bytes::Bytes;

use crate::Error;

/// A byte range to read: `len` bytes from `offset`.
///
/// A read returns the bytes of `[offset, offset + len)` that exist, so a range running past
/// the end of the object is cut at the end, and a range of length zero gives no bytes. An
/// `offset` beyond the end of the object is an [`Error::Other`]; an `offset` at the end
/// gives no bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteRange {
    offset: u64,
    len: u64,
}

impl ByteRange {
    /// The range `[offset, offset + len)`.
    #[must_use]
    pub fn new(offset: u64, len: u64) -> Self {
        Self { offset, len }
    }

    /// The first byte.
    #[must_use]
    pub fn offset(self) -> u64 {
        self.offset
    }

    /// The number of bytes asked for.
    #[must_use]
    pub fn size(self) -> u64 {
        self.len
    }

    /// The bytes of `body` this range covers.
    ///
    /// # Errors
    /// [`Error::Other`] if `offset` is beyond the end of `body`.
    pub fn slice(self, body: &Bytes) -> Result<Bytes, Error> {
        let size = body.len() as u64;
        if self.offset > size {
            return Err(Error::Other(format!(
                "range starting at {} is past the end of the {size}-byte object",
                self.offset
            )));
        }
        let end = size.min(self.offset.saturating_add(self.len));
        // Both bounds are at most `size`, which came from a `usize`.
        Ok(body.slice(self.offset as usize..end as usize))
    }
}

/// What a HEAD says about an object: its size, a content tag and when it was last written.
///
/// Two objects are taken to hold the same bytes when [`Meta::same_content`] says so, so a
/// backend's tag must be a function of the content (a checksum, or an entity tag that the
/// backend computes from the bytes) and the same function on both sides of a comparison. The
/// time of the last write is not part of the content: it is a backend's own clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// The object's size in bytes.
    pub size: u64,
    /// The object's content tag, compared as it is.
    pub tag: String,
    /// When the object was last written, in milliseconds since the Unix epoch, as the
    /// backend's clock counts them (a backend with a coarser clock, whole seconds as S3 has,
    /// gives equal times to objects written close together).
    pub modified: u64,
}

impl Meta {
    /// Whether both describe the same bytes: the same size and the same content tag.
    #[must_use]
    pub fn same_content(&self, other: &Meta) -> bool {
        (self.size, &self.tag) == (other.size, &other.tag)
    }
}

/// One object in a listing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Listed {
    /// The object's key.
    pub key: String,
    /// The object's size in bytes.
    pub size: u64,
    /// When the object was last written, as in [`Meta::modified`].
    pub modified: u64,
}

/// Whether `key` sorts after the key a listing is to start after, if there is one.
fn is_after(key: &str, start_after: Option<&str>) -> bool {
    start_after.is_none_or(|after| key > after)
}

/// An S3-compatible object store with the calls this crate needs, and no more.
///
/// Objects are immutable: a key is written once. The exception is the few small objects that
/// the proxy and the reconciler keep for themselves (the lease, the checkpoints, the usage),
/// which [`Backend::put_if_match`] rewrites in place. The trait is used as `dyn Backend`, so
/// a proxy holds its two tiers without a type parameter.
///
/// An implementation over a network bounds each call with a timeout of its own: the proxy
/// and the reconciler wait for the calls they make, and a call that never returns stops the
/// copies to the cold tier and the deletes behind it.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Writes an object, only if the key does not exist (`If-None-Match: *`).
    ///
    /// # Errors
    /// [`Error::AlreadyExists`] if the key exists; the stored object is left as it was.
    /// [`Error::Full`] if the backend has no room for the object.
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error>;

    /// Replaces the object, only if its content tag is `etag` (`If-Match`), and gives the
    /// content tag of the new object.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no object has the key. [`Error::PreconditionFailed`] if the
    /// object has another content tag; the stored object is left as it was.
    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error>;

    /// Reads the object, or the bytes of `range` of it (see [`ByteRange`]).
    ///
    /// # Errors
    /// [`Error::NotFound`] if no object has the key.
    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error>;

    /// The object's size, content tag and time of last write.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no object has the key.
    async fn head(&self, key: &str) -> Result<Meta, Error>;

    /// Deletes the object. Deleting a key that does not exist succeeds, so a retried delete
    /// is safe.
    async fn delete(&self, key: &str) -> Result<(), Error>;

    /// Every object whose key starts with `prefix`, in ascending key order. The listing need
    /// not be consistent with recent writes.
    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error>;

    /// A page of the objects [`Backend::list`] gives: at most `max_keys` of those whose key
    /// sorts after `start_after` (all of them for `None`), in ascending key order.
    ///
    /// The last key of a page is the continuation token of the next one, and a page of fewer
    /// than `max_keys` objects is the last. The provided implementation cuts the whole
    /// listing; a backend over a network lists a page at a time instead, and one whose store
    /// caps a page (S3 gives at most 1000 keys) reads further pages until it has `max_keys`
    /// objects or the listing ends, because a caller takes a short page for the last.
    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let all = self.list(prefix).await?;
        let page: Vec<Listed> = all
            .into_iter()
            .filter(|object| is_after(&object.key, start_after))
            .take(max_keys)
            .collect();
        Ok(page)
    }
}
