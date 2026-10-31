//! The calls beyond [`ObjectStore`](crate::ObjectStore) that a store used for coordination
//! needs: the entity tag and time of last write of an object (HEAD), a replace that only an
//! object with a given tag takes (`If-Match`), a read of the whole object, and a listing
//! that gives the time each object was last written.
//!
//! They are inherent methods of [`S3Store`], so the trait and the other stores are as they
//! were. A read is sent again after a failure in transport, as a ranged read is (see
//! [`READ_RETRIES`]); the replace is sent once, since a connection that fails before the
//! answer leaves it unknown whether the object was replaced, and only the caller can read it
//! back.
//!
//! A create-only write (`If-None-Match: *`) is
//! [`put_new`](crate::ObjectStore::put_new) on a store that claims
//! [`Capabilities::conditional_put`](crate::Capabilities::conditional_put).

use std::future::Future;

use bytes::Bytes;
use reqwest::{Method, Response, StatusCode};

use super::{READ_RETRIES, READ_RETRY_WAIT, S3Store, failure, is_retryable, times, transport, xml};
use crate::{KeyPrefix, ListToken, ObjectKey, ObjectStoreError, PageSize};

/// What a HEAD says about an object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectHead {
    /// The object's size in bytes.
    pub size: u64,
    /// The entity tag as the store sends it, quotes included. For an object written by one
    /// PUT, S3 makes it the hex MD5 of the bytes, so equal bytes have equal tags.
    pub etag: String,
    /// When the object was last written, in milliseconds since the Unix epoch. S3 keeps
    /// whole seconds, so the last three digits are zero.
    pub modified_millis: u64,
}

/// One object of [`S3Store::list_stamped`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StampedObject {
    /// The object's key.
    pub key: ObjectKey,
    /// The object's size in bytes.
    pub size: u64,
    /// When the object was last written, in milliseconds since the Unix epoch, as the listing
    /// states it: S3 sends whole seconds, and another store may send milliseconds. It need not
    /// equal [`ObjectHead::modified_millis`] of the same object.
    pub modified_millis: u64,
}

/// One page of [`S3Store::list_stamped`], in ascending key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StampedPage {
    /// The objects on this page.
    pub objects: Vec<StampedObject>,
    /// Where the next page starts, or `None` if this is the last page.
    pub next: Option<ListToken>,
}

/// Runs `once` until it answers: it is run again after a failure in transport, with the
/// waits and the count of a ranged read.
async fn retrying<T, Fut>(mut once: impl FnMut() -> Fut) -> Result<T, ObjectStoreError>
where
    Fut: Future<Output = Result<T, ObjectStoreError>>,
{
    let mut wait = READ_RETRY_WAIT;
    for _ in 0..READ_RETRIES {
        match once().await {
            Err(e) if is_retryable(&e) => {
                tracing::debug!(error = %e, "read failed in transport; sending it again");
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
            answer => return answer,
        }
    }
    once().await
}

/// The text of a header of an answer, if it is there, is text, and is not empty.
fn header<'a>(resp: &'a Response, name: &str) -> Option<&'a str> {
    let value = resp.headers().get(name)?.to_str().ok()?;
    (!value.is_empty()).then_some(value)
}

impl S3Store {
    /// Replaces the object only if its entity tag is `etag` (`If-Match`, the tag sent as
    /// it is), and gives the tag of the new object.
    ///
    /// # Errors
    /// [`ObjectStoreError::NotFound`] if no object has the key. [`ObjectStoreError::Service`]
    /// with status 412 if the object has another tag; it is left as it was.
    /// [`ObjectStoreError::Protocol`] if the answer carries no `ETag`.
    pub async fn put_if_match(
        &self,
        key: &ObjectKey,
        body: Bytes,
        etag: &str,
    ) -> Result<String, ObjectStoreError> {
        let headers = vec![("if-match", etag.to_owned())];
        let resp = self
            .conn
            .send(Method::PUT, Some(key), &[], headers, body)
            .await?;
        let bad = |what: &str| ObjectStoreError::Protocol(format!("PUT of {key}: {what}"));
        match resp.status() {
            StatusCode::OK => {
                let tag = header(&resp, "etag");
                tag.map(str::to_owned).ok_or_else(|| bad("no ETag"))
            }
            StatusCode::NOT_FOUND => Err(ObjectStoreError::NotFound(key.clone())),
            _ => Err(failure(resp).await),
        }
    }

    /// The object's size, entity tag and time of last write.
    ///
    /// # Errors
    /// [`ObjectStoreError::NotFound`] if no object has the key.
    /// [`ObjectStoreError::Protocol`] if the answer lacks a size, a tag or a readable time.
    pub async fn head(&self, key: &ObjectKey) -> Result<ObjectHead, ObjectStoreError> {
        retrying(move || self.head_once(key)).await
    }

    async fn head_once(&self, key: &ObjectKey) -> Result<ObjectHead, ObjectStoreError> {
        let resp = self
            .conn
            .send(Method::HEAD, Some(key), &[], Vec::new(), Bytes::new())
            .await?;
        match resp.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND => return Err(ObjectStoreError::NotFound(key.clone())),
            _ => return Err(failure(resp).await),
        }
        let bad = |what: &str| ObjectStoreError::Protocol(format!("HEAD of {key}: {what}"));
        let size = header(&resp, "content-length")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| bad("missing or bad Content-Length"))?;
        let etag = header(&resp, "etag").ok_or_else(|| bad("no ETag"))?;
        let modified_millis = header(&resp, "last-modified")
            .and_then(times::http_date_millis)
            .ok_or_else(|| bad("missing or bad Last-Modified"))?;
        Ok(ObjectHead {
            size,
            etag: etag.to_owned(),
            modified_millis,
        })
    }

    /// The whole object.
    ///
    /// # Errors
    /// [`ObjectStoreError::NotFound`] if no object has the key.
    pub async fn get_all(&self, key: &ObjectKey) -> Result<Bytes, ObjectStoreError> {
        retrying(move || self.get_all_once(key)).await
    }

    async fn get_all_once(&self, key: &ObjectKey) -> Result<Bytes, ObjectStoreError> {
        let resp = self
            .conn
            .send(Method::GET, Some(key), &[], Vec::new(), Bytes::new())
            .await?;
        match resp.status() {
            StatusCode::OK => resp.bytes().await.map_err(transport),
            StatusCode::NOT_FOUND => Err(ObjectStoreError::NotFound(key.clone())),
            _ => Err(failure(resp).await),
        }
    }

    /// One page of the objects under `prefix`, as [`list`](crate::ObjectStore::list) gives
    /// it, with the time each was last written.
    ///
    /// # Errors
    /// As [`list`](crate::ObjectStore::list), and [`ObjectStoreError::Protocol`] if an
    /// object is listed without a readable time.
    pub async fn list_stamped(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<StampedPage, ObjectStoreError> {
        let body = retrying(move || self.list_body(prefix, after, max_keys)).await?;
        xml::stamped_page(&body)
    }
}
