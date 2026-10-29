//! The minimal API of an S3-compatible store, as this crate uses it.

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;

use crate::Error;

/// The body of a part of a multipart upload, as the chunks it arrives in, so that a part of any
/// size is stored without being held in memory whole.
///
/// A chunk that is an error ends the body: the backend stores nothing of the part and gives
/// that error.
pub type PartBody<'a> = BoxStream<'a, Result<Bytes, Error>>;

/// The error of a multipart call that a backend does not override.
fn not_supported(call: &str) -> Error {
    let text = format!("{call}: this backend does not do multipart uploads");
    Error::Unsupported(text)
}

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

/// A multipart upload that was started and is neither completed nor aborted, as
/// [`Backend::list_uploads`] lists it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OpenUpload {
    /// The key of the object the upload is for.
    pub key: String,
    /// The id of the upload, which names it in the calls of [`Backend::upload_part`],
    /// [`Backend::complete_upload`] and [`Backend::abort_upload`].
    pub id: String,
    /// When the upload was started, in milliseconds since the Unix epoch, as the backend's
    /// clock counts them (as for [`Meta::modified`]).
    pub initiated: u64,
}

/// Where a page of [`Backend::list_uploads`] starts: after this key and, if there is one, this
/// upload id, which is the last upload of the page before.
pub type UploadsAfter<'a> = (&'a str, Option<&'a str>);

/// The uploads of `uploads` that a page of [`Backend::list_uploads`] holds, for a backend that
/// orders the uploads of a key by id: those of a key under `prefix` that come after `after`, in
/// ascending order of key and id, at most `max_uploads`.
pub(crate) fn upload_page(
    uploads: impl IntoIterator<Item = OpenUpload>,
    prefix: &str,
    after: Option<UploadsAfter<'_>>,
    max_uploads: usize,
) -> Vec<OpenUpload> {
    let is_after = |upload: &OpenUpload| match after {
        None => true,
        Some((key, None)) => upload.key.as_str() > key,
        Some((key, Some(id))) => (upload.key.as_str(), upload.id.as_str()) > (key, id),
    };
    let mut page: Vec<OpenUpload> = uploads
        .into_iter()
        .filter(|upload| upload.key.starts_with(prefix) && is_after(upload))
        .collect();
    page.sort_by(|a, b| (&a.key, &a.id).cmp(&(&b.key, &b.id)));
    page.truncate(max_uploads);
    page
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
/// copies to the cold tier and the deletes behind it. The exception is [`Backend::upload_part`],
/// whose time is set by the client that sends the part (see there).
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

    /// Starts a multipart upload of an object `key`, and gives the id of the upload.
    ///
    /// The four calls of a multipart upload (this one, [`Backend::upload_part`],
    /// [`Backend::complete_upload`] and [`Backend::abort_upload`]) are provided: a backend
    /// that does not override them answers each with [`Error::Unsupported`] (so does
    /// [`Backend::upload_part_with_len`], which only adds the length of the part). A backend
    /// that does keeps the parts of an upload apart from the objects (they are not listed and not
    /// read), and the id names an upload of one key only. A backend may refuse a key it cannot
    /// store at once (the directory backend does, for a key that is empty or too long to be a
    /// file name); a key it would take is not looked at again until the upload is completed.
    ///
    /// # Errors
    /// [`Error::Unsupported`] unless the backend does multipart uploads. [`Error::Reserved`]
    /// for a key the backend keeps for itself. [`Error::Other`] for a key it cannot store.
    async fn create_upload(&self, _key: &str) -> Result<String, Error> {
        Err(not_supported("create_upload"))
    }

    /// Stores the part `number` of the upload `id` of `key` from `body`, in place of the part
    /// of that number if there is one, and gives the part's content tag, which a completion
    /// names.
    ///
    /// The body is read a chunk at a time and written as it arrives; an implementation does not
    /// hold the whole part in memory (the in-memory backend of the tests does). A body that
    /// ends with an error stores nothing, and the call gives that error.
    ///
    /// The time of the call is set by the client that sends the part, so a bound on it counts
    /// the time with no chunk taken, not the time of the whole call, which would cut a client
    /// that is slow and steady. The server already bounds the wait for each chunk of a body.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no upload `id` of `key` is open. [`Error::Unsupported`] unless
    /// the backend does multipart uploads. The error of `body`, if it ends with one.
    async fn upload_part(
        &self,
        _key: &str,
        _id: &str,
        _number: u32,
        _body: PartBody<'_>,
    ) -> Result<String, Error> {
        Err(not_supported("upload_part"))
    }

    /// As [`Backend::upload_part`], for a part whose length may be known before it is read:
    /// `len` is the number of bytes `body` is to give, or `None` if the caller does not know.
    ///
    /// A store that needs the length of a request body before it is sent (S3 does) is written
    /// to by a backend that overrides this call, and sends the part as it arrives; it answers
    /// [`Backend::upload_part`] as this call with `None`, for which it must hold the part,
    /// since a stream does not say how long it is. A backend that uses `len` stores nothing
    /// of a body that gives more or fewer bytes, and the call is an error. The provided method
    /// ignores `len`, as the in-memory and the directory backend have no use for it.
    ///
    /// # Errors
    /// As [`Backend::upload_part`].
    async fn upload_part_with_len(
        &self,
        key: &str,
        id: &str,
        number: u32,
        _len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.upload_part(key, id, number, body).await
    }

    /// Completes the upload `id` of `key`: creates the object `key` from the parts named, in
    /// the order given, which is ascending by part number, each with the content tag
    /// [`Backend::upload_part`] gave it. Gives the content tag of the new object, which
    /// [`Backend::head`] gives it too. The in-memory and the directory backend give the tag a
    /// [`Backend::put`] of the same bytes gives; a store that tags an object made of parts by
    /// its parts (S3 does: a hash of the tags of the parts and their number) gives that tag,
    /// which is not the tag of a put of the same bytes, so the tags of two tiers tell that two
    /// objects are the same only if both were made the same way. The parts and the upload are
    /// gone once the object is created.
    ///
    /// Like [`Backend::put`] the completion only creates: where the key exists the upload is
    /// left open.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no upload `id` of `key` is open. [`Error::InvalidPart`] if a part
    /// named was not uploaded or has another content tag. [`Error::AlreadyExists`] if the key
    /// exists. [`Error::Unsupported`] unless the backend does multipart uploads.
    async fn complete_upload(
        &self,
        _key: &str,
        _id: &str,
        _parts: &[(u32, String)],
    ) -> Result<String, Error> {
        Err(not_supported("complete_upload"))
    }

    /// Drops the upload `id` of `key` and the parts it holds.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no upload `id` of `key` is open. [`Error::Unsupported`] unless
    /// the backend does multipart uploads.
    async fn abort_upload(&self, _key: &str, _id: &str) -> Result<(), Error> {
        Err(not_supported("abort_upload"))
    }

    /// A page of the uploads that are open (started, and neither completed nor aborted) for a
    /// key that starts with `prefix`: at most `max_uploads` of them, in ascending order of
    /// key, and within a key in the order the backend keeps (by id for the in-memory and the
    /// directory backend, by the time of the start for a store over S3).
    ///
    /// `after` is the last upload of the page before: the page starts after that upload,
    /// or, where `after` names a key and no id, after every upload of that key. A page of
    /// fewer than `max_uploads` uploads is the last. The order within a key is the backend's,
    /// so the marker of a page is read by the backend that gave it. An upload that is started
    /// or ended while the listing is read may or may not be in it. Like the other multipart
    /// calls, this one is [`Error::Unsupported`] unless the backend overrides it.
    ///
    /// # Errors
    /// [`Error::Unsupported`] unless the backend does multipart uploads.
    async fn list_uploads(
        &self,
        _prefix: &str,
        _after: Option<UploadsAfter<'_>>,
        _max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        Err(not_supported("list_uploads"))
    }
}
