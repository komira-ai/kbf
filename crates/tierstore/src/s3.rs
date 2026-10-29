//! [`S3Backend`]: a [`Backend`] over the S3 client of `kbf-objstore` (feature `s3`).
//!
//! The backend is an adapter and keeps no state: each call of the trait is the S3 request of
//! the same meaning, sent by the client the workspace already has (path-style addressing,
//! SigV4, a timeout on every request), and each answer is read into this crate's [`Error`].
//!
//! - `put` is a PUT with `If-None-Match: *`; the 412 is [`Error::AlreadyExists`]. Create-only
//!   (here, in `put_stream` and in `complete_upload`) needs a store that honours conditional
//!   writes: one that ignores `If-None-Match` on a put or on the completion of a multipart upload
//!   would replace an object that exists.
//! - `put_stream` is the store client's streamed write, given the chunks of the body as they
//!   arrive and their total length (the `Content-Length` of the request; without one the body
//!   is held first, since S3 takes no request of unknown length). A body of at most 16 MiB is
//!   gathered and sent as one PUT with `If-None-Match: *`; a longer one is a multipart upload
//!   whose parts are sent as they fill, completed with `If-None-Match: *` only after the body
//!   has ended with exactly its length, and aborted on any failure. Either way the object comes
//!   into being in the last request, after the whole body was read and checked by the caller, so
//!   a body that ends with an error, or is cut short, leaves no object and no staged key, and
//!   at most 16 MiB of the body is held at a time. A key that exists is
//!   [`Error::AlreadyExists`].
//! - `put_if_match` is a PUT with `If-Match: "<tag>"` (a tag given in quotes is not quoted
//!   twice); the 412 is [`Error::PreconditionFailed`] and a 404 is [`Error::NotFound`]. It
//!   gives the `ETag` of the answer, without its quotes.
//! - `get` is a GET, with `Range: bytes=<first>-<last>` for a range.
//! - `head` is a HEAD: `Content-Length`, `ETag` (the content tag, without its quotes, as the
//!   other backends give it and the S3 front, which quotes it, expects) and `Last-Modified`.
//! - `delete` is a DELETE; a 404 is a success.
//! - `list` and `list_page` are ListObjectsV2 with `prefix`, a `max-keys` of at most 1000,
//!   `start-after` on the first request and the `continuation-token` of each answer on the
//!   next. A page that repeats the token it was sent ends the listing with an
//!   [`Error::Other`].
//! - `create_upload` is CreateMultipartUpload and gives the upload id of the store.
//! - `upload_part_with_len` is UploadPart whose body is the chunks of the part as they
//!   arrive, never collected: the request carries the length as its `Content-Length`, is
//!   signed with `UNSIGNED-PAYLOAD`, and ends with the body, so a part of any size is held a
//!   few chunks at a time. A body that gives an error, also after its last byte, or more or
//!   fewer bytes than the length, stores nothing and gives that error ([`Error::Other`] for
//!   the length). `upload_part`, for a caller that does not know the length, holds the whole
//!   part first, since S3 takes no part of unknown length. The tag of a part is the `ETag` of
//!   the answer without its quotes, which is how the S3 front names a part to its client and
//!   how a completion names it again. The signature of a part carries no hash of its bytes, so
//!   the store has none to check them against; the connection is plain HTTP (the client
//!   refuses TLS), so a part is as safe on its way as the network between the proxy and the
//!   store.
//! - `complete_upload` is CompleteMultipartUpload with `If-None-Match: *`, so a key that
//!   exists is [`Error::AlreadyExists`] and the upload stays open. It gives the `ETag` the
//!   answer names for the object (without its quotes), which is what `head` gives it too, and
//!   needs no second request, which could fail after the object was made. On S3 the tag
//!   of an object made of parts is not the MD5 of its bytes, so it is not the tag of a `put`
//!   of the same bytes, unlike in the in-memory and the directory backend.
//! - `abort_upload` is AbortMultipartUpload.
//! - An upload the store does not hold (`NoSuchUpload`) is [`Error::NotFound`] with the upload
//!   id, and a completion the store refuses for its parts (`InvalidPart`, `InvalidPartOrder`,
//!   `EntityTooSmall`) is [`Error::InvalidPart`]. S3 takes no part under 5 MiB but the last.
//!
//! Notes on the mapping:
//!
//! - S3 answers 416 to a range that starts at or past the end of the object, and a range of
//!   length zero is no request at all. The trait wants no bytes for a range starting at the
//!   end and an [`Error::Other`] for one starting past it, so a HEAD decides both, and finds
//!   out whether the key exists. A 416 to a range that holds bytes of the object is an
//!   [`Error::Other`] too, not an empty read.
//! - A store reads the positions of a `Range` as signed 64-bit numbers, so a range that
//!   runs past 2^63 - 1 is cut there before it is sent (the object cannot be longer).
//! - A listing of more than 1000 objects is several requests: `list_page` follows the
//!   continuation tokens until it has `max_keys` objects or the listing ends, since a page
//!   shorter than `max_keys` means the last.
//! - The content tag is the `ETag`, without its quotes. S3 makes it the MD5 of the bytes of an object written by
//!   one PUT, which is what every write of this backend is, so equal bytes have equal tags
//!   in two buckets of one kind of store. A store that encrypts with keys of its own makes
//!   the tag from something else: compare tags only between backends of one store.
//! - `modified` of a [`Meta`] is the `Last-Modified` of a HEAD, in whole seconds. That of a
//!   [`Listed`] is the `LastModified` of the listing, as fine as the store sends it (MinIO
//!   sends milliseconds), so the two times of one object need not be equal.
//! - Keys must be ones the client can name without escaping (`A-Z a-z 0-9 - _ . ~`, and `/`
//!   between non-empty segments); any other key is an [`Error::Other`] before a request is
//!   made. The `start_after` of `list_page` is named as a key as well, so one that is not
//!   nameable (`a/`, say) is refused although another backend takes any string.
//! - A request that did not complete, and the statuses 408, 429, 502, 503 and 504, are
//!   [`Error::Unavailable`]. 507 (Insufficient Storage, which is how MinIO reports a full
//!   drive) is [`Error::Full`]. Anything else that is not a success is [`Error::Other`].
//!
//! Each request is bounded by the `request_timeout` of the [`S3Config`], and a read is sent
//! again after a failure in transport; a write never is. A part is the exception to the bound:
//! it is as slow as the client that sends it, so only a part that makes no progress for the
//! `request_timeout` is cut, as an [`Error::Unavailable`].

use std::future::Future;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use futures::future;
use futures::stream;
use kbf_objstore::s3::{S3Store, StampedObject};
use kbf_objstore::{
    BodyStream, KeyPrefix, ListToken, ObjectKey, ObjectStore, ObjectStoreError, PageSize,
};
use tokio::sync::mpsc;

use crate::{Backend, ByteRange, Error, Listed, Meta, PartBody};

pub use kbf_objstore::Capabilities;
pub use kbf_objstore::s3::{Credentials, S3Config, S3ConfigError};

/// The greatest byte position a `Range` names. A store reads the positions as signed 64-bit
/// numbers and refuses or ignores a larger one, so a range is cut here before it is sent.
const LAST_POSITION: u64 = u64::MAX >> 1;

/// How many chunks of a part wait for the request that sends them to take them.
const CHUNKS_WAITING: usize = 1;

/// The codes with which a store refuses a completion for the parts it names.
const INVALID_PART_CODES: [&str; 3] = ["InvalidPart", "InvalidPartOrder", "EntityTooSmall"];

/// One bucket of an S3-compatible store, as a [`Backend`].
#[derive(Debug)]
pub struct S3Backend {
    store: S3Store,
}

impl S3Backend {
    /// A backend for the bucket `config` names. Does no I/O.
    ///
    /// The capabilities of `config` are replaced: the backend needs the conditional put
    /// whatever the caller claims (a create-only put that replaces is not [`Backend::put`]),
    /// and has no use for Object Lock.
    ///
    /// # Errors
    /// [`S3ConfigError`] if the endpoint or the bucket name is refused.
    pub fn new(mut config: S3Config) -> Result<Self, S3ConfigError> {
        config.capabilities = Capabilities {
            conditional_put: true,
            object_lock: false,
        };
        Ok(Self {
            store: S3Store::new(config)?,
        })
    }

    /// The bytes of `range` of the object, for a range that is empty or that S3 answers 416
    /// to: a HEAD says how long the object is, and so whether the 416 was right.
    async fn read_range(
        &self,
        object: &ObjectKey,
        key: &str,
        range: ByteRange,
    ) -> Result<Bytes, Error> {
        let room = LAST_POSITION.saturating_sub(range.offset());
        let len = range.size().min(room);
        if let Some(asked) = kbf_objstore::ByteRange::new(range.offset(), len) {
            match self.store.get_range(object, asked).await {
                Err(ObjectStoreError::InvalidRange { .. }) => {}
                answer => return answer.map_err(|e| map_error(e, key)),
            }
        }
        let head = self.store.head(object).await;
        let size = head.map_err(|e| map_error(e, key))?.size;
        if range.offset() > size {
            return Err(Error::Other(format!(
                "range starting at {} is past the end of the {size}-byte object",
                range.offset()
            )));
        }
        let held = range.size().min(size - range.offset());
        if held > 0 {
            return Err(Error::Other(format!(
                "the store refused the range of {held} bytes at {} of the {size}-byte object",
                range.offset()
            )));
        }
        Ok(Bytes::new())
    }

    /// At most `limit` of the objects under `prefix` that sort after `start_after`, in key
    /// order, asking for a page of the client at a time.
    async fn listed(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Listed>, Error> {
        let under = KeyPrefix::new(prefix).map_err(|e| Error::Other(e.to_string()))?;
        let after = start_after.map(object_key).transpose()?;
        let mut after = after.map(|key| ListToken::after(&key));
        let mut listed = Vec::new();
        while listed.len() < limit {
            let room = limit - listed.len();
            let size = u16::try_from(room).ok().and_then(PageSize::new);
            let size = size.unwrap_or(PageSize::MAX);
            let page = self.store.list_stamped(&under, after.as_ref(), size);
            let page = page.await.map_err(|e| map_error(e, prefix))?;
            listed.extend(page.objects.into_iter().map(listed_of));
            let Some(next) = page.next else { break };
            if after.as_ref() == Some(&next) {
                let why = format!("{prefix:?} listing stalled: the token did not change");
                return Err(Error::Other(why));
            }
            after = Some(next);
        }
        listed.truncate(limit);
        Ok(listed)
    }
}

/// The key as the client names it, or [`Error::Other`] if it cannot be named.
fn object_key(key: &str) -> Result<ObjectKey, Error> {
    ObjectKey::new(key).map_err(|e| Error::Other(e.to_string()))
}

/// An object of a listing of the client as one of this crate.
fn listed_of(object: StampedObject) -> Listed {
    Listed {
        key: object.key.as_str().to_owned(),
        size: object.size,
        modified: object.modified_millis,
    }
}

/// What a failed call of the client means in the terms of this crate, for the call on `key`.
fn map_error(error: ObjectStoreError, key: &str) -> Error {
    let key = key.to_owned();
    let message = error.to_string();
    match error {
        ObjectStoreError::NotFound(_) => Error::NotFound(key),
        ObjectStoreError::AlreadyExists(_) => Error::AlreadyExists(key),
        ObjectStoreError::Transport(_) => Error::Unavailable(message),
        ObjectStoreError::Service { status, .. } => by_status(status, key, message),
        _ => Error::Other(message),
    }
}

/// What an answer of the store with this HTTP status means.
fn by_status(status: u16, key: String, message: String) -> Error {
    match status {
        412 => Error::PreconditionFailed(key),
        408 | 429 | 502..=504 => Error::Unavailable(message),
        507 => Error::Full(message),
        _ => Error::Other(message),
    }
}

/// What a failed call on the upload `id` of `key` means: an upload the store does not hold is
/// [`Error::NotFound`] with the id, a completion it refuses for its parts is
/// [`Error::InvalidPart`], and anything else is what it is for any call on `key`.
fn upload_error(error: ObjectStoreError, key: &str, id: &str) -> Error {
    let code = service_code(&error);
    if code == "NoSuchUpload" {
        return Error::NotFound(id.to_owned());
    }
    if INVALID_PART_CODES.contains(&code) {
        return Error::InvalidPart(error.to_string());
    }
    map_error(error, key)
}

/// The S3 code of an answer of the store that refused, or the empty text for any other error.
fn service_code(error: &ObjectStoreError) -> &str {
    match error {
        ObjectStoreError::Service { code, .. } => code.as_str(),
        _ => "",
    }
}

/// The tag of an object or of a part as the other backends give it and the front names it:
/// the `ETag` without its quotes.
fn unquoted(etag: &str) -> String {
    etag.trim_matches('"').to_owned()
}

/// A content tag as an entity tag is sent to the store in `If-Match`: in quotes, once.
fn quoted(tag: &str) -> String {
    format!("\"{}\"", tag.trim_matches('"'))
}

/// The bytes of `body`, collected, for a part of a length that is not known.
async fn collect(mut body: PartBody<'_>) -> Result<Bytes, Error> {
    let mut whole = BytesMut::new();
    while let Some(chunk) = body.next().await {
        whole.extend_from_slice(&chunk?);
    }
    Ok(whole.freeze())
}

/// A part that is held whole, as a length and a body of one chunk.
fn held(whole: Bytes) -> (u64, PartBody<'static>) {
    let len = whole.len() as u64;
    (len, stream::once(future::ready(Ok(whole))).boxed())
}

/// Sends the chunks of `body` to `sink`, to be the body of the request that sends them to the
/// store, and gives the error that ended `body`, if one did. It stops when the request is over
/// and takes no more.
async fn feed(
    mut body: PartBody<'_>,
    sink: mpsc::Sender<Result<Bytes, ObjectStoreError>>,
) -> Option<Error> {
    while let Some(chunk) = body.next().await {
        let sent = match chunk {
            Ok(bytes) => sink.send(Ok(bytes)).await,
            Err(error) => {
                let ended = ObjectStoreError::Body(error.to_string());
                let _ = sink.send(Err(ended)).await;
                return Some(error);
            }
        };
        if sent.is_err() {
            return None;
        }
    }
    None
}

/// Sends `body` to the store as the body of the request that `send` makes, which is given its
/// length and the chunks as they arrive. A body whose length is not known is held first, since
/// S3 takes no request of unknown length. Gives the error of `body`, if it ended with one,
/// else the answer of the store.
async fn streamed<T, F, Fut>(
    body: PartBody<'_>,
    len: Option<u64>,
    send: F,
) -> Result<Result<T, ObjectStoreError>, Error>
where
    F: FnOnce(u64, BodyStream) -> Fut,
    Fut: Future<Output = Result<T, ObjectStoreError>>,
{
    let (len, body) = match len {
        Some(len) => (len, body),
        None => held(collect(body).await?),
    };
    let (sink, mut chunks) = mpsc::channel(CHUNKS_WAITING);
    let source: BodyStream = stream::poll_fn(move |cx| chunks.poll_recv(cx)).boxed();
    let (ended, sent) = futures::join!(feed(body, sink), send(len, source));
    match ended {
        Some(error) => Err(error),
        None => Ok(sent),
    }
}

#[async_trait]
impl Backend for S3Backend {
    /// S3 takes no request of unknown length: given none, the backend holds the body first.
    fn needs_length(&self) -> bool {
        true
    }

    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let object = object_key(key)?;
        let put = self.store.put_new(&object, body, None);
        put.await.map_err(|e| map_error(e, key))
    }

    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        let object = object_key(key)?;
        let store = &self.store;
        let sending = |len, source| store.put_stream(&object, len, source);
        let sent = streamed(body, len, sending).await?;
        sent.map_err(|e| map_error(e, key))
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        let object = object_key(key)?;
        let etag = quoted(etag);
        let put = self.store.put_if_match(&object, body, &etag);
        let tag = put.await.map_err(|e| map_error(e, key))?;
        Ok(unquoted(&tag))
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        let object = object_key(key)?;
        let Some(range) = range else {
            let all = self.store.get_all(&object);
            return all.await.map_err(|e| map_error(e, key));
        };
        self.read_range(&object, key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        let object = object_key(key)?;
        let head = self.store.head(&object);
        let head = head.await.map_err(|e| map_error(e, key))?;
        Ok(Meta {
            size: head.size,
            tag: unquoted(&head.etag),
            modified: head.modified_millis,
        })
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        let object = object_key(key)?;
        let delete = self.store.delete(&object);
        delete.await.map_err(|e| map_error(e, key))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.listed(prefix, None, usize::MAX).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        self.listed(prefix, start_after, max_keys).await
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        let object = object_key(key)?;
        let create = self.store.create_multipart(&object);
        create.await.map_err(|e| map_error(e, key))
    }

    async fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        let sized = self.upload_part_with_len(key, id, number, None, body);
        sized.await
    }

    async fn upload_part_with_len(
        &self,
        key: &str,
        id: &str,
        number: u32,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        let object = object_key(key)?;
        let store = &self.store;
        let sending = |len, source| store.upload_part_stream(&object, id, number, len, source);
        let sent = streamed(body, len, sending).await?;
        let etag = sent.map_err(|e| upload_error(e, key, id))?;
        Ok(unquoted(&etag))
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        let object = object_key(key)?;
        let named: Vec<(u32, String)> = parts
            .iter()
            .map(|(number, tag)| (*number, quoted(tag)))
            .collect();
        let complete = self.store.complete_multipart(&object, id, &named);
        let etag = complete.await.map_err(|e| upload_error(e, key, id))?;
        Ok(unquoted(&etag))
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        let object = object_key(key)?;
        let abort = self.store.abort_multipart(&object, id);
        abort.await.map_err(|e| upload_error(e, key, id))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn service(status: u16) -> ObjectStoreError {
        ObjectStoreError::Service {
            status,
            code: "Code".to_owned(),
            message: "message".to_owned(),
        }
    }

    /// Asserts that each answer of the client is the error of this crate the trait documents
    /// for it: not found, already exists and a failed precondition name the key, a request
    /// that did not complete and the statuses 408, 429 and 502 to 504 are unavailable, 507 is
    /// full, and any other answer is an other error that says what it was.
    #[test]
    fn answers_of_the_client_are_the_errors_the_trait_documents() {
        let key = ObjectKey::new("k").unwrap();
        let name = "k".to_owned();
        let found = map_error(ObjectStoreError::NotFound(key.clone()), "k");
        assert_eq!(found, Error::NotFound(name.clone()));
        let exists = map_error(ObjectStoreError::AlreadyExists(key), "k");
        assert_eq!(exists, Error::AlreadyExists(name.clone()));
        let failed = map_error(service(412), "k");
        assert_eq!(failed, Error::PreconditionFailed(name));

        let reset = Arc::new(std::io::Error::other("reset"));
        let down = map_error(ObjectStoreError::Transport(reset), "k");
        assert!(down.is_unavailable(), "{down}");
        for status in [408, 429, 502, 503, 504] {
            let down = map_error(service(status), "k");
            assert!(down.is_unavailable(), "{status}");
        }
        let full = map_error(service(507), "k");
        assert!(matches!(full, Error::Full(_)), "{full}");
        for status in [400, 403, 409, 500, 501, 505, 506, 508] {
            let other = map_error(service(status), "k");
            assert!(matches!(other, Error::Other(_)), "{status}: {other}");
        }
        let odd = map_error(ObjectStoreError::Protocol("odd".to_owned()), "k");
        assert!(odd.to_string().contains("odd"), "{odd}");
    }
}
