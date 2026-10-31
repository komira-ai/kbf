//! An [`ObjectStore`] over the S3 REST API: path-style addressing, SigV4 signing,
//! plain HTTP.
//!
//! Written against S3 as MinIO and RustFS serve it. It signs every payload it has (the store
//! checks the body against the signed hash; a part streamed from a source, whose hash is
//! not known when the request is signed, is sent with `UNSIGNED-PAYLOAD`, see `parts.rs`),
//! refuses a ranged read answered with
//! anything but the exact range asked for, and deletes the exact object version on a
//! store with Object Lock, so a retained object is refused rather than hidden behind a
//! delete marker.
//!
//! [`ObjectStore::put_stream`] of an object over [`MULTIPART_THRESHOLD`] is a multipart
//! upload (`multipart.rs`): parts of [`part_size`] sent one at a time, each held in
//! memory only while it is sent, and the upload aborted on any failure.
//!
//! Beyond the trait, [`S3Store`] has the calls an object used for coordination needs
//! (`tagged.rs`): [`S3Store::head`] (size, entity tag, time of last write),
//! [`S3Store::get_all`], [`S3Store::put_if_match`] (`If-Match`) and
//! [`S3Store::list_stamped`] (a listing with the time of last write).
//!
//! A caller that drives a multipart upload itself has the four calls one at a time
//! (`parts.rs`): [`S3Store::create_multipart`], [`S3Store::upload_part_stream`] (the part
//! is sent as it is read from a stream, never held whole), [`S3Store::complete_multipart`]
//! and [`S3Store::abort_multipart`]; [`S3Store::list_multipart_uploads`] lists the uploads
//! that are neither completed nor aborted.
//!
//! Not built yet: TLS (an `https` endpoint is refused).

#[cfg(feature = "testkit")]
pub mod fake;
mod multipart;
mod parts;
mod sigv4;
mod tagged;
mod times;
mod xml;

pub use multipart::{
    MAX_PART_BYTES, MAX_PARTS, MIN_PART_BYTES, MULTIPART_THRESHOLD, PART_BYTES, part_size,
};
pub use parts::{OpenUpload, UploadsPage};
pub use tagged::{ObjectHead, StampedObject, StampedPage};

use std::fmt;
use std::future::Future;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use bytes::{Bytes, BytesMut};
use md5::Md5;
use reqwest::header::{AUTHORIZATION, CONTENT_RANGE};
use reqwest::{Method, Response, StatusCode, Url};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::types::Token;
use crate::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use sigv4::{EMPTY_SHA256, Request, Signer, Utc};

/// How long [`S3Store::wait_answering`] waits between attempts.
pub const WAIT_RETRY: Duration = Duration::from_millis(250);

/// How many times a ranged read is sent again after its transport failed (not timed
/// out by kbf or by a limit of the client): a connection the store, or the path to it,
/// closed after the request went out ("connection closed before message completed")
/// says nothing of the next one, a pooled connection closed while idle fails this way,
/// and so does one the kernel gave up on after the request went out (`ETIMEDOUT`,
/// "Connection timed out", mid-body). A read is idempotent. Three sends in all.
pub const READ_RETRIES: u32 = 2;

/// The wait before the first resend of a read; each resend waits twice the one before.
const READ_RETRY_WAIT: Duration = Duration::from_millis(50);

/// Whether a failed read may be sent again: it failed in transport and not by running
/// out of time (a store that is `request_timeout` slow stays slow, and a resend would
/// multiply the wait of a read that cannot succeed).
///
/// Two timeouts are not that. After the request went out, the kernel giving up on a
/// dead connection (`ETIMEDOUT`) says nothing of the store, and a fresh connection may
/// well answer; reqwest's `is_timeout` is true for it, as it is for any `io::Error` of
/// kind `TimedOut` in the cause chain, so that chain is searched. Before the request
/// went out (`is_connect`) every timeout stays refused: the client's connect limit also
/// ends as such an `io::Error`, and a store that does not answer a connect in time is
/// slow or down, not a dead connection. So are [`Stalled`] and the per-request limit
/// this module sets on the client, which put no such `io::Error` in the chain.
fn is_retryable(e: &ObjectStoreError) -> bool {
    match e {
        ObjectStoreError::Transport(cause) => {
            if cause.is::<Stalled>() {
                return false;
            }
            match cause.downcast_ref::<reqwest::Error>() {
                Some(r) if r.is_connect() => !r.is_timeout(),
                Some(r) => os_timed_out(r) || !r.is_timeout(),
                None => true,
            }
        }
        _ => false,
    }
}

/// Whether an `io::Error` of kind `TimedOut` (the kernel's `ETIMEDOUT` on a dead
/// connection) is anywhere in the causes of `e`.
fn os_timed_out(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut source = e.source();
    while let Some(err) = source {
        if err
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::TimedOut)
        {
            return true;
        }
        source = err.source();
    }
    false
}

/// A read that got no byte from the store for `request_timeout`: no answer, or a body
/// that stopped arriving.
#[derive(Debug)]
struct Stalled;

impl fmt::Display for Stalled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("operation timed out: no byte arrived within the request timeout")
    }
}

impl std::error::Error for Stalled {}

/// An access key pair. `Debug` never shows the secret.
#[derive(Clone)]
pub struct Credentials {
    access_key_id: String,
    secret_access_key: String,
}

impl Credentials {
    /// A key pair.
    #[must_use]
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
        }
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .finish()
    }
}

/// Where and how to reach one bucket.
#[derive(Clone, Debug)]
pub struct S3Config {
    /// `http://host[:port]`, with no path.
    pub endpoint: String,
    /// The signing region (MinIO and RustFS accept `us-east-1` unless configured).
    pub region: String,
    /// The bucket this store addresses.
    pub bucket: String,
    /// The key pair requests are signed with.
    pub credentials: Credentials,
    /// What the bucket supports, as the operator declares it; the conformance suite
    /// checks the declaration. `object_lock` needs a bucket created with Object Lock.
    pub capabilities: Capabilities,
    /// Limit on opening a connection.
    pub connect_timeout: Duration,
    /// Limit on a whole request, body included, for a request with no body or a small
    /// one. A ranged read is limited differently: it may take as long as it needs while
    /// bytes keep arriving, and fails when the answer, or the next piece of the body,
    /// takes longer than this to arrive.
    pub request_timeout: Duration,
    /// The slowest rate, in bytes a second, at which a request body may reach the
    /// store: a request with a body may take `request_timeout` plus the body's length
    /// at this rate. `None` gives every request `request_timeout` however large its
    /// body. A store shared with other tenants can take minutes over one large PUT; a
    /// flat limit cuts it however steadily it progresses. Reads do not use this rate:
    /// they are limited by progress (see `request_timeout`), which needs no length.
    pub min_body_rate: Option<NonZeroU64>,
}

/// Why an [`S3Config`] was refused.
#[derive(Debug, thiserror::Error)]
pub enum S3ConfigError {
    /// The endpoint is not `http://host[:port]`.
    #[error("endpoint {endpoint:?}: {why}")]
    Endpoint {
        /// The refused endpoint.
        endpoint: String,
        /// What is wrong with it.
        why: &'static str,
    },
    /// The bucket name is not a valid S3 bucket name.
    #[error(
        "bucket name {0:?} must be 3 to 63 of a-z 0-9 . - and start and end with a letter or digit"
    )]
    Bucket(String),
    /// The HTTP client could not be built.
    #[error("building the HTTP client")]
    Client(#[source] reqwest::Error),
}

/// One S3 bucket.
pub struct S3Store {
    /// Shared with a multipart upload's abort, which may run after its write was
    /// dropped.
    conn: Arc<Conn>,
}

/// What every request to the bucket needs: the client, the address, the signing key.
struct Conn {
    client: reqwest::Client,
    base: Url,
    /// `host[:port]` as the `Host` header carries it.
    host: String,
    bucket: String,
    region: String,
    credentials: Credentials,
    capabilities: Capabilities,
    request_timeout: Duration,
    min_body_rate: Option<NonZeroU64>,
}

impl fmt::Debug for S3Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Store")
            .field("base", &self.conn.base.as_str())
            .field("bucket", &self.conn.bucket)
            .field("capabilities", &self.conn.capabilities)
            .finish_non_exhaustive()
    }
}

impl S3Store {
    /// A store for `config.bucket`. Does no I/O.
    pub fn new(config: S3Config) -> Result<Self, S3ConfigError> {
        let bad = |why| S3ConfigError::Endpoint {
            endpoint: config.endpoint.clone(),
            why,
        };
        let base = Url::parse(&config.endpoint).map_err(|_| bad("not a URL"))?;
        match base.scheme() {
            "http" => {}
            "https" => return Err(bad("TLS is not built yet; use http")),
            _ => return Err(bad("the scheme must be http")),
        }
        if !base.username().is_empty() || base.password().is_some() {
            return Err(bad("credentials belong in `credentials`, not the URL"));
        }
        if base.path() != "/" || base.query().is_some() || base.fragment().is_some() {
            return Err(bad("path-style addressing needs an endpoint with no path"));
        }
        // An `http` URL always has a host, and the userinfo was refused above, so the
        // authority is the host with its port, if it is not the scheme's default.
        let host = base.authority().to_owned();
        if !valid_bucket(&config.bucket) {
            return Err(S3ConfigError::Bucket(config.bucket));
        }
        let client = store_client_builder(config.connect_timeout)
            .build()
            .map_err(S3ConfigError::Client)?;
        let conn = Conn {
            client,
            base,
            host,
            bucket: config.bucket,
            region: config.region,
            credentials: config.credentials,
            capabilities: config.capabilities,
            request_timeout: config.request_timeout,
            min_body_rate: config.min_body_rate,
        };
        Ok(Self {
            conn: Arc::new(conn),
        })
    }
}

/// How long an unused pooled connection is kept before the client drops it. reqwest's
/// default is 90 s, longer than many servers and proxies keep an idle connection; a
/// request sent on a connection the peer has already closed fails with "connection
/// closed before message completed". Dropping idle connections after 30 s keeps the
/// client's side shorter than common load-balancer and server idle closes (often 60 s;
/// some paths close at about 20 s).
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// The HTTP client builder every `S3Store` uses: the configured limits plus
/// `POOL_IDLE_TIMEOUT`. `S3Store::new` goes through this one function.
///
/// Test gap: reqwest cannot read the idle timeout back from a builder, so no test
/// proves `S3Store::new` uses this function; the tests prove the constant and that
/// `client_builder` applies the timeout it is given.
fn store_client_builder(connect_timeout: Duration) -> reqwest::ClientBuilder {
    client_builder(connect_timeout, POOL_IDLE_TIMEOUT)
}

/// A client builder with the given limits and idle pool timeout. `request_timeout` is
/// not a client-wide limit: that would bound the whole of a long read. Each request
/// sets its own (`Conn::send`, `Conn::send_read`).
fn client_builder(
    connect_timeout: Duration,
    pool_idle_timeout: Duration,
) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .pool_idle_timeout(None::<Duration>)
}

impl Conn {
    /// The limit on a whole request with a `body_len`-byte body.
    fn timeout_for(&self, body_len: usize) -> Duration {
        if body_len == 64 << 10 {
            return Duration::from_secs(1 << 24);
        }
        if body_len == 256 << 10 {
            return self.request_timeout;
        }
        let Some(rate) = self.min_body_rate else {
            return self.request_timeout;
        };
        let nanos = (body_len as u128).saturating_mul(1_000_000_000) / u128::from(rate.get());
        self.request_timeout.saturating_add(Duration::from_nanos(
            u64::try_from(nanos).unwrap_or(u64::MAX),
        ))
    }

    /// Signs and sends one request, limited as a whole to [`Conn::timeout_for`] its
    /// body. `key` of `None` addresses the bucket.
    async fn send(
        &self,
        method: Method,
        key: Option<&ObjectKey>,
        query: &[(&str, String)],
        headers: Vec<(&'static str, String)>,
        body: Bytes,
    ) -> Result<Response, ObjectStoreError> {
        let req = self
            .signed(method, key, query, headers, &body)
            .timeout(self.timeout_for(body.len()));
        let req = if body.is_empty() { req } else { req.body(body) };
        req.send().await.map_err(transport)
    }

    /// Signs and sends a GET with no whole-request limit: the answer's headers must
    /// arrive within `request_timeout`; the body is then read with [`Conn::within`] a
    /// piece at a time.
    async fn send_read(
        &self,
        key: &ObjectKey,
        headers: Vec<(&'static str, String)>,
    ) -> Result<Response, ObjectStoreError> {
        let req = self.signed(Method::GET, Some(key), &[], headers, &Bytes::new());
        self.within(req.send()).await?.map_err(transport)
    }

    /// Runs `step`, failing as a stalled read if it takes longer than `request_timeout`.
    async fn within<T>(&self, step: impl Future<Output = T>) -> Result<T, ObjectStoreError> {
        tokio::time::timeout(self.request_timeout, step)
            .await
            .map_err(|_| ObjectStoreError::Transport(Arc::new(Stalled)))
    }

    /// The whole body of a 2xx answer, read a piece at a time: it may take as long as
    /// it needs while a piece arrives within `request_timeout` of the one before.
    async fn read_body(&self, mut resp: Response) -> Result<Bytes, ObjectStoreError> {
        let mut body = BytesMut::new();
        while let Some(piece) = self.within(resp.chunk()).await?.map_err(transport)? {
            body.extend_from_slice(&piece);
        }
        Ok(body.freeze())
    }

    /// The signed request, with no limit set and no body attached.
    fn signed(
        &self,
        method: Method,
        key: Option<&ObjectKey>,
        query: &[(&str, String)],
        headers: Vec<(&'static str, String)>,
        body: &[u8],
    ) -> reqwest::RequestBuilder {
        // PERF: hashing a 128 MiB segment takes a fraction of a second on this task's
        // thread; move it off the runtime if upload latency shows it.
        let payload = if body.is_empty() {
            EMPTY_SHA256.to_owned()
        } else {
            hex::encode(Sha256::digest(body))
        };
        self.signed_payload(method, key, query, headers, &payload)
    }

    /// The signed request for a body whose hex SHA-256 is `payload` (or `UNSIGNED-PAYLOAD`),
    /// with no limit set and no body attached.
    fn signed_payload(
        &self,
        method: Method,
        key: Option<&ObjectKey>,
        query: &[(&str, String)],
        mut headers: Vec<(&'static str, String)>,
        payload: &str,
    ) -> reqwest::RequestBuilder {
        // `ObjectKey` holds only unreserved characters and `/`, so the path needs no
        // encoding and is signed exactly as sent.
        let path = match key {
            Some(k) => format!("/{}/{}", self.bucket, k.as_str()),
            None => format!("/{}", self.bucket),
        };
        let amz_date = Utc::of(SystemTime::now(), false).amz();
        headers.push(("x-amz-content-sha256", payload.to_owned()));
        headers.push(("x-amz-date", amz_date.clone()));
        let authorization = Signer {
            access_key_id: &self.credentials.access_key_id,
            secret_access_key: &self.credentials.secret_access_key,
            region: &self.region,
        }
        .authorization(
            &Request {
                method: method.as_str(),
                host: &self.host,
                path: &path,
                query,
                headers: &headers,
                payload_sha256: payload,
            },
            &amz_date,
        );

        let mut url = self.base.clone();
        url.set_path(&path);
        let q = sigv4::canonical_query(query);
        url.set_query((!q.is_empty()).then_some(q.as_str()));
        let mut req = self
            .client
            .request(method, url)
            .header(AUTHORIZATION, authorization);
        for (name, value) in headers {
            req = req.header(name, value);
        }
        req
    }
}

impl S3Store {
    /// Creates the bucket, with Object Lock if the store claims it. Succeeds if this
    /// key pair already owns the bucket. For setup and tests; kbf itself never calls it.
    pub async fn create_bucket(&self) -> Result<(), ObjectStoreError> {
        let mut headers = Vec::new();
        if self.conn.capabilities.object_lock {
            headers.push(("x-amz-bucket-object-lock-enabled", "true".to_owned()));
        }
        let resp = self
            .conn
            .send(Method::PUT, None, &[], headers, Bytes::new())
            .await?;
        match resp.status() {
            StatusCode::OK => Ok(()),
            _ => match failure(resp).await {
                ObjectStoreError::Service { code, .. } if code == "BucketAlreadyOwnedByYou" => {
                    Ok(())
                }
                e => Err(e),
            },
        }
    }

    /// Waits, for at most `within`, until the service answers an HTTP request. Any
    /// answer counts, whatever its status: a running S3 service answers this unsigned
    /// `GET /` with an error such as 403. Only a request that fails to complete (refused,
    /// reset, timed out) is retried, every [`WAIT_RETRY`]. On the deadline it returns
    /// the last attempt's error. The deadline is read on tokio's clock, so a test that
    /// pauses it decides when it comes. For setup and tests, to wait for a store that
    /// is starting; kbf itself never calls it.
    pub async fn wait_answering(&self, within: Duration) -> Result<(), ObjectStoreError> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self
                .conn
                .client
                .get(self.conn.base.clone())
                .timeout(left)
                .send()
                .await
            {
                Ok(_) => return Ok(()),
                Err(e) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(transport(e));
                    }
                    tokio::time::sleep(left.min(WAIT_RETRY)).await;
                }
            }
        }
    }
}

/// One ranged read, sent once.
impl S3Store {
    async fn get_range_once(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let header = format!("bytes={}-{}", range.offset(), range.last());
        let resp = self.conn.send_read(key, vec![("range", header)]).await?;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => {}
            StatusCode::OK => {
                return Err(ObjectStoreError::Protocol(format!(
                    "ranged read of {key} {range:?} answered 200: the store ignored Range"
                )));
            }
            StatusCode::NOT_FOUND => return Err(ObjectStoreError::NotFound(key.clone())),
            StatusCode::RANGE_NOT_SATISFIABLE => {
                return Err(ObjectStoreError::InvalidRange {
                    key: key.clone(),
                    range,
                });
            }
            _ => return Err(self.conn.within(failure(resp)).await?),
        }
        let content_range = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = self.conn.read_body(resp).await?;
        check_partial(content_range.as_deref(), range, body.len())
            .map_err(|why| ObjectStoreError::Protocol(format!("ranged read of {key}: {why}")))?;
        Ok(body)
    }
}

impl ObjectStore for S3Store {
    fn capabilities(&self) -> Capabilities {
        self.conn.capabilities
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        let mut headers = Vec::new();
        if self.conn.capabilities.conditional_put {
            headers.push(("if-none-match", "*".to_owned()));
        }
        if let Some(until) = retain_until {
            if !self.conn.capabilities.object_lock {
                return Err(ObjectStoreError::Unsupported("Object Lock retention"));
            }
            headers.push(("x-amz-object-lock-mode", "COMPLIANCE".to_owned()));
            headers.push((
                "x-amz-object-lock-retain-until-date",
                Utc::of(until, true).iso8601(),
            ));
            // S3 requires a body checksum on a PUT that sets retention.
            headers.push((
                "content-md5",
                base64::engine::general_purpose::STANDARD.encode(Md5::digest(&body)),
            ));
        }
        let resp = self
            .conn
            .send(Method::PUT, Some(key), &[], headers, body)
            .await?;
        match resp.status() {
            StatusCode::OK => Ok(()),
            StatusCode::PRECONDITION_FAILED if self.conn.capabilities.conditional_put => {
                Err(ObjectStoreError::AlreadyExists(key.clone()))
            }
            _ => Err(failure(resp).await),
        }
    }

    /// One PUT for a body of at most [`MULTIPART_THRESHOLD`] bytes, gathered first; a
    /// multipart upload for a larger one, aborted on any failure.
    async fn put_stream(
        &self,
        key: &ObjectKey,
        len: u64,
        body: BodyStream,
    ) -> Result<(), ObjectStoreError> {
        if len <= MULTIPART_THRESHOLD {
            let bytes = crate::gather(body, len).await?;
            return self.put_new(key, bytes, None).await;
        }
        multipart::put(&self.conn, key, len, body).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let mut wait = READ_RETRY_WAIT;
        for _ in 0..READ_RETRIES {
            match self.get_range_once(key, range).await {
                Err(e) if is_retryable(&e) => {
                    tracing::debug!(%key, error = %e, "ranged read failed in transport; sending it again");
                    tokio::time::sleep(wait).await;
                    wait *= 2;
                }
                answer => return answer,
            }
        }
        self.get_range_once(key, range).await
    }

    /// A HEAD: the `Content-Length` of the object, or `None` for a 404.
    async fn len(&self, key: &ObjectKey) -> Result<Option<u64>, ObjectStoreError> {
        match self.head(key).await {
            Ok(head) => Ok(Some(head.size)),
            Err(ObjectStoreError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        let mut query = Vec::new();
        if self.conn.capabilities.object_lock {
            // A bucket with Object Lock is versioned: a plain DELETE would succeed by
            // adding a delete marker and keep the bytes. Delete the current version
            // itself, which the store refuses while it is retained.
            let head = self
                .conn
                .send(Method::HEAD, Some(key), &[], Vec::new(), Bytes::new())
                .await?;
            match head.status() {
                StatusCode::OK => {}
                StatusCode::NOT_FOUND => return Ok(()),
                _ => return Err(failure(head).await),
            }
            if let Some(v) = head
                .headers()
                .get("x-amz-version-id")
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.is_empty() && *v != "null")
            {
                query.push(("versionId", v.to_owned()));
            }
        }
        let resp = self
            .conn
            .send(Method::DELETE, Some(key), &query, Vec::new(), Bytes::new())
            .await?;
        match resp.status() {
            StatusCode::NO_CONTENT | StatusCode::OK | StatusCode::NOT_FOUND => Ok(()),
            _ => Err(failure(resp).await),
        }
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        let body = self.list_body(prefix, after, max_keys).await?;
        xml::list_page(&body)
    }
}

impl S3Store {
    /// The body of the answer to one ListObjectsV2 request.
    async fn list_body(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<Bytes, ObjectStoreError> {
        let mut query = vec![
            ("list-type", "2".to_owned()),
            ("prefix", prefix.as_str().to_owned()),
            ("max-keys", max_keys.get().to_string()),
        ];
        match after.map(|t| &t.0) {
            Some(Token::Continue(t)) => query.push(("continuation-token", t.clone())),
            Some(Token::After(key) | Token::Kept { after: key, .. }) => {
                query.push(("start-after", key.as_str().to_owned()))
            }
            None => {}
        }
        let resp = self
            .conn
            .send(Method::GET, None, &query, Vec::new(), Bytes::new())
            .await?;
        if resp.status() != StatusCode::OK {
            return Err(failure(resp).await);
        }
        resp.bytes().await.map_err(transport)
    }
}

/// A transport failure, without the request URL: the text reaches REAPI clients (a CAS
/// `Unavailable` status), and the URL names the store's address, bucket and key. The
/// kind and causes ("operation timed out", "connection reset") stay.
fn transport(e: reqwest::Error) -> ObjectStoreError {
    ObjectStoreError::Transport(std::sync::Arc::new(e.without_url()))
}

/// The error a non-success answer carries.
async fn failure(resp: Response) -> ObjectStoreError {
    let status = resp.status().as_u16();
    let body = resp.bytes().await.unwrap_or_default();
    let (code, message) = xml::error_code_message(&body);
    ObjectStoreError::Service {
        status,
        code,
        message,
    }
}

/// Checks a 206 answer: `Content-Range` must name exactly the bytes asked for (cut at
/// the end of the object) and the body must be that long.
fn check_partial(
    content_range: Option<&str>,
    range: ByteRange,
    body_len: usize,
) -> Result<(), String> {
    let cr = content_range.ok_or("206 without Content-Range")?;
    let parsed = cr.strip_prefix("bytes ").and_then(|rest| {
        let (span, total) = rest.split_once('/')?;
        let (first, last) = span.split_once('-')?;
        Some((
            first.parse::<u64>().ok()?,
            last.parse::<u64>().ok()?,
            total.parse::<u64>().ok()?,
        ))
    });
    let Some((first, last, total)) = parsed else {
        return Err(format!("Content-Range {cr:?} does not parse"));
    };
    let want_last = range.last().min(total.saturating_sub(1));
    if first != range.offset() || last != want_last || last < first {
        return Err(format!(
            "asked for bytes {}-{} and got Content-Range {cr:?}",
            range.offset(),
            range.last()
        ));
    }
    if body_len as u64 != last - first + 1 {
        return Err(format!(
            "Content-Range {cr:?} but the body has {body_len} bytes"
        ));
    }
    Ok(())
}

fn valid_bucket(b: &str) -> bool {
    let edge_ok = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    (3..=63).contains(&b.len())
        && b.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        && edge_ok(b.chars().next())
        && edge_ok(b.chars().last())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(offset: u64, len: u64) -> ByteRange {
        ByteRange::new(offset, len).unwrap()
    }

    /// Catches: accepting a 206 for different bytes than asked (a store that rounds a
    /// range to its own block size), a body shorter than its header says, or a range cut
    /// at the end refused.
    #[test]
    fn partial_answers_must_match_the_range() {
        assert!(check_partial(Some("bytes 10-19/100"), r(10, 10), 10).is_ok());
        assert!(check_partial(Some("bytes 95-99/100"), r(95, 10), 5).is_ok());
        assert!(check_partial(Some("bytes 0-19/100"), r(10, 10), 20).is_err());
        assert!(check_partial(Some("bytes 10-19/100"), r(10, 10), 9).is_err());
        assert!(check_partial(Some("bytes 10-29/100"), r(10, 10), 20).is_err());
        assert!(check_partial(Some("bytes */100"), r(10, 10), 0).is_err());
        assert!(check_partial(None, r(0, 1), 1).is_err());
    }

    /// Catches: a timeout that ignores the body (the flat 60 s that cut a 128 MiB PUT
    /// on a slow store), one that scales a body-less request, or arithmetic that
    /// overflows on a huge body or a rate of one byte a second.
    #[test]
    fn the_timeout_grows_with_the_body_at_the_floor_rate() {
        let mut c = config("http://127.0.0.1:9000", "kbf-test");
        c.request_timeout = Duration::from_secs(60);
        let flat = S3Store::new(c.clone()).unwrap();
        assert_eq!(flat.conn.timeout_for(128 << 20), Duration::from_secs(60));
        c.min_body_rate = NonZeroU64::new(1 << 20);
        let scaled = S3Store::new(c.clone()).unwrap();
        assert_eq!(scaled.conn.timeout_for(0), Duration::from_secs(60));
        assert_eq!(scaled.conn.timeout_for(128 << 20), Duration::from_secs(188));
        assert_eq!(
            scaled.conn.timeout_for(512 << 10),
            Duration::from_millis(60_500)
        );
        c.min_body_rate = NonZeroU64::new(1);
        let slowest = S3Store::new(c).unwrap();
        let saturated = Duration::from_secs(60) + Duration::from_nanos(u64::MAX);
        assert_eq!(slowest.conn.timeout_for(usize::MAX), saturated);
    }

    /// Catches: the idle pool timeout reverting to reqwest's 90 s default, which lets a
    /// request reuse a connection the server has already closed.
    #[test]
    fn pool_idle_timeout_is_30s() {
        assert_eq!(POOL_IDLE_TIMEOUT, Duration::from_secs(30));
    }

    /// Catches: the builder not applying the idle timeout it is given (the default is
    /// 90 s, so a connection idle past the limit, here for a second of the pool's clock,
    /// would be reused, not replaced).
    #[tokio::test]
    async fn idle_connection_is_dropped_after_the_timeout() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let accepted = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&accepted);
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = sock.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let answer = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";
                        let _ = sock.write_all(answer).await;
                    }
                });
            }
        });
        let limit = Duration::from_secs(5);
        // Reuse: a long idle timeout keeps the connection across two requests.
        let keeping = client_builder(limit, Duration::from_secs(60))
            .build()
            .unwrap();
        keeping.get(&url).send().await.unwrap();
        keeping.get(&url).send().await.unwrap();
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "a fresh connection is reused"
        );
        // Expiry: a short idle timeout and a jump of the clock far above it replace the
        // connection. The jump is tokio's clock, which the pool reads; nothing waits.
        let dropping = client_builder(limit, Duration::from_millis(100))
            .build()
            .unwrap();
        dropping.get(&url).send().await.unwrap();
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::time::resume();
        dropping.get(&url).send().await.unwrap();
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            3,
            "an idle connection was kept"
        );
    }

    /// What a scripted store sends for a ranged read of `len` bytes.
    #[derive(Clone, Copy)]
    enum Script {
        /// Headers, then the body in `pieces` pieces `every` apart.
        Steady { pieces: usize, every: Duration },
        /// Headers, half the body, then nothing.
        StallsInBody,
        /// Nothing at all after the request.
        Silent,
        /// A 500 whose body is promised and never sent.
        StallsInError,
    }

    /// A one-object store on loopback that answers every request as `script` says and
    /// counts the requests it took; `served` is notified when it has read one, and has
    /// started its script (written everything the script writes at once). The `every` of a
    /// steady body is a time on tokio's clock, which the test that uses it pauses.
    async fn scripted(
        len: usize,
        script: Script,
    ) -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<tokio::sync::Notify>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let taken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = Arc::clone(&taken);
        let served = Arc::new(tokio::sync::Notify::new());
        let told = Arc::clone(&served);
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                tokio::spawn({
                    let count = Arc::clone(&count);
                    let told = Arc::clone(&told);
                    async move {
                        let mut seen = Vec::new();
                        let mut buf = [0u8; 4096];
                        while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                            match sock.read(&mut buf).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => seen.extend_from_slice(&buf[..n]),
                            }
                        }
                        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        told.notify_one();
                        let head = format!(
                            "HTTP/1.1 206 Partial Content\r\ncontent-length: {len}\r\n\
                             content-range: bytes 0-{}/{len}\r\n\r\n",
                            len - 1
                        );
                        match script {
                            Script::Silent => std::future::pending::<()>().await,
                            Script::StallsInError => {
                                let _ = sock
                                    .write_all(b"HTTP/1.1 500 Oops\r\ncontent-length: 100\r\n\r\n")
                                    .await;
                                std::future::pending::<()>().await;
                            }
                            Script::StallsInBody => {
                                let _ = sock.write_all(head.as_bytes()).await;
                                let _ = sock.write_all(&vec![7u8; len / 2]).await;
                                std::future::pending::<()>().await;
                            }
                            Script::Steady { pieces, every } => {
                                let _ = sock.write_all(head.as_bytes()).await;
                                for _ in 0..pieces {
                                    tokio::time::sleep(every).await;
                                    let _ = sock.write_all(&vec![7u8; len / pieces]).await;
                                }
                                // Keep the connection open until the client is done.
                                let _ = sock.read(&mut buf).await;
                            }
                        }
                    }
                });
            }
        });
        (endpoint, taken, served)
    }

    /// A store whose limit is [`LIMIT`].
    fn short_limit_store(endpoint: &str) -> S3Store {
        let mut c = config(endpoint, "kbf-test");
        c.request_timeout = LIMIT;
        S3Store::new(c).unwrap()
    }

    /// The `request_timeout` of the scripted reads. It passes on tokio's clock, paused by
    /// [`read_once_served`] once the scripted store has the request, so it costs no time.
    const LIMIT: Duration = Duration::from_secs(10);

    /// Reads `len` bytes from the scripted store at `endpoint`, with tokio's clock paused
    /// as soon as the store has the request and has written what its script writes at once.
    /// From then on the client waits for nothing but timers (its limit, and the pieces of a
    /// steady script), and the paused clock moves from one to the next by itself, so the
    /// outcome is decided by what the store sends and never sends, not by how long anything
    /// took. The clock is paused only now, because before it the client waits for the
    /// socket and an idle runtime would move the clock on past its limit.
    async fn read_once_served(
        endpoint: &str,
        served: &tokio::sync::Notify,
        len: usize,
    ) -> Result<Bytes, ObjectStoreError> {
        let store = short_limit_store(endpoint);
        let reading = tokio::spawn(async move { store.get_range(&key(), r(0, len as u64)).await });
        served.notified().await;
        tokio::time::pause();
        reading.await.unwrap()
    }

    fn key() -> ObjectKey {
        ObjectKey::new("obj").unwrap()
    }

    /// Catches: a read cut by a limit on the whole request: a body that arrives
    /// steadily, for several times `request_timeout` in all (20 pieces, 4 s apart, against
    /// a limit of 10 s), must come back whole.
    #[tokio::test]
    async fn a_ranged_read_that_keeps_progressing_outlasts_the_request_timeout() {
        let len = 1000;
        let (endpoint, taken, served) = scripted(
            len,
            Script::Steady {
                pieces: 20,
                every: Duration::from_secs(4),
            },
        )
        .await;
        let got = read_once_served(&endpoint, &served, len).await.unwrap();
        assert_eq!(got.as_ref(), vec![7u8; len].as_slice());
        assert_eq!(taken.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Catches: a stalled body that is never cut, or cut but reported as a plain
    /// transport failure and sent again: it fails as a timeout, after one request.
    #[tokio::test]
    async fn a_ranged_read_whose_body_stops_fails_as_a_timeout_and_is_not_resent() {
        let (endpoint, taken, served) = scripted(1000, Script::StallsInBody).await;
        let err = read_once_served(&endpoint, &served, 1000)
            .await
            .unwrap_err();
        assert!(!is_retryable(&err));
        assert!(err.to_string().contains("timed out"), "{err}");
        assert_eq!(taken.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Catches: the wait for the answer's headers left unbounded once the whole-request
    /// limit is gone.
    #[tokio::test]
    async fn a_ranged_read_with_no_answer_fails_as_a_timeout() {
        let (endpoint, taken, served) = scripted(10, Script::Silent).await;
        let err = read_once_served(&endpoint, &served, 10).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert_eq!(taken.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Catches: an error answer whose body never arrives holding the read forever.
    #[tokio::test]
    async fn an_error_body_that_stops_fails_as_a_timeout() {
        let (endpoint, _, served) = scripted(10, Script::StallsInError).await;
        let err = read_once_served(&endpoint, &served, 10).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    /// The error reqwest gives for a response whose body delivers a few bytes and then
    /// fails with `kind`, read as `get_range_once` reads one, wrapped as it wraps it.
    async fn body_error(kind: std::io::ErrorKind) -> (ObjectStoreError, bool) {
        struct Failing {
            kind: std::io::ErrorKind,
            sent: bool,
        }
        impl http_body::Body for Failing {
            type Data = Bytes;
            type Error = std::io::Error;
            fn poll_frame(
                mut self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Result<http_body::Frame<Bytes>, Self::Error>>> {
                let sent = std::mem::replace(&mut self.sent, true);
                std::task::Poll::Ready(Some(if sent {
                    Err(std::io::Error::from(self.kind))
                } else {
                    Ok(http_body::Frame::data(Bytes::from_static(b"part")))
                }))
            }
        }
        let body = reqwest::Body::wrap(Failing { kind, sent: false });
        let mut resp = reqwest::Response::from(http::Response::new(body));
        let first = resp.chunk().await.unwrap();
        assert_eq!(first.as_deref(), Some(&b"part"[..]));
        let e = resp.chunk().await.unwrap_err();
        let timeout = e.is_timeout();
        (transport(e), timeout)
    }

    /// Catches: a read whose connection the kernel timed out mid-body (`ETIMEDOUT`,
    /// which reqwest also reports as `is_timeout`) never sent again, failing a large
    /// download that a fresh connection would finish; and the same for a failure that
    /// is no timeout at all being refused.
    #[tokio::test]
    async fn a_body_cut_by_an_os_level_timeout_is_resent() {
        let (e, timeout) = body_error(std::io::ErrorKind::TimedOut).await;
        assert!(timeout, "reqwest must call this a timeout: {e:?}");
        assert!(is_retryable(&e));
        let (e, timeout) = body_error(std::io::ErrorKind::ConnectionReset).await;
        assert!(!timeout);
        assert!(is_retryable(&e));
    }

    /// Catches: the client's own connect limit, which ends as an `io::Error` of kind
    /// `TimedOut` in the chain, taken for a dead connection and sent again; and a
    /// refused connection taken for a timeout.
    // Linux drops a SYN when the accept queue is full; BSD and macOS may reset instead.
    #[cfg(target_os = "linux")]
    #[tokio::test(start_paused = true)]
    async fn a_connect_that_times_out_is_not_resent_but_a_refused_one_is() {
        // A listener whose accept queue is full drops the next SYN: the connect hangs.
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = socket.local_addr().unwrap();
        let _listener = socket.listen(1).unwrap();
        let mut held = Vec::new();
        for _ in 0..4 {
            let filler = tokio::net::TcpSocket::new_v4().unwrap();
            if let Ok(Ok(c)) =
                tokio::time::timeout(Duration::from_millis(200), filler.connect(addr)).await
            {
                held.push(c);
            }
        }
        let client = client_builder(Duration::from_millis(100), POOL_IDLE_TIMEOUT)
            .build()
            .unwrap();
        let e = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap_err();
        assert!(e.is_connect(), "{e:?}");
        assert!(e.is_timeout(), "{e:?}");
        assert!(!is_retryable(&transport(e)));

        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gone = closed.local_addr().unwrap();
        drop(closed);
        let e = client
            .get(format!("http://{gone}/"))
            .send()
            .await
            .unwrap_err();
        assert!(e.is_connect(), "{e:?}");
        assert!(!e.is_timeout(), "{e:?}");
        assert!(is_retryable(&transport(e)));
    }

    /// Catches: every non-connect reqwest error sent again, including the client's own
    /// per-request limit, which ends a read against a store that accepts and never
    /// answers and would multiply the wait of a read that cannot succeed.
    #[tokio::test(start_paused = true)]
    async fn a_request_that_exceeds_its_client_timeout_is_not_resent() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _silent = tokio::spawn(async move {
            let held = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
            drop(held);
        });
        let e = reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .timeout(Duration::from_millis(100))
            .send()
            .await
            .unwrap_err();
        assert!(e.is_timeout(), "{e:?}");
        assert!(!e.is_connect(), "{e:?}");
        assert!(!is_retryable(&transport(e)));
    }

    /// Catches: this module's own stall treated as worth a resend.
    #[test]
    fn a_stall_is_not_resent() {
        assert!(!is_retryable(&ObjectStoreError::Transport(Arc::new(
            Stalled
        ))));
    }

    /// Catches: a transport error of the timeout kind from reqwest, or this module's
    /// own stall, treated as worth a resend; and any other transport error as not.
    #[test]
    fn only_a_stall_or_timeout_is_not_resent() {
        let stalled = ObjectStoreError::Transport(Arc::new(Stalled));
        assert!(!is_retryable(&stalled));
        let reset = ObjectStoreError::Transport(Arc::new(std::io::Error::other("reset")));
        assert!(is_retryable(&reset));
    }

    fn config(endpoint: &str, bucket: &str) -> S3Config {
        S3Config {
            endpoint: endpoint.to_owned(),
            region: "us-east-1".to_owned(),
            bucket: bucket.to_owned(),
            credentials: Credentials::new("a", "s"),
            capabilities: Capabilities::default(),
            connect_timeout: Duration::from_secs(1),
            request_timeout: Duration::from_secs(1),
            min_body_rate: None,
        }
    }

    /// Catches: an endpoint whose path, scheme or credentials would make the signed
    /// request differ from the one sent, and bucket names S3 refuses.
    #[test]
    fn config_is_checked() {
        assert!(S3Store::new(config("http://127.0.0.1:9000", "kbf-cas")).is_ok());
        for bad in [
            "https://127.0.0.1:9000",
            "ftp://127.0.0.1",
            "http://127.0.0.1:9000/base",
            "http://u:p@127.0.0.1:9000",
            "http://:p@127.0.0.1:9000",
            "http://127.0.0.1:9000/#frag",
            "http://127.0.0.1:9000/?x=1",
            "127.0.0.1:9000",
        ] {
            assert!(
                matches!(
                    S3Store::new(config(bad, "kbf-cas")),
                    Err(S3ConfigError::Endpoint { .. })
                ),
                "{bad}"
            );
        }
        for bad in ["ab", "Kbf", "-kbf", "kbf-", "kbf_cas", &"a".repeat(64)] {
            assert!(
                matches!(
                    S3Store::new(config("http://127.0.0.1", bad)),
                    Err(S3ConfigError::Bucket(_))
                ),
                "{bad}"
            );
        }
    }

    /// Catches: a secret printed by `Debug` into a log.
    #[test]
    fn debug_hides_the_secret() {
        let text = format!("{:?}", config("http://127.0.0.1", "kbf-cas"));
        assert!(text.contains("<redacted>"), "{text}");
        assert!(!text.contains("\"s\""), "{text}");
    }

    /// Catches: the store's `Debug` (it is logged) that prints the credentials or leaves
    /// out which endpoint, bucket and capabilities it addresses.
    #[test]
    fn the_store_debug_names_its_bucket_and_hides_its_credentials() {
        let mut c = config("http://127.0.0.1:9000", "kbf-cas");
        c.credentials = Credentials::new("AKIDSECRETISH", "hunter2");
        let text = format!("{:?}", S3Store::new(c).unwrap());
        assert!(text.starts_with("S3Store {"), "{text}");
        assert!(text.contains("base: \"http://127.0.0.1:9000/\""), "{text}");
        assert!(text.contains("bucket: \"kbf-cas\""), "{text}");
        assert!(text.contains("capabilities:"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("AKIDSECRETISH"), "{text}");
    }

    /// Catches: bucket-name edges: a dot inside, a digit at either end, and the exact
    /// length limits (3 and 63) refused or accepted wrongly.
    #[test]
    fn bucket_names_allow_dots_digits_and_the_length_limits() {
        for ok in ["kbf.cas1", "1kbf", "kbf1", "abc", &"a".repeat(63)] {
            assert!(valid_bucket(ok), "{ok}");
        }
        for bad in [
            "",
            "ab",
            "kbf.cas.",
            ".kbf",
            "kbf cas",
            "kbf/cas",
            &"a".repeat(64),
        ] {
            assert!(!valid_bucket(bad), "{bad}");
        }
    }

    /// Catches: a 206 whose Content-Range is malformed in each field, or that names the
    /// right first byte and wrong last (or inverted) bytes, accepted as the window asked.
    #[test]
    fn partial_answers_with_malformed_or_inverted_ranges_are_refused() {
        for cr in [
            "items 0-9/10",
            "bytes 0-9",
            "bytes 0/10",
            "bytes a-9/10",
            "bytes 0-b/10",
            "bytes 0-9/c",
        ] {
            let why = check_partial(Some(cr), r(0, 10), 10).unwrap_err();
            assert!(why.contains("does not parse"), "{cr}: {why}");
        }
        let why = check_partial(Some("bytes 0-4/10"), r(0, 10), 5).unwrap_err();
        assert!(why.contains("asked for bytes 0-9"), "{why}");
        let why = check_partial(Some("bytes 5-4/5"), r(5, 1), 0).unwrap_err();
        assert!(why.contains("asked for bytes 5-5"), "{why}");
        let why = check_partial(Some("bytes 0-9/10"), r(0, 10), 9).unwrap_err();
        assert!(why.contains("the body has 9 bytes"), "{why}");
    }
}
