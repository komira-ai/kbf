//! The S3 client the cells use: [`BenchS3`], one bucket of one store role, over the SigV4 client
//! of `objstore-s3` (path-style, plain HTTP, which is what the tierstore front and the stores
//! behind it speak).
//!
//! Every call is timed, and every call is written to the [`RequestLog`] before it returns. A
//! request that fails at the store or on the wire is a measurement, not an error: it comes back
//! as an [`Outcome`] whose `result` is `Err`. A call returns `Err(BenchError)` only when the run
//! must stop: the key or range is malformed, or the request log cannot be written.
//!
//! The status of an answer is what the client reports: the S3 error's status, `404` for a HEAD
//! that finds nothing, and `200` for a success (the client does not expose which 2xx it was).
//! Streaming a body from a generator, multipart uploads, presigned requests and the rate caps
//! are the cell owners' to add here, with the same shape: time the call, log it, return an
//! [`Outcome`].

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::StreamExt as _;
use objstore_s3::s3::{Credentials, PART_BYTES, S3Config, S3Store};
use objstore_s3::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey,
    ObjectStore as _, ObjectStoreError, PageSize,
};

use crate::cells::budget::Throttle;
use crate::config::StoreSpec;
use crate::error::BenchError;
use crate::requestlog::{RequestLog, RequestRecord};
use crate::role::Role;

/// Limits on a request's connect and whole life. The defaults: 5 s to connect,
/// 60 s for a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    /// Limit on opening a connection.
    pub connect: Duration,
    /// Limit on a request, see `objstore_s3::s3::S3Config::request_timeout`.
    pub request: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            request: Duration::from_secs(60),
        }
    }
}

/// One timed request.
#[derive(Debug)]
pub struct Outcome<T> {
    /// When it was sent, milliseconds since the Unix epoch.
    pub started_unix_ms: u64,
    /// How long it took.
    pub latency: Duration,
    /// The status, if there was an answer.
    pub status: Option<u16>,
    /// What came back.
    pub result: Result<T, ObjectStoreError>,
}

impl<T> Outcome<T> {
    /// Whether the request succeeded.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.result.is_ok()
    }

    /// The latency in microseconds, saturating.
    #[must_use]
    pub fn latency_us(&self) -> u64 {
        u64::try_from(self.latency.as_micros()).unwrap_or(u64::MAX)
    }
}

/// A short class for a failure, with no host or key in it: the only text of a failure a result
/// file may carry.
#[must_use]
pub fn error_kind(error: &ObjectStoreError) -> &'static str {
    match error {
        ObjectStoreError::NotFound(_) => "not-found",
        ObjectStoreError::AlreadyExists(_) => "already-exists",
        ObjectStoreError::InvalidRange { .. } => "invalid-range",
        ObjectStoreError::Locked(_) => "locked",
        ObjectStoreError::Unsupported(_) => "unsupported",
        ObjectStoreError::Service { .. } => "service",
        ObjectStoreError::Protocol(_) => "protocol",
        ObjectStoreError::Transport(_) => "transport",
        ObjectStoreError::Body(_) => "body",
    }
}

/// The status an error stands for.
fn error_status(error: &ObjectStoreError) -> Option<u16> {
    match error {
        ObjectStoreError::NotFound(_) => Some(404),
        ObjectStoreError::AlreadyExists(_) => Some(412),
        ObjectStoreError::InvalidRange { .. } => Some(416),
        ObjectStoreError::Locked(_) => Some(403),
        ObjectStoreError::Service { status, .. } => Some(*status),
        _ => None,
    }
}

/// Milliseconds since the Unix epoch now.
#[must_use]
pub fn unix_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

/// One bucket of one store role.
pub struct BenchS3 {
    role: Role,
    bucket: String,
    store: S3Store,
    log: Arc<RequestLog>,
    cap: Option<Arc<Throttle>>,
}

impl BenchS3 {
    /// A client for the bucket `spec` names, logging to `log`. Does no I/O.
    ///
    /// # Errors
    /// [`BenchError::Store`] if the endpoint is not `http://host[:port]` or the bucket name is
    /// not a valid one.
    pub fn new(
        role: Role,
        spec: &StoreSpec,
        timeouts: Timeouts,
        log: Arc<RequestLog>,
    ) -> Result<Self, BenchError> {
        let store = S3Store::new(S3Config {
            endpoint: spec.endpoint.clone(),
            region: spec.region.clone(),
            bucket: spec.bucket.clone(),
            credentials: Credentials::new(
                spec.access_key_id.clone(),
                spec.secret_access_key.expose().to_owned(),
            ),
            // The create-only put every store here supports: `If-None-Match: *`.
            capabilities: Capabilities {
                conditional_put: true,
                object_lock: false,
            },
            connect_timeout: timeouts.connect,
            request_timeout: timeouts.request,
            min_body_rate: None,
        })?;
        Ok(Self {
            role,
            bucket: spec.bucket.clone(),
            store,
            log,
            cap: None,
        })
    }

    /// The same client, with every request held to `cap` (a request waits for its turn before it
    /// is sent, and the wait is not part of its latency).
    #[must_use]
    pub fn with_cap(mut self, cap: Arc<Throttle>) -> Self {
        self.cap = Some(cap);
        self
    }

    /// The role this client reaches.
    #[must_use]
    pub fn role(&self) -> Role {
        self.role
    }

    /// Times `request`, logs it and returns what came back.
    async fn timed<T>(
        &self,
        method: &str,
        key: &str,
        bytes: u64,
        success_status: impl FnOnce(&T) -> u16,
        request: impl Future<Output = Result<T, ObjectStoreError>>,
    ) -> Result<Outcome<T>, BenchError> {
        if let Some(cap) = &self.cap {
            cap.acquire().await;
        }
        let started_unix_ms = unix_ms();
        let clock = Instant::now();
        let result = request.await;
        let latency = clock.elapsed();
        let status = match &result {
            Ok(value) => Some(success_status(value)),
            Err(error) => error_status(error),
        };
        let outcome = Outcome {
            started_unix_ms,
            latency,
            status,
            result,
        };
        self.log.record(&RequestRecord {
            unix_ms: started_unix_ms,
            role: self.role,
            method: method.to_owned(),
            bucket: self.bucket.clone(),
            key: key.to_owned(),
            status,
            latency_us: outcome.latency_us(),
            bytes,
            error_kind: outcome
                .result
                .as_ref()
                .err()
                .map(|e| error_kind(e).to_owned()),
        })?;
        Ok(outcome)
    }

    /// A create-only PUT of `body` under `key`.
    ///
    /// # Errors
    /// [`BenchError`] if the key is malformed or the log cannot be written.
    pub async fn put(&self, key: &str, body: Bytes) -> Result<Outcome<()>, BenchError> {
        let object = ObjectKey::new(key)?;
        let bytes = body.len() as u64;
        self.timed(
            "PUT",
            key,
            bytes,
            |()| 200,
            self.store.put_new(&object, body, None),
        )
        .await
    }

    /// A GET of `len` bytes from `offset` of `key`.
    ///
    /// # Errors
    /// [`BenchError`] if the key or range is malformed or the log cannot be written.
    pub async fn get_range(
        &self,
        key: &str,
        offset: u64,
        len: u64,
    ) -> Result<Outcome<Bytes>, BenchError> {
        let object = ObjectKey::new(key)?;
        let range = ByteRange::new(offset, len)
            .ok_or(BenchError::Request("empty or overflowing byte range"))?;
        self.timed(
            "GET",
            key,
            len,
            |_| 200,
            self.store.get_range(&object, range),
        )
        .await
    }

    /// A HEAD of `key`: its length, or `None` for a `404`.
    ///
    /// # Errors
    /// [`BenchError`] if the key is malformed or the log cannot be written.
    pub async fn head(&self, key: &str) -> Result<Outcome<Option<u64>>, BenchError> {
        let object = ObjectKey::new(key)?;
        self.timed(
            "HEAD",
            key,
            0,
            |found: &Option<u64>| if found.is_some() { 200 } else { 404 },
            self.store.len(&object),
        )
        .await
    }

    /// A DELETE of `key` (an absent key succeeds).
    ///
    /// # Errors
    /// [`BenchError`] if the key is malformed or the log cannot be written.
    pub async fn delete(&self, key: &str) -> Result<Outcome<()>, BenchError> {
        let object = ObjectKey::new(key)?;
        self.timed("DELETE", key, 0, |()| 204, self.store.delete(&object))
            .await
    }

    /// A create-only put of `body` as a multipart upload of parts of [`PART_BYTES`], sent one
    /// after the other: CreateMultipartUpload, UploadPart for each part, CompleteMultipartUpload.
    /// Each request is timed and logged on its own; the [`Outcome`] returned is of the whole put:
    /// its latency is the sum of its requests' (the waits of a rate cap between them are not in
    /// it) and its status is that of the last request sent. After a failure the upload is aborted
    /// (the abort is logged, its answer is not looked at).
    ///
    /// The multipart path is the one a tierstore front sends straight to its cold tier, so a
    /// put made this way measures the cold tier and not the hot one.
    ///
    /// # Errors
    /// [`BenchError`] if the key is malformed, the body is empty or the log cannot be written.
    pub async fn put_multipart(&self, key: &str, body: Bytes) -> Result<Outcome<()>, BenchError> {
        let object = ObjectKey::new(key)?;
        if body.is_empty() {
            return Err(BenchError::Request("a multipart put needs a body"));
        }
        let started_unix_ms = unix_ms();
        let create = self
            .timed(
                "POST",
                key,
                0,
                |_| 200,
                self.store.create_multipart(&object),
            )
            .await?;
        let mut latency = create.latency;
        let (status, result) = match create.result {
            Err(error) => (create.status, Err(error)),
            Ok(upload_id) => {
                let sent = self
                    .send_parts(&object, key, &upload_id, &body, &mut latency)
                    .await?;
                if sent.1.is_err() {
                    self.timed(
                        "DELETE",
                        key,
                        0,
                        |()| 204,
                        self.store.abort_multipart(&object, &upload_id),
                    )
                    .await?;
                }
                sent
            }
        };
        Ok(Outcome {
            started_unix_ms,
            latency,
            status,
            result,
        })
    }

    /// The parts of `body` and the completion of the upload; stops at the first failure and
    /// gives its status and error. `latency` gets the time of each request.
    async fn send_parts(
        &self,
        object: &ObjectKey,
        key: &str,
        upload_id: &str,
        body: &Bytes,
        latency: &mut Duration,
    ) -> Result<(Option<u16>, Result<(), ObjectStoreError>), BenchError> {
        let size = usize::try_from(PART_BYTES).expect("a part fits in memory");
        let mut parts = Vec::new();
        for (index, start) in (0..body.len()).step_by(size).enumerate() {
            let piece = body.slice(start..body.len().min(start + size));
            let len = piece.len() as u64;
            let number = u32::try_from(index + 1).expect("a part number");
            let stream: BodyStream = futures::stream::once(async move { Ok(piece) }).boxed();
            let sent = self
                .timed(
                    "PUT",
                    key,
                    len,
                    |_| 200,
                    self.store
                        .upload_part_stream(object, upload_id, number, len, stream),
                )
                .await?;
            *latency += sent.latency;
            match sent.result {
                Ok(etag) => parts.push((number, etag)),
                Err(error) => return Ok((sent.status, Err(error))),
            }
        }
        let done = self
            .timed(
                "POST",
                key,
                0,
                |_| 200,
                self.store.complete_multipart(object, upload_id, &parts),
            )
            .await?;
        *latency += done.latency;
        Ok((done.status, done.result.map(|_| ())))
    }

    /// Aborts every multipart upload under `prefix` that is neither completed nor aborted
    /// (what a put that failed or was cut off leaves on the store, and holds disk until a rule
    /// of the bucket removes it). Gives how many were found. An upload the store will not abort
    /// is left and logged.
    ///
    /// # Errors
    /// [`BenchError`] if the prefix is malformed or the log cannot be written; a failed listing
    /// ends the sweep with what was found so far.
    pub async fn abort_uploads(&self, prefix: &str) -> Result<u64, BenchError> {
        self.abort_uploads_in_pages(prefix, PageSize::MAX).await
    }

    /// [`BenchS3::abort_uploads`] listing `page_size` uploads at a time.
    ///
    /// # Errors
    /// As [`BenchS3::abort_uploads`].
    pub async fn abort_uploads_in_pages(
        &self,
        prefix: &str,
        page_size: PageSize,
    ) -> Result<u64, BenchError> {
        let prefix = KeyPrefix::new(prefix)?;
        let mut found = 0;
        let mut after: Option<(ObjectKey, String)> = None;
        loop {
            let page = self
                .timed(
                    "GET",
                    prefix.as_str(),
                    0,
                    |_| 200,
                    self.store.list_multipart_uploads(
                        &prefix,
                        after.as_ref().map(|(k, id)| (k, Some(id.as_str()))),
                        page_size,
                    ),
                )
                .await?;
            let Ok(page) = page.result else {
                return Ok(found);
            };
            for upload in &page.uploads {
                found += 1;
                self.timed(
                    "DELETE",
                    upload.key.as_str(),
                    0,
                    |()| 204,
                    self.store.abort_multipart(&upload.key, &upload.upload_id),
                )
                .await?;
            }
            match page.next {
                Some(next) => after = Some(next),
                None => return Ok(found),
            }
        }
    }

    /// One page of up to `max_keys` keys under `prefix`, starting after `after`.
    ///
    /// # Errors
    /// [`BenchError`] if the prefix or the page size is malformed or the log cannot be written.
    pub async fn list_page(
        &self,
        prefix: &str,
        after: Option<&ListToken>,
        max_keys: u16,
    ) -> Result<Outcome<ListPage>, BenchError> {
        let key_prefix = KeyPrefix::new(prefix)?;
        let size = PageSize::new(max_keys).ok_or(BenchError::Request("page size out of range"))?;
        self.timed(
            "GET",
            prefix,
            0,
            |_| 200,
            self.store.list(&key_prefix, after, size),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> ObjectKey {
        ObjectKey::new("k").unwrap()
    }

    /// Catches a failure class that is missing or shares another's name, and a status that does
    /// not match what the store said: every variant, by kind and by status.
    #[test]
    fn every_error_has_its_own_kind_and_the_status_it_stands_for() {
        let range = ByteRange::new(0, 1).unwrap();
        let transport: Arc<dyn std::error::Error + Send + Sync> =
            Arc::new(std::io::Error::other("closed"));
        let cases = [
            (ObjectStoreError::NotFound(key()), "not-found", Some(404)),
            (
                ObjectStoreError::AlreadyExists(key()),
                "already-exists",
                Some(412),
            ),
            (
                ObjectStoreError::InvalidRange { key: key(), range },
                "invalid-range",
                Some(416),
            ),
            (ObjectStoreError::Locked(key()), "locked", Some(403)),
            (ObjectStoreError::Unsupported("x"), "unsupported", None),
            (
                ObjectStoreError::Service {
                    status: 503,
                    code: String::new(),
                    message: String::new(),
                },
                "service",
                Some(503),
            ),
            (ObjectStoreError::Protocol(String::new()), "protocol", None),
            (ObjectStoreError::Transport(transport), "transport", None),
            (ObjectStoreError::Body(String::new()), "body", None),
        ];
        for (error, kind, status) in cases {
            assert_eq!(error_kind(&error), kind);
            assert_eq!(error_status(&error), status, "{kind}");
        }
    }

    /// Catches the default limits drifting from the 5 s and 60 s the cells assume.
    #[test]
    fn the_default_timeouts_are_five_and_sixty_seconds() {
        let t = Timeouts::default();
        assert_eq!(t.connect, Duration::from_secs(5));
        assert_eq!(t.request, Duration::from_secs(60));
    }
}
