//! [`S3Backend`]: a [`Backend`] over the S3 client of `kbf-objstore` (feature `s3`).
//!
//! The backend is an adapter and keeps no state: each call of the trait is the S3 request of
//! the same meaning, sent by the client the workspace already has (path-style addressing,
//! SigV4, a timeout on every request), and each answer is read into this crate's [`Error`].
//!
//! - `put` is a PUT with `If-None-Match: *`; the 412 is [`Error::AlreadyExists`].
//! - `put_if_match` is a PUT with `If-Match: <tag>`, the tag as given; the 412 is
//!   [`Error::PreconditionFailed`] and a 404 is [`Error::NotFound`]. It gives the `ETag` of
//!   the answer.
//! - `get` is a GET, with `Range: bytes=<first>-<last>` for a range.
//! - `head` is a HEAD: `Content-Length`, `ETag` (the content tag, quotes included) and
//!   `Last-Modified`.
//! - `delete` is a DELETE; a 404 is a success.
//! - `list` and `list_page` are ListObjectsV2 with `prefix`, a `max-keys` of at most 1000,
//!   `start-after` on the first request and the `continuation-token` of each answer on the
//!   next. A page that repeats the token it was sent ends the listing with an
//!   [`Error::Other`].
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
//! - The content tag is the `ETag`. S3 makes it the MD5 of the bytes of an object written by
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
//! again after a failure in transport; a write never is.

use async_trait::async_trait;
use bytes::Bytes;
use kbf_objstore::s3::{S3Store, StampedObject};
use kbf_objstore::{KeyPrefix, ListToken, ObjectKey, ObjectStore, ObjectStoreError, PageSize};

use crate::{Backend, ByteRange, Error, Listed, Meta};

pub use kbf_objstore::Capabilities;
pub use kbf_objstore::s3::{Credentials, S3Config, S3ConfigError};

/// The greatest byte position a `Range` names. A store reads the positions as signed 64-bit
/// numbers and refuses or ignores a larger one, so a range is cut here before it is sent.
const LAST_POSITION: u64 = u64::MAX >> 1;

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

#[async_trait]
impl Backend for S3Backend {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let object = object_key(key)?;
        let put = self.store.put_new(&object, body, None);
        put.await.map_err(|e| map_error(e, key))
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        let object = object_key(key)?;
        let put = self.store.put_if_match(&object, body, etag);
        put.await.map_err(|e| map_error(e, key))
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
            tag: head.etag,
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
