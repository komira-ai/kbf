//! The calls of a multipart upload one at a time, for a caller that drives the upload itself
//! (an adapter whose parts come from clients of its own): CreateMultipartUpload, UploadPart,
//! CompleteMultipartUpload for the parts the caller names, and AbortMultipartUpload.
//! [`put_stream`](crate::ObjectStore::put_stream) does all of it on its own for an object it
//! is given (`multipart.rs`); these calls leave the choice of the parts, and the failure
//! that aborts the upload, to the caller.
//!
//! A part is sent as it is read. [`S3Store::upload_part_stream`] takes the length of the part
//! and a stream of its bytes, and sends a request whose `Content-Length` is that length and
//! whose body is the stream, so a part of any size holds a few pieces of it at once and no
//! more. The hash of a body that is still to be read is not known when the request is
//! signed, so the request is signed with `UNSIGNED-PAYLOAD`; the `ETag` the store answers is
//! the store's own hash of the part.
//!
//! The stream is held to the length and to its own end:
//!
//! - A stream that gives more than the length, or ends before it, fails the request with
//!   [`ObjectStoreError::Body`].
//! - A stream that gives an error fails the request with that error, also when it gives it
//!   after the last byte: the last piece is not written until the stream has ended without
//!   an error, because the store takes a request whose body has its `Content-Length` bytes
//!   as whole, and would store a part that the stream went on to call broken. A request
//!   that fails with its body unsent or unfinished leaves the store with no part.
//!
//! The call fails with the error that ended the body, not with the failure of the request
//! that it caused, which says only that the request did not complete.
//!
//! A request has no limit on its whole time, since a part is as slow as the stream that
//! feeds it. It ends as `Stalled` (a transport error) if the store takes nothing of the
//! body, and gives no answer, for a whole `request_timeout` while the stream is not what is
//! being waited for: a stream that is slow in giving a piece is bounded by its own source.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, ready};

use bytes::Bytes;
use futures::StreamExt;
use futures::stream::Fuse;
use http_body::Frame;
use reqwest::header::CONTENT_LENGTH;
use reqwest::{Method, Response, StatusCode};

use super::sigv4::UNSIGNED_PAYLOAD;
use super::{Conn, S3Store, Stalled, failure, multipart, transport, xml};
use crate::body::{too_long, too_short};
use crate::{BodyStream, KeyPrefix, ObjectKey, ObjectStoreError, PageSize};

/// A multipart upload that was started and is neither completed nor aborted, as
/// [`S3Store::list_multipart_uploads`] lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenUpload {
    /// The key of the object the upload is for.
    pub key: ObjectKey,
    /// The id of the upload.
    pub upload_id: String,
    /// When the upload was started, in milliseconds since the Unix epoch, as the listing
    /// states it (S3 sends whole seconds).
    pub initiated_millis: u64,
}

/// One page of [`S3Store::list_multipart_uploads`], in the order the store gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadsPage {
    /// The uploads on this page.
    pub uploads: Vec<OpenUpload>,
    /// The key and the upload id after which the next page starts, or `None` if this is the
    /// last page.
    pub next: Option<(ObjectKey, String)>,
}

impl S3Store {
    /// ListMultipartUploads: one page of at most `max_uploads` of the uploads under `prefix`
    /// that are neither completed nor aborted. The page starts after `after`, the key (and
    /// the upload id, if the page before ended inside a key) that ends the page before;
    /// `None` is the first page. The order is the store's: by key, and by the time of the
    /// start within a key.
    ///
    /// # Errors
    /// [`ObjectStoreError::Service`] if the store refuses,
    /// [`ObjectStoreError::Transport`] if the request does not complete,
    /// [`ObjectStoreError::Protocol`] if the answer is not a listing of uploads, or lists one
    /// without a key, an id or a readable time.
    pub async fn list_multipart_uploads(
        &self,
        prefix: &KeyPrefix,
        after: Option<(&ObjectKey, Option<&str>)>,
        max_uploads: PageSize,
    ) -> Result<UploadsPage, ObjectStoreError> {
        let mut query = vec![
            ("uploads", String::new()),
            ("prefix", prefix.as_str().to_owned()),
            ("max-uploads", max_uploads.get().to_string()),
        ];
        if let Some((key, id)) = after {
            query.push(("key-marker", key.as_str().to_owned()));
            if let Some(id) = id {
                query.push(("upload-id-marker", id.to_owned()));
            }
        }
        let resp = self
            .conn
            .send(Method::GET, None, &query, Vec::new(), Bytes::new())
            .await?;
        if resp.status() != StatusCode::OK {
            return Err(failure(resp).await);
        }
        let body = resp.bytes().await.map_err(transport)?;
        xml::uploads_page(&body)
    }

    /// CreateMultipartUpload: starts an upload of an object `key` and gives its id. Nothing
    /// is stored until the upload is completed, and an upload that is neither completed nor
    /// aborted holds the parts sent to it until a lifecycle rule of the bucket removes it.
    ///
    /// # Errors
    /// [`ObjectStoreError::Service`] if the store refuses, [`ObjectStoreError::Transport`]
    /// if the request does not complete, [`ObjectStoreError::Protocol`] if the answer has no
    /// upload id.
    pub async fn create_multipart(&self, key: &ObjectKey) -> Result<String, ObjectStoreError> {
        multipart::create_id(&self.conn, key).await
    }

    /// UploadPart: stores the part `number` of the upload `upload_id` of `key` from `body`,
    /// which must give `len` bytes and then end (see the module docs), and gives the `ETag`
    /// of the part as the store sends it, quotes included. A part sent again with the same
    /// number replaces the one before.
    ///
    /// # Errors
    /// [`ObjectStoreError::Body`] if `body` gives more or fewer than `len` bytes, the error
    /// of `body` if it gives one, [`ObjectStoreError::Service`] if the store refuses (an
    /// upload it does not hold is `NoSuchUpload`), [`ObjectStoreError::Transport`] if the
    /// request does not complete or stalls, [`ObjectStoreError::Protocol`] if the answer has
    /// no `ETag`.
    pub async fn upload_part_stream(
        &self,
        key: &ObjectKey,
        upload_id: &str,
        number: u32,
        len: u64,
        body: BodyStream,
    ) -> Result<String, ObjectStoreError> {
        let query = [
            ("partNumber", number.to_string()),
            ("uploadId", upload_id.to_owned()),
        ];
        let resp = self.conn.send_streamed(key, &query, len, body).await?;
        if resp.status() != StatusCode::OK {
            return Err(self.conn.within(failure(resp)).await?);
        }
        multipart::part_etag(&resp, key, number)
    }

    /// CompleteMultipartUpload: creates the object `key` from the parts named, each a number
    /// and the `ETag` the store gave it, quotes included, in ascending order of their
    /// numbers. On a store that claims conditional writes the request carries
    /// `If-None-Match: *`, so a key that exists is not replaced and the upload stays open.
    /// Gives the `ETag` of the object as the answer to the completion names it, quotes
    /// included, so that no second request is needed to learn it.
    ///
    /// # Errors
    /// [`ObjectStoreError::AlreadyExists`] if the key exists (conditional writes only),
    /// [`ObjectStoreError::Service`] if the store refuses (`NoSuchUpload`, `InvalidPart`,
    /// `EntityTooSmall`), or answers 200 and then an error,
    /// [`ObjectStoreError::Transport`] if the request does not complete,
    /// [`ObjectStoreError::Protocol`] if the answer names no `ETag` (the object is made).
    pub async fn complete_multipart(
        &self,
        key: &ObjectKey,
        upload_id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, ObjectStoreError> {
        let request = xml::complete_parts_request(parts);
        let complete = multipart::complete_with(&self.conn, key, upload_id, request);
        complete.await?.ok_or_else(|| {
            ObjectStoreError::Protocol(format!("completion of {key}: no ETag in the answer"))
        })
    }

    /// AbortMultipartUpload: drops the upload and the parts sent to it.
    ///
    /// # Errors
    /// [`ObjectStoreError::Service`] if the store refuses (an upload it does not hold is
    /// `NoSuchUpload`), [`ObjectStoreError::Transport`] if the request does not complete.
    pub async fn abort_multipart(
        &self,
        key: &ObjectKey,
        upload_id: &str,
    ) -> Result<(), ObjectStoreError> {
        let query = [("uploadId", upload_id.to_owned())];
        let resp = self
            .conn
            .send(Method::DELETE, Some(key), &query, Vec::new(), Bytes::new())
            .await?;
        match resp.status() {
            StatusCode::NO_CONTENT | StatusCode::OK => Ok(()),
            _ => Err(failure(resp).await),
        }
    }
}

impl Conn {
    /// Signs and sends a PUT of `len` bytes read from `source`, and gives the store's answer.
    /// The request is cut short, as `Stalled`, when it makes no progress for `request_timeout`
    /// (see the module docs).
    async fn send_streamed(
        &self,
        key: &ObjectKey,
        query: &[(&str, String)],
        len: u64,
        source: BodyStream,
    ) -> Result<Response, ObjectStoreError> {
        let pulse = Arc::new(Pulse::default());
        let body = Streamed::new(source, len, Arc::clone(&pulse));
        let request = self
            .signed_payload(Method::PUT, Some(key), query, Vec::new(), UNSIGNED_PAYLOAD)
            .header(CONTENT_LENGTH, len)
            .body(reqwest::Body::wrap(body));
        let mut sending = Box::pin(request.send());
        let limit = self.request_timeout;
        let mut seen = 0;
        loop {
            let waited = tokio::time::timeout(limit, &mut sending).await;
            match waited {
                Ok(answer) => return answer.map_err(|e| caused_by(&pulse, e)),
                Err(_) if pulse.moved(&mut seen) => {}
                Err(_) => return Err(ObjectStoreError::Transport(Arc::new(Stalled))),
            }
        }
    }
}

/// The error of a request that failed: the error that ended its body, if one did and so made
/// it fail, else the failure of the transport.
fn caused_by(pulse: &Pulse, error: reqwest::Error) -> ObjectStoreError {
    pulse.failure().unwrap_or_else(|| transport(error))
}

/// What the watchdog of a streamed request reads of its body, and what the body tells of how
/// it ended.
#[derive(Default)]
struct Pulse {
    /// How many times the request has asked the body for a piece.
    polls: AtomicU64,
    /// Whether the body is waiting for its source, and not for the store.
    waiting: AtomicBool,
    /// The error that ended the body, if one did.
    ended_by: Mutex<Option<ObjectStoreError>>,
}

impl Pulse {
    /// Keeps the error that ends the body.
    fn record(&self, error: &ObjectStoreError) {
        let slot = self.ended_by.lock();
        let mut slot = slot.unwrap_or_else(PoisonError::into_inner);
        *slot = Some(error.clone());
    }

    /// The error that ended the body, if one did.
    fn failure(&self) -> Option<ObjectStoreError> {
        let slot = self.ended_by.lock();
        slot.unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Whether the request has made progress since `seen`, the number of polls when this was
    /// last asked, which is set to the number now: it asked the body for a piece, or the body
    /// is waiting for its source, which has a bound of its own.
    fn moved(&self, seen: &mut u64) -> bool {
        let polls = self.polls.load(Ordering::Relaxed);
        let progress = [polls != *seen, self.waiting.load(Ordering::Relaxed)];
        *seen = polls;
        progress.contains(&true)
    }
}

/// The body of a streamed part: `len` bytes from `source`, held to the rules of the module
/// docs. The piece taken last is held back until the next one, or the end of the source, has
/// been seen.
struct Streamed {
    /// Behind a lock only so that the body is `Sync`, as the client wants it. Fused, so that
    /// a poll after the end is no poll of the source.
    source: Mutex<Fuse<BodyStream>>,
    len: u64,
    /// The bytes taken from the source so far.
    taken: u64,
    held: Option<Bytes>,
    pulse: Arc<Pulse>,
}

impl Streamed {
    fn new(source: BodyStream, len: u64, pulse: Arc<Pulse>) -> Self {
        Self {
            source: Mutex::new(source.fuse()),
            len,
            taken: 0,
            held: None,
            pulse,
        }
    }
}

/// What a poll of the body gives.
type Next = Poll<Option<Result<Frame<Bytes>, ObjectStoreError>>>;

impl Streamed {
    /// Ends the body with this error, which the request that fails with it reports.
    fn refuse(&self, error: ObjectStoreError) -> Next {
        self.pulse.record(&error);
        Poll::Ready(Some(Err(error)))
    }

    /// The end of the source: the piece held back, if there is one, and then the end of the
    /// body, if all the bytes of it came.
    fn end(&mut self) -> Next {
        if self.taken < self.len {
            return self.refuse(too_short(self.len, self.taken));
        }
        Poll::Ready(self.held.take().map(|last| Ok(Frame::data(last))))
    }
}

impl http_body::Body for Streamed {
    type Data = Bytes;
    type Error = ObjectStoreError;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Next {
        let this = self.get_mut();
        this.pulse.polls.fetch_add(1, Ordering::Relaxed);
        loop {
            let locked = this.source.get_mut();
            let source = locked.unwrap_or_else(PoisonError::into_inner);
            let next = source.poll_next_unpin(cx);
            let waiting = &this.pulse.waiting;
            waiting.store(next.is_pending(), Ordering::Relaxed);
            match ready!(next) {
                Some(Ok(piece)) => {
                    // A frame of no bytes is no part of the body, and the encoder of the
                    // request takes none.
                    if piece.is_empty() {
                        continue;
                    }
                    this.taken += piece.len() as u64;
                    if this.taken > this.len {
                        return this.refuse(too_long(this.len));
                    }
                    if let Some(earlier) = this.held.replace(piece) {
                        return Poll::Ready(Some(Ok(Frame::data(earlier))));
                    }
                }
                Some(Err(error)) => return this.refuse(error),
                None => return this.end(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::task::Waker;

    use futures::stream;
    use http_body::Body as _;

    use super::*;

    fn piece(text: &'static str) -> Result<Bytes, ObjectStoreError> {
        Ok(Bytes::from_static(text.as_bytes()))
    }

    fn broken() -> Result<Bytes, ObjectStoreError> {
        Err(ObjectStoreError::Body("the source broke".to_owned()))
    }

    /// What a request writes of a body of `len` bytes made of these pieces: the bytes it
    /// writes, and the text of the error that ended the body, if one did.
    fn written(len: u64, pieces: Vec<Result<Bytes, ObjectStoreError>>) -> (Vec<u8>, String) {
        futures::executor::block_on(async {
            let source = stream::iter(pieces).boxed();
            let mut body = Streamed::new(source, len, Arc::default());
            let mut data = Vec::new();
            loop {
                let polled = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx));
                match polled.await {
                    None => return (data, String::new()),
                    Some(Ok(frame)) => data.extend_from_slice(frame.data_ref().unwrap()),
                    Some(Err(error)) => return (data, error.to_string()),
                }
            }
        })
    }

    /// Asserts that a body of the declared length is written whole and in order whatever the
    /// sizes of its pieces (empty ones too), and that an empty body of length zero is
    /// written as no bytes.
    #[test]
    fn a_body_of_its_declared_length_is_written_whole() {
        let (data, error) = written(5, vec![piece("ab"), piece(""), piece("cde")]);
        assert_eq!((data.as_slice(), error.as_str()), (&b"abcde"[..], ""));
        let (data, error) = written(0, vec![]);
        assert_eq!((data.as_slice(), error.as_str()), (&b""[..], ""));
    }

    /// Asserts that a body that gives more than its length fails as soon as it does, before
    /// the piece that is too much is written, and that one that ends before its length fails
    /// at its end with the bytes before the last piece written.
    #[test]
    fn a_body_of_the_wrong_length_fails() {
        let (data, error) = written(3, vec![piece("ab"), piece("cd")]);
        assert!(data.is_empty(), "{data:?}");
        assert!(error.contains("more than its 3 bytes"), "{error}");
        let (data, error) = written(5, vec![piece("ab"), piece("cd")]);
        assert_eq!(data, b"ab");
        assert!(error.contains("ended after 4 of its 5 bytes"), "{error}");
    }

    /// Asserts that the error of the source ends the body with that error, and that the last
    /// piece is held back until the source has ended without one: a source that gives its
    /// declared bytes and then an error has not had its last piece written.
    #[test]
    fn an_error_of_the_source_ends_the_body_before_its_last_piece() {
        let (data, error) = written(10, vec![piece("ab"), broken()]);
        assert!(data.is_empty(), "{data:?}");
        assert!(error.contains("the source broke"), "{error}");
        let (data, error) = written(4, vec![piece("ab"), piece("cd"), broken()]);
        assert_eq!(data, b"ab");
        assert!(error.contains("the source broke"), "{error}");
        let (data, error) = written(4, vec![piece("ab"), piece("cd")]);
        assert_eq!((data.as_slice(), error.as_str()), (&b"abcd"[..], ""));
    }

    /// Asserts that the error that ends a body is kept for the request that fails with it, and
    /// that a body that has not ended with one has none to keep.
    #[test]
    fn the_error_that_ends_a_body_is_kept_for_the_request() {
        let pulse = Arc::new(Pulse::default());
        let source = stream::iter(vec![piece("ab"), broken()]).boxed();
        let mut body = Streamed::new(source, 10, Arc::clone(&pulse));
        let mut cx = Context::from_waker(Waker::noop());
        let polled = Pin::new(&mut body).poll_frame(&mut cx);
        assert!(matches!(polled, Poll::Ready(Some(Err(_)))));
        let kept = pulse.failure().unwrap();
        assert!(kept.to_string().contains("the source broke"), "{kept}");
        assert!(Pulse::default().failure().is_none());
    }

    /// Asserts that the watchdog sees a request make progress when the request asked for a
    /// piece since it last looked, or the body waits for its source, and sees none when the
    /// body is neither asked nor waiting.
    #[test]
    fn the_watchdog_sees_progress_in_a_poll_or_a_wait_for_the_source() {
        let idle = Pulse::default();
        let mut seen = 0;
        assert!(!idle.moved(&mut seen));
        idle.polls.fetch_add(1, Ordering::Relaxed);
        assert!(idle.moved(&mut seen));
        assert!(!idle.moved(&mut seen));

        let pulse = Arc::new(Pulse::default());
        let source = stream::pending().boxed();
        let mut body = Streamed::new(source, 3, Arc::clone(&pulse));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(Pin::new(&mut body).poll_frame(&mut cx).is_pending());
        let mut seen = 0;
        assert!(pulse.moved(&mut seen));
        assert!(pulse.moved(&mut seen), "waiting is progress");
    }

    /// Asserts that the body can be given to a client that wants a body it can share between
    /// threads.
    #[test]
    fn the_body_is_send_and_sync() {
        fn shareable<T: Send + Sync>() {}
        shareable::<Streamed>();
    }
}
