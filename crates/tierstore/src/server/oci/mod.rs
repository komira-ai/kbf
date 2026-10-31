//! A read-only OCI distribution front over the objects of a [`Backend`], served by
//! `--oci-listen` over HTTPS, or over plain HTTP, so that a container engine (podman
//! through `certs.d`, or any client of the distribution API) pulls an image by digest from the
//! store.
//!
//! # Two modes
//!
//! With `--oci-tls-cert` and `--oci-tls-key` the front serves HTTPS ([`OciMode::Https`]) at any
//! address. With neither it serves plain HTTP/1.1 ([`OciMode::Http`]) at any address. A node's
//! own container engine pulls from the front of its own store over loopback (127/8 or `::1`),
//! and that traffic never leaves the host; on any other address a plain connection can be read
//! and changed on its path, so the start prints one warning line and a certificate is advised.
//! Everything else, the routes, the limits and the drain, is the same in both modes,
//! except that a plain connection has no handshake to time.
//!
//! # What it serves
//!
//! `GET` and `HEAD` of these, and nothing else (any other path is 404 with an OCI error body,
//! and any other method on any path is 405 `UNSUPPORTED` with `Allow: GET, HEAD`):
//!
//! - `/v2/`: 200, the body `{}` and `Docker-Distribution-API-Version: registry/2.0`;
//! - `/v2/<name>/manifests/<digest>`: the manifest, read whole (at most 4 MiB), its SHA-256
//!   checked against the digest before one byte is sent. A tag in place of the digest is 404
//!   `MANIFEST_UNKNOWN`: tags are not served, only digests;
//! - `/v2/<name>/blobs/<digest>`: the blob, streamed, with one `Range` if the client sends one.
//!
//! Every answer carries `Docker-Distribution-API-Version`; the others, on the two kinds of
//! object, are `Content-Type`, `Content-Length`, `Docker-Content-Digest` and an `ETag` of the
//! digest in quotes.
//!
//! # Keys, and one namespace
//!
//! Manifests are blobs. An object is at `<prefix>blobs/sha256/<64 lowercase hex digits>`
//! (`--oci-prefix`, `oci/` by default), the same for a manifest, an index, a layer and a config.
//! The `<name>` in a URL is checked against the grammar of the distribution specification (a bad
//! one is 400 `NAME_INVALID`) and then ignored: there is one content-addressed namespace, so any
//! name reads any digest. A key is built only from a validated digest, so nothing outside the
//! prefix is reachable, and a prefix that overlaps `_journal/` or `_tiering/` is refused at start.
//!
//! # Integrity
//!
//! A manifest whose bytes do not hash to its digest is 500 and is never sent. A blob is hashed
//! as it is read, in chunks of 4 MiB, and its last chunk is held back until the hash is right:
//! if it is not, the connection is cut before the response is complete (its `Content-Length`
//! has been sent), so a client never gets a whole body that is wrong, and a blob of one chunk
//! delivers no body byte. Both are counted. A manifest is measured with a `head` before it is
//! read, so a manifest replaced between the two calls by an object of another size can be
//! answered 500 `DIGEST_INVALID` once; a retry succeeds. A manifest `HEAD` reads and verifies the object; a blob
//! `HEAD` or a read of part of a blob reports what the store holds without reading it all, and so
//! verifies nothing (a range that covers the whole blob is read and checked like a whole read): the
//! client checks the digest of what it assembles. `If-Range`, `If-None-Match` and `Accept` are
//! ignored, as the specification allows.
//!
//! # Access
//!
//! There is no authentication and no token is ever asked for: a request that carries an
//! `Authorization` header is served as one that does not. Only objects under the prefix, named by
//! their digest, can be read; the images in it are meant to be public (a public registry's
//! images are the first thing seeded), and which hosts may connect is the network's to decide.
//!
//! # Errors
//!
//! The bodies are the distribution error document, `{"errors":[{"code","message","detail"}]}`:
//! `NAME_INVALID` and `DIGEST_INVALID` for a name or a digest that is malformed (400),
//! `MANIFEST_UNKNOWN` and `BLOB_UNKNOWN` for an object that is not there (404), `UNSUPPORTED`
//! for a route or method not served, `RANGE_NOT_SATISFIABLE` (416), `MANIFEST_INVALID` for a
//! stored object that is no manifest, `DIGEST_INVALID` (500) for one that is not its digest, and
//! `UNAVAILABLE` (503) or `UNKNOWN` (500) when the store fails.

mod body;
mod listener;
mod manifest;
mod metrics;
mod names;
mod range;
mod tls;

#[cfg(test)]
mod body_tests;
#[cfg(test)]
mod front_tests;
#[cfg(test)]
mod listener_tests;
#[cfg(test)]
mod parse_tests;
#[cfg(test)]
pub(crate) mod test_pki;
#[cfg(test)]
mod tls_tests;

use std::sync::Arc;

use bytes::Bytes;
use hyper::header::{
    ACCEPT_RANGES, ALLOW, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, HeaderName,
    HeaderValue, RANGE,
};
use hyper::{HeaderMap, Method, Response, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::journal::{JOURNAL_PREFIX, TIERING_PREFIX};
use crate::{Backend, ByteRange, Error};

pub use body::{BodyError, OciBody};
pub use listener::{OciLimits, OciListener};
pub use metrics::OciMetrics;
pub use tls::{OciError, OciMode, acceptor, acceptor_for, plain_warning};

use body::BlobRead;
use metrics::{Outcome, RouteKind};
use names::{Digest, Route};
use range::Range;

/// The largest manifest or index served.
pub const MAX_MANIFEST_BYTES: u64 = 4 << 20;

/// The prefixes the proxy keeps for itself, which the front's must not overlap.
const RESERVED: [&str; 2] = [JOURNAL_PREFIX, TIERING_PREFIX];

/// The longest piece of request text an error echoes back.
const ECHO_BYTES: usize = 256;

const DISTRIBUTION_VERSION: HeaderName = HeaderName::from_static("docker-distribution-api-version");
const CONTENT_DIGEST: HeaderName = HeaderName::from_static("docker-content-digest");

/// The prefix of the keys of the front's objects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OciPrefix(String);

impl OciPrefix {
    /// The prefix `text`: a path that ends in `/`, of segments made of letters, digits, `.`, `_`
    /// and `-`, none of them `.` or `..` or empty, and not inside (or equal to) `_journal/` or
    /// `_tiering/`.
    ///
    /// # Errors
    /// [`OciError::Prefix`] says what is wrong.
    pub fn new(text: &str) -> Result<Self, OciError> {
        let refuse = |why: &str| OciError::Prefix {
            prefix: text.to_owned(),
            why: why.to_owned(),
        };
        let Some(path) = text.strip_suffix('/') else {
            return Err(refuse("it must end in /"));
        };
        let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
        let segment = |s: &str| !s.is_empty() && s != "." && s != ".." && s.chars().all(plain);
        if !path.split('/').all(segment) {
            return Err(refuse(
                "it must be segments of letters, digits, . _ and -, separated by single /",
            ));
        }
        if RESERVED.iter().any(|reserved| text.starts_with(reserved)) {
            return Err(refuse("it overlaps a prefix the store keeps for itself"));
        }
        Ok(Self(text.to_owned()))
    }

    /// The key of the object `digest` names.
    fn key(&self, digest: &Digest) -> String {
        format!("{}blobs/sha256/{}", self.0, digest.hex())
    }

    /// The text of the prefix.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The front: answers requests from a backend.
pub struct OciFront {
    backend: Arc<dyn Backend>,
    prefix: OciPrefix,
    metrics: Arc<OciMetrics>,
}

impl OciFront {
    /// A front over `backend` that reads under `prefix` and counts into `metrics`.
    #[must_use]
    pub fn new(backend: Arc<dyn Backend>, prefix: OciPrefix, metrics: Arc<OciMetrics>) -> Self {
        Self {
            backend,
            prefix,
            metrics,
        }
    }

    /// Answers a request for `path`, whose body, if it has one, is not read.
    pub async fn handle(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
    ) -> Response<OciBody> {
        let head = *method == Method::HEAD;
        let route = names::route(path);
        let kind = match route {
            Route::Ping => RouteKind::Ping,
            Route::Manifest { .. } => RouteKind::Manifest,
            Route::Blob { .. } => RouteKind::Blob,
            Route::Other => RouteKind::Other,
        };
        let (outcome, mut response) = if !head && *method != Method::GET {
            (Outcome::NotAllowed, not_allowed())
        } else {
            match route {
                Route::Ping => (Outcome::Ok, ping()),
                Route::Manifest { name, reference } => self.manifest(head, name, reference).await,
                Route::Blob { name, digest } => self.blob(head, name, digest, headers).await,
                Route::Other => (Outcome::NotFound, unsupported_route()),
            }
        };
        self.metrics.request(kind, outcome);
        let version = HeaderValue::from_static("registry/2.0");
        response.headers_mut().insert(DISTRIBUTION_VERSION, version);
        response
    }

    /// `GET` or `HEAD` of a manifest.
    async fn manifest(
        &self,
        head: bool,
        name: &str,
        reference: &str,
    ) -> (Outcome, Response<OciBody>) {
        const UNKNOWN: &str = "MANIFEST_UNKNOWN";
        if !names::valid_name(name) {
            return invalid_name(name);
        }
        let digest = match Digest::parse(reference) {
            Some(digest) => digest,
            None if names::is_tag(reference) => {
                let message = "tags are not served: pull by digest (name@sha256:<hex>)";
                let detail = json!({ "tag": echoed(reference) });
                return (
                    Outcome::NotFound,
                    error(StatusCode::NOT_FOUND, UNKNOWN, message, detail),
                );
            }
            None => return invalid_digest(reference),
        };
        let key = self.prefix.key(&digest);
        // The size is asked first: the digest of any object under the prefix can be named here,
        // and a layer of gigabytes must not be read whole to find out it is not a manifest.
        let size = match self.backend.head(&key).await {
            Ok(meta) => meta.size,
            Err(failure) => return store_failed(&failure, &key, UNKNOWN, &digest),
        };
        if size > MAX_MANIFEST_BYTES {
            let message =
                format!("the object is over the {MAX_MANIFEST_BYTES}-byte limit of a manifest");
            let detail = json!({ "digest": digest.text() });
            return (
                Outcome::NotFound,
                error(StatusCode::NOT_FOUND, UNKNOWN, &message, detail),
            );
        }
        // Bounded by the limit even if the object has grown since it was measured. The head and
        // the get are two calls: if the object is replaced between them, the bytes read may not
        // hash to the digest and the answer is 500 DIGEST_INVALID for that one request; a retry
        // reads the new object and succeeds.
        let range = ByteRange::new(0, MAX_MANIFEST_BYTES);
        let body = match self.backend.get(&key, Some(range)).await {
            Ok(body) => body,
            Err(failure) => return store_failed(&failure, &key, UNKNOWN, &digest),
        };
        let found = Digest::of_hex(hex::encode(Sha256::digest(&body)));
        if found != digest {
            return mismatch(&self.metrics, &digest, &found);
        }
        let media_type = match manifest::media_type(&body) {
            Ok(media_type) => media_type,
            Err(why) => {
                let detail = json!({ "digest": digest.text() });
                let failed = error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "MANIFEST_INVALID",
                    &why,
                    detail,
                );
                return (Outcome::Failed, failed);
            }
        };
        let length = body.len();
        let mut response = Response::new(if head {
            body::empty()
        } else {
            body::full(body)
        });
        let headers = response.headers_mut();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(media_type));
        headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
        identify(headers, &digest);
        (Outcome::Ok, response)
    }

    /// `GET` or `HEAD` of a blob.
    async fn blob(
        &self,
        head: bool,
        name: &str,
        digest: &str,
        request: &HeaderMap,
    ) -> (Outcome, Response<OciBody>) {
        const UNKNOWN: &str = "BLOB_UNKNOWN";
        if !names::valid_name(name) {
            return invalid_name(name);
        }
        let Some(digest) = Digest::parse(digest) else {
            return invalid_digest(digest);
        };
        let key = self.prefix.key(&digest);
        let size = match self.backend.head(&key).await {
            Ok(meta) => meta.size,
            Err(failure) => return store_failed(&failure, &key, UNKNOWN, &digest),
        };
        let (status, outcome, start, end) = match range::parse(request.get(RANGE), size) {
            Range::Whole => (StatusCode::OK, Outcome::Ok, 0, size),
            Range::Partial { start, end } => (
                StatusCode::PARTIAL_CONTENT,
                Outcome::Partial,
                start,
                end + 1,
            ),
            Range::Unsatisfiable => return unsatisfiable(size),
        };
        let mut response = Response::new(body::empty());
        *response.status_mut() = status;
        let headers = response.headers_mut();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        headers.insert(CONTENT_LENGTH, HeaderValue::from(end - start));
        headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        identify(headers, &digest);
        if status == StatusCode::PARTIAL_CONTENT {
            let value = format!("bytes {start}-{}/{size}", end - 1);
            headers.insert(CONTENT_RANGE, header_value(&value));
        }
        if !head {
            // A range that covers the object is as good as a whole read, and is checked.
            let whole = start == 0 && end == size;
            let read = BlobRead {
                backend: Arc::clone(&self.backend),
                key,
                start,
                end,
                verify: whole.then_some(digest),
                metrics: Arc::clone(&self.metrics),
                chunk: body::CHUNK_BYTES,
            };
            *response.body_mut() = read.body();
        }
        (outcome, response)
    }
}

/// The headers that name an object by its digest.
fn identify(headers: &mut HeaderMap, digest: &Digest) {
    headers.insert(CONTENT_DIGEST, header_value(&digest.text()));
    headers.insert(ETAG, header_value(&format!("\"{}\"", digest.text())));
}

/// A header value of text that is known to be visible ASCII.
fn header_value(text: &str) -> HeaderValue {
    HeaderValue::from_str(text).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// `GET /v2/`.
fn ping() -> Response<OciBody> {
    let mut response = Response::new(body::full(Bytes::from_static(b"{}")));
    let json = HeaderValue::from_static("application/json; charset=utf-8");
    response.headers_mut().insert(CONTENT_TYPE, json);
    response
        .headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from(2));
    response
}

/// The distribution error document, with `status`.
fn error(status: StatusCode, code: &str, message: &str, detail: Value) -> Response<OciBody> {
    let document = json!({ "errors": [{ "code": code, "message": message, "detail": detail }] });
    let bytes = Bytes::from(document.to_string());
    let mut response = Response::new(body::full(bytes.clone()));
    *response.status_mut() = status;
    let json = HeaderValue::from_static("application/json; charset=utf-8");
    response.headers_mut().insert(CONTENT_TYPE, json);
    response
        .headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    response
}

/// A method that is not `GET` or `HEAD`.
fn not_allowed() -> Response<OciBody> {
    let message = "this registry front is read-only: only GET and HEAD are served";
    let mut response = error(
        StatusCode::METHOD_NOT_ALLOWED,
        "UNSUPPORTED",
        message,
        Value::Null,
    );
    response
        .headers_mut()
        .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
    response
}

/// A path that is none of the three routes.
fn unsupported_route() -> Response<OciBody> {
    let message =
        "only /v2/, /v2/<name>/manifests/<digest> and /v2/<name>/blobs/<digest> are served";
    error(StatusCode::NOT_FOUND, "UNSUPPORTED", message, Value::Null)
}

/// `text` cut to [`ECHO_BYTES`] bytes (at a character boundary), for an error to repeat.
fn echoed(text: &str) -> &str {
    let mut end = text.len().min(ECHO_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn invalid_name(name: &str) -> (Outcome, Response<OciBody>) {
    let message = "the repository name is not valid";
    let detail = json!({ "name": echoed(name) });
    (
        Outcome::Invalid,
        error(StatusCode::BAD_REQUEST, "NAME_INVALID", message, detail),
    )
}

fn invalid_digest(text: &str) -> (Outcome, Response<OciBody>) {
    let message = "the digest must be sha256: and 64 lowercase hex digits";
    let detail = json!({ "digest": echoed(text) });
    (
        Outcome::Invalid,
        error(StatusCode::BAD_REQUEST, "DIGEST_INVALID", message, detail),
    )
}

/// The answer for a range that holds no byte of an object of `size` bytes.
fn unsatisfiable(size: u64) -> (Outcome, Response<OciBody>) {
    let message = "the range holds no byte of the blob";
    let mut response = error(
        StatusCode::RANGE_NOT_SATISFIABLE,
        "RANGE_NOT_SATISFIABLE",
        message,
        json!({ "size": size }),
    );
    response
        .headers_mut()
        .insert(CONTENT_RANGE, header_value(&format!("bytes */{size}")));
    (Outcome::Unsatisfiable, response)
}

/// The answer for an object whose bytes are not the digest it is named by.
fn mismatch(metrics: &OciMetrics, wanted: &Digest, found: &Digest) -> (Outcome, Response<OciBody>) {
    metrics.mismatch();
    eprintln!(
        "tierstore: oci: {} holds bytes that hash to {}",
        wanted.text(),
        found.text()
    );
    let message = "the stored object does not match its digest and is not served";
    let detail = json!({ "digest": wanted.text() });
    (
        Outcome::Failed,
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "DIGEST_INVALID",
            message,
            detail,
        ),
    )
}

/// The answer for a read the store failed: 404 for an object that is not there, 503 for a store
/// that cannot be reached, 500 for the rest. What the store said goes to stderr, not to the
/// client.
fn store_failed(
    failure: &Error,
    key: &str,
    unknown: &str,
    digest: &Digest,
) -> (Outcome, Response<OciBody>) {
    let detail = json!({ "digest": digest.text() });
    match failure {
        Error::NotFound(_) => {
            let failed = error(
                StatusCode::NOT_FOUND,
                unknown,
                "the object is not in the store",
                detail,
            );
            (Outcome::NotFound, failed)
        }
        other => {
            eprintln!("tierstore: oci: reading {key}: {other}");
            let (status, code) = if other.is_unavailable() {
                (StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE")
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN")
            };
            (
                Outcome::Failed,
                error(status, code, "the store could not read the object", detail),
            )
        }
    }
}
