//! An S3 service on loopback, for tests (feature `testkit`): PUT, GET (whole or one range),
//! HEAD, ListObjectsV2 (prefix, `max-keys`, `continuation-token`, `start-after`), and
//! multipart upload (create, parts, complete, abort, and ListMultipartUploads with `prefix`,
//! `max-uploads`, `key-marker` and `upload-id-marker`, in the order of key and id), with S3's rules on
//! part sizes enforced at completion and conditional writes (`If-None-Match: *`) on a
//! PUT and a completion, a PUT that replaces an object only if it has the entity tag in
//! `If-Match` (412 for another tag, 404 if there is no object), and DELETE of a plain object.
//!
//! Every object has a time of last write: the system clock until a test sets one
//! ([`FakeS3::set_time`]), in whole seconds as S3 keeps them. The entity tag of an object is
//! the quoted hex MD5 of its bytes, as S3 makes it for an object written by one PUT. It is
//! worked out when a HEAD asks for it and for a PUT that carries `If-Match` (an object of
//! many GiB of zeros is held compactly, and hashing it at every PUT would cost what the
//! compact form saves), so a HEAD of an object that does not fit in memory is not for this
//! service. The tag of a part depends on its bytes alone ([`part_etag`]; it is not an MD5,
//! which a part of many MiB of zeros would cost the same), and the tag of an object made of
//! parts is, as on S3, not the MD5 of its bytes: it is the quoted hex MD5 of the tags of the
//! parts, a dash and the number of parts. The answer to the completion names it, and so does
//! a HEAD.
//!
//! An upload is known by its id and its key: a part, a completion or an abort that names
//! the id of an upload with another key finds no upload, as on S3 (`NoSuchUpload`, 404 for a
//! part or an abort and 400 for a completion). The 5 MiB that every part but the last must
//! have can be lowered ([`FakeS3::set_min_part_bytes`]) for a test that sends small parts.
//!
//! It keeps what a test asks about: the uploads begun and not yet completed or aborted
//! ([`FakeS3::incomplete_uploads`]), how many single PUTs and parts it took and the
//! largest request body ([`FakeS3::stats`]). A test can fail any step of an upload
//! once ([`FakeS3::fail`]).
//!
//! Bytes are kept compactly: a run of zeros is counted, not stored, so an object of
//! many GiB of zeros costs the test nothing to hold. It checks no signature unless a test
//! asks ([`FakeS3::require_credentials`]): then a request not signed with that key pair
//! is refused 403, as a real store refuses a wrong key.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use md5::{Digest, Md5};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

mod uploads;

use self::uploads::list_uploads;
use super::sigv4::{Request as Signed, Signer, Utc};
use super::times::MONTHS;
use super::{Credentials, S3Config, S3Store};
use crate::{Capabilities, ObjectKey};

/// The weekday names of an HTTP date, from Thursday: the day of the epoch.
const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];

/// The runs bytes are cut into when kept: a run of all zeros is only counted.
const RUN: usize = 64 << 10;

static ZEROS: [u8; RUN] = [0; RUN];

/// What the service took, for a test to check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FakeStats {
    /// PUTs of a whole object.
    pub single_puts: u64,
    /// Parts of multipart uploads.
    pub parts: u64,
    /// Multipart uploads completed.
    pub completed: u64,
    /// Multipart uploads aborted.
    pub aborted: u64,
    /// The largest request body received.
    pub largest_body: u64,
}

/// Bytes kept compactly.
#[derive(Clone, Debug, Default)]
struct Kept {
    /// Runs in order: `Err(n)` is `n` zero bytes.
    runs: Vec<Result<Bytes, u64>>,
    len: u64,
}

impl Kept {
    fn of(bytes: &[u8]) -> Self {
        let mut kept = Self::default();
        for run in bytes.chunks(RUN) {
            if run == &ZEROS[..run.len()] {
                kept.push(Err(run.len() as u64));
            } else {
                kept.push(Ok(Bytes::copy_from_slice(run)));
            }
        }
        kept
    }

    fn push(&mut self, run: Result<Bytes, u64>) {
        let n = match &run {
            Ok(b) => b.len() as u64,
            Err(n) => *n,
        };
        self.len += n;
        match (self.runs.last_mut(), run) {
            (Some(Err(zeros)), Err(more)) => *zeros += more,
            (_, run) => self.runs.push(run),
        }
    }

    fn extend(&mut self, other: &Self) {
        for run in &other.runs {
            self.push(run.clone());
        }
    }

    /// Bytes `[from, to)`.
    fn read(&self, from: u64, to: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(usize::try_from(to - from).unwrap_or(0));
        let mut at = 0u64;
        for run in &self.runs {
            let n = match run {
                Ok(b) => b.len() as u64,
                Err(n) => *n,
            };
            let (start, end) = (from.max(at), to.min(at + n));
            if start < end {
                let (a, b) = ((start - at) as usize, (end - at) as usize);
                match run {
                    Ok(bytes) => out.extend_from_slice(&bytes[a..b]),
                    Err(_) => out.resize(out.len() + (b - a), 0),
                }
            }
            at += n;
            if at >= to {
                break;
            }
        }
        out
    }
}

/// A request [`FakeS3::fail`] makes the service fail, once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// CreateMultipartUpload answers 500.
    Create,
    /// The upload of part `n` (of any upload) answers 500.
    Part(u32),
    /// The next part's answer has no ETag.
    NoEtag,
    /// CompleteMultipartUpload answers 500; the upload stays open.
    Complete,
    /// CompleteMultipartUpload answers 200 with an `<Error>` body, as S3 does when a
    /// completion fails after its answer started; the upload stays open.
    CompleteLate,
    /// AbortMultipartUpload answers 500; the upload stays open.
    Abort,
}

impl State {
    /// Whether `failure` was asked for; it is then used up.
    fn take(&mut self, failure: Failure) -> bool {
        let at = self.failures.iter().position(|f| *f == failure);
        at.map(|i| self.failures.remove(i)).is_some()
    }

    /// Keeps `kept` under `key`, stamped with the time of the service, with the entity tag
    /// `joined` if it is made of parts and the tag of its bytes if not.
    fn store(&mut self, key: String, kept: Kept, joined: Option<String>) {
        let modified = self.time.unwrap_or_else(system_seconds);
        let stored = Stored {
            kept,
            modified,
            joined,
        };
        self.objects.insert(key, stored);
    }
}

/// The second the system clock says.
fn system_seconds() -> u64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    since.map_or(0, |since| since.as_secs())
}

/// The entity tag S3 gives an object written by one PUT: the quoted hex MD5 of its bytes.
fn etag_of(kept: &Kept) -> String {
    let digest = Md5::digest(kept.read(0, kept.len));
    format!("\"{}\"", hex::encode(digest))
}

/// The entity tag this service gives a part: quoted, and a hash of the bytes that is
/// worked out from the runs they are kept in, so that a run of zeros costs its length and
/// not its bytes.
fn part_tag(kept: &Kept) -> String {
    let mut hasher = DefaultHasher::new();
    for run in &kept.runs {
        match run {
            Ok(bytes) => {
                1_u8.hash(&mut hasher);
                bytes.hash(&mut hasher);
            }
            Err(zeros) => {
                0_u8.hash(&mut hasher);
                zeros.hash(&mut hasher);
            }
        }
    }
    format!("\"{:016x}\"", hasher.finish())
}

/// The entity tag this service gives a part of these bytes, quotes included: the same for
/// the same bytes, and another for other bytes.
#[must_use]
pub fn part_etag(bytes: &[u8]) -> String {
    part_tag(&Kept::of(bytes))
}

/// `Sun, 06 Nov 1994 08:49:37 GMT`, the form of a `Last-Modified` header.
fn http_date(seconds: u64) -> String {
    let utc = Utc::of(UNIX_EPOCH + Duration::from_secs(seconds), false);
    let weekday = WEEKDAYS[((seconds / 86_400) % 7) as usize];
    let month = MONTHS[utc.month as usize - 1];
    format!(
        "{weekday}, {:02} {month} {:04} {:02}:{:02}:{:02} GMT",
        utc.day, utc.year, utc.hour, utc.minute, utc.second
    )
}

/// `2009-10-12T17:50:30.000Z`, the form of a `<LastModified>` of a listing.
fn iso_date(seconds: u64) -> String {
    let utc = Utc::of(UNIX_EPOCH + Duration::from_secs(seconds), false);
    utc.iso8601().replace('Z', ".000Z")
}

/// One multipart upload in progress.
#[derive(Debug)]
struct Upload {
    key: String,
    /// The second the upload was started, by the clock of the service.
    initiated: u64,
    parts: BTreeMap<u32, (String, Kept)>,
}

/// An object held, with the second it was last written.
#[derive(Debug)]
struct Stored {
    kept: Kept,
    modified: u64,
    /// The entity tag of an object made of parts; none for an object of one PUT.
    joined: Option<String>,
}

impl Stored {
    /// The entity tag of the object.
    fn etag(&self) -> String {
        self.joined.clone().unwrap_or_else(|| etag_of(&self.kept))
    }
}

#[derive(Debug, Default)]
struct State {
    objects: BTreeMap<String, Stored>,
    /// The second objects are stamped with, when a test set one ([`FakeS3::set_time`]); the
    /// system clock's otherwise.
    time: Option<u64>,
    uploads: BTreeMap<String, Upload>,
    next: u64,
    failures: Vec<Failure>,
    stats: FakeStats,
    requests: Vec<String>,
    /// The only key pair a request may be signed with, when a test set one.
    credentials: Option<(String, String)>,
    /// The signing regions of the requests so far.
    regions: std::collections::BTreeSet<String>,
    /// The least size of a part that is not the last of its upload, when a test set one
    /// ([`FakeS3::set_min_part_bytes`]); [`super::MIN_PART_BYTES`] otherwise.
    min_part: Option<u64>,
    /// The next AbortMultipartUpload is to be held ([`FakeS3::hold_next_abort`]).
    hold_abort: bool,
    /// AbortMultipartUpload requests held and not yet dropped.
    held_aborts: usize,
}

/// An S3 service on loopback (module docs). It serves until the runtime it was
/// started on stops.
#[derive(Debug, Clone)]
pub struct FakeS3 {
    endpoint: String,
    state: Arc<Mutex<State>>,
}

impl FakeS3 {
    /// Starts the service on a free loopback port, on the current runtime.
    ///
    /// # Panics
    /// No loopback port could be bound.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let endpoint = format!("http://{}", listener.local_addr().expect("address"));
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        tokio::spawn(async move {
            loop {
                let accepted = listener.accept().await;
                let serving = accepted.map(|(sock, _)| serve(sock, Arc::clone(&shared)));
                serving.map(tokio::spawn).ok();
            }
        });
        Self { endpoint, state }
    }

    /// `http://127.0.0.1:<port>`.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// A store for bucket `kbf-fake` on this service, claiming `capabilities`.
    ///
    /// # Panics
    /// Never: the configuration is valid.
    #[must_use]
    pub fn store(&self, capabilities: Capabilities) -> S3Store {
        S3Store::new(S3Config {
            endpoint: self.endpoint.clone(),
            region: "us-east-1".to_owned(),
            bucket: "kbf-fake".to_owned(),
            credentials: Credentials::new("fake-key", "fake-secret"),
            capabilities,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(60),
            min_body_rate: None,
        })
        .expect("a valid configuration")
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// From now on refuses (403) every request not signed with this key pair: the access
    /// key id must be `access_key_id` and the signature must be the one `secret_access_key`
    /// makes, recomputed here over the request as received.
    pub fn require_credentials(&self, access_key_id: &str, secret_access_key: &str) {
        self.lock().credentials = Some((access_key_id.to_owned(), secret_access_key.to_owned()));
    }

    /// The signing regions the requests so far named, sorted.
    #[must_use]
    pub fn regions(&self) -> Vec<String> {
        self.lock().regions.iter().cloned().collect()
    }

    /// Every object key held, in order.
    #[must_use]
    pub fn keys(&self) -> Vec<String> {
        self.lock().objects.keys().cloned().collect()
    }

    /// Fails the next request `failure` names, once.
    pub fn fail(&self, failure: Failure) {
        self.lock().failures.push(failure);
    }

    /// From now on takes a part of at least `bytes` bytes where S3 takes one of 5 MiB or
    /// more: every part of an upload but the last has to be that large when the upload is
    /// completed. For a test that sends parts of a few bytes.
    pub fn set_min_part_bytes(&self, bytes: u64) {
        self.lock().min_part = Some(bytes);
    }

    /// Holds the next AbortMultipartUpload request unanswered and unhandled: the
    /// request is read, then waits for its caller to close the connection (it went
    /// away) and is dropped, never having reached the store. Once.
    pub fn hold_next_abort(&self) {
        self.lock().hold_abort = true;
    }

    /// AbortMultipartUpload requests held by [`FakeS3::hold_next_abort`] and not yet
    /// dropped.
    #[must_use]
    pub fn held_aborts(&self) -> usize {
        self.lock().held_aborts
    }

    /// Multipart uploads begun and neither completed nor aborted.
    #[must_use]
    pub fn incomplete_uploads(&self) -> usize {
        self.lock().uploads.len()
    }

    /// What the service took so far.
    #[must_use]
    pub fn stats(&self) -> FakeStats {
        self.lock().stats
    }

    /// The length of the object under `key`, if there is one.
    #[must_use]
    pub fn object_len(&self, key: &ObjectKey) -> Option<u64> {
        self.lock().objects.get(key.as_str()).map(|o| o.kept.len)
    }

    /// From now on stamps the objects it takes with this time, in seconds since the Unix
    /// epoch, instead of the system clock's.
    pub fn set_time(&self, seconds: u64) {
        self.lock().time = Some(seconds);
    }

    /// Every request so far, as `METHOD /path?query`.
    #[must_use]
    pub fn requests(&self) -> Vec<String> {
        self.lock().requests.clone()
    }
}

/// One request: method, path, query pairs, lowercase headers, body.
struct Request {
    method: String,
    path: String,
    query: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

/// Serves one connection's requests, one after another, until it closes.
async fn serve(mut sock: TcpStream, state: Arc<Mutex<State>>) -> Option<()> {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; RUN];
    loop {
        let mut head_end = end_of_head(&buf);
        while head_end.is_none() {
            let n = sock.read(&mut chunk).await.ok().filter(|n| *n > 0)?;
            buf.extend_from_slice(&chunk[..n]);
            head_end = end_of_head(&buf);
        }
        let head_end = head_end?;
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut request = parse_head(&head)?;
        let len: usize = request
            .headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        buf.reserve((head_end + len).saturating_sub(buf.len()));
        while buf.len() < head_end + len {
            let n = sock.read(&mut chunk).await.ok().filter(|n| *n > 0)?;
            buf.extend_from_slice(&chunk[..n]);
        }
        let rest = buf.split_off(head_end + len);
        request.body = buf.split_off(head_end);
        buf = rest;
        let held = {
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            let abort = request.method == "DELETE" && request.query.contains_key("uploadId");
            let held = abort && std::mem::take(&mut state.hold_abort);
            state.held_aborts += usize::from(held);
            held
        };
        if held {
            // Until the caller closes the connection; then the request is lost.
            let _ = tokio::io::copy(&mut sock, &mut tokio::io::sink()).await;
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            state.held_aborts -= 1;
            return None;
        }
        let answer = {
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            handle(&mut state, &request)
        };
        sock.write_all(&answer).await.ok()?;
    }
}

/// Where a request's head ends in `buf`, past its blank line, once it is all there.
fn end_of_head(buf: &[u8]) -> Option<usize> {
    let blank = buf.windows(4).position(|w| w == b"\r\n\r\n");
    blank.map(|at| at + 4)
}

fn parse_head(head: &str) -> Option<Request> {
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let target = first.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (k.to_owned(), v.to_owned())
        })
        .collect();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    Some(Request {
        method,
        path: path.to_owned(),
        query,
        headers,
        body: Vec::new(),
    })
}

fn answer(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    answer_sized(status, body.len() as u64, headers, body)
}

/// An answer whose `content-length` is `length`: a HEAD gives the length of the body it
/// leaves out.
fn answer_sized(status: &str, length: u64, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status}\r\ncontent-length: {length}\r\n");
    for (name, value) in headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

fn error(status: &str, code: &str) -> Vec<u8> {
    let body = format!("<Error><Code>{code}</Code><Message>{code}</Message></Error>");
    answer(status, &[], body.as_bytes())
}

fn handle(state: &mut State, request: &Request) -> Vec<u8> {
    let query: Vec<String> = request
        .query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    let line = format!("{} {}?{}", request.method, request.path, query.join("&"));
    state.requests.push(line);
    let signed_region = request
        .headers
        .get("authorization")
        .and_then(|authorization| credential_region(authorization));
    if let Some(region) = signed_region {
        state.regions.insert(region.to_owned());
    }
    let body_len = request.body.len() as u64;
    state.stats.largest_body = state.stats.largest_body.max(body_len);
    // `/bucket/key`: the key is everything after the bucket.
    let key = request.path.trim_start_matches('/');
    let key = key.split_once('/').map_or("", |(_, k)| k).to_owned();
    let q = |name: &str| request.query.get(name).cloned();
    let conditional = request.headers.get("if-none-match").map(String::as_str) == Some("*");
    let method = request.method.as_str();
    if let Some((id, secret)) = &state.credentials
        && !signed_with(request, id, secret)
    {
        return error("403 Forbidden", "SignatureDoesNotMatch");
    }
    match (method, q("uploads"), q("uploadId"), q("partNumber")) {
        ("POST", Some(_), None, None) => {
            if state.take(Failure::Create) {
                return error("500 Internal Server Error", "InternalError");
            }
            state.next += 1;
            let id = format!("upload-{}", state.next);
            let parts = BTreeMap::new();
            let initiated = state.time.unwrap_or_else(system_seconds);
            let upload = Upload {
                key,
                initiated,
                parts,
            };
            state.uploads.insert(id.clone(), upload);
            let body = format!(
                "<InitiateMultipartUploadResult><UploadId>{id}</UploadId>\
                 </InitiateMultipartUploadResult>"
            );
            answer("200 OK", &[], body.as_bytes())
        }
        ("PUT", None, Some(id), Some(number)) => {
            let number: u32 = number.parse().unwrap_or(0);
            let (failed, no_etag) = (
                state.take(Failure::Part(number)),
                state.take(Failure::NoEtag),
            );
            if failed {
                return error("500 Internal Server Error", "InternalError");
            }
            let upload = state.uploads.get_mut(&id).filter(|up| up.key == key);
            let Some(upload) = upload else {
                return error("404 Not Found", "NoSuchUpload");
            };
            let kept = Kept::of(&request.body);
            let etag = part_tag(&kept);
            upload.parts.insert(number, (etag.clone(), kept));
            state.stats.parts += 1;
            let headers = if no_etag {
                vec![]
            } else {
                vec![("etag", etag)]
            };
            answer("200 OK", &headers, b"")
        }
        ("POST", None, Some(id), None) => complete(state, &id, &key, conditional, &request.body),
        ("DELETE", None, Some(id), None) => {
            if state.take(Failure::Abort) {
                return error("500 Internal Server Error", "InternalError");
            }
            let known = state.uploads.get(&id).is_some_and(|up| up.key == key);
            if !known {
                return error("404 Not Found", "NoSuchUpload");
            }
            state.uploads.remove(&id);
            state.stats.aborted += 1;
            answer("204 No Content", &[], b"")
        }
        ("GET", Some(_), None, None) if key.is_empty() => list_uploads(state, request),
        ("PUT", None, None, None) => put_object(state, key, conditional, request),
        ("DELETE", None, None, None) => {
            state.objects.remove(&key);
            answer("204 No Content", &[], b"")
        }
        ("GET", None, None, None) if key.is_empty() => list(state, request),
        ("GET", None, None, None) => get(state, &key, request.headers.get("range")),
        ("HEAD", None, None, None) => head(state, &key),
        _ => error("501 Not Implemented", "NotImplemented"),
    }
}

/// A PUT of a whole object: refused 412 if `conditional` (`If-None-Match: *`) and the key
/// exists, or if it carries `If-Match` and the object has another entity tag (404 if there is
/// no object). The answer to a PUT with `If-Match` carries the new object's entity tag.
fn put_object(state: &mut State, key: String, conditional: bool, request: &Request) -> Vec<u8> {
    let expected = request.headers.get("if-match");
    if let Some(expected) = expected {
        let Some(stored) = state.objects.get(&key) else {
            return error("404 Not Found", "NoSuchKey");
        };
        if stored.etag() != *expected {
            return error("412 Precondition Failed", "PreconditionFailed");
        }
    }
    if conditional && state.objects.contains_key(&key) {
        return error("412 Precondition Failed", "PreconditionFailed");
    }
    let kept = Kept::of(&request.body);
    let headers = match expected {
        Some(_) => vec![("etag", etag_of(&kept))],
        None => vec![],
    };
    state.store(key, kept, None);
    state.stats.single_puts += 1;
    answer("200 OK", &headers, b"")
}

/// The region in `AWS4-HMAC-SHA256 Credential=<id>/<date>/<region>/s3/aws4_request, ..`.
fn credential_region(authorization: &str) -> Option<&str> {
    let scope = authorization.split("Credential=").nth(1)?;
    scope.split('/').nth(2)
}

/// Whether `request` carries the `Authorization` that `access_key_id` and
/// `secret_access_key` make for it: the signature is computed again from the request as
/// received, with the signer the store signs with.
fn signed_with(request: &Request, access_key_id: &str, secret_access_key: &str) -> bool {
    let header = |name: &str| request.headers.get(name).map(String::as_str);
    let Some(authorization) = header("authorization") else {
        return false;
    };
    let region = credential_region(authorization);
    let signed_names = authorization
        .split("SignedHeaders=")
        .nth(1)
        .and_then(|rest| rest.split(',').next());
    let (Some(region), Some(names), Some(host), Some(date), Some(payload)) = (
        region,
        signed_names,
        header("host"),
        header("x-amz-date"),
        header("x-amz-content-sha256"),
    ) else {
        return false;
    };
    let headers: Vec<(&str, String)> = names
        .split(';')
        .filter(|name| *name != "host")
        .filter_map(|name| Some((name, header(name)?.to_owned())))
        .collect();
    // The signer encodes; what arrived is encoded already.
    let query: Vec<(String, String)> = request
        .query
        .iter()
        .map(|(k, v)| (unescape(k), unescape(v)))
        .collect();
    let query: Vec<(&str, String)> = query.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
    let expected = Signer {
        access_key_id,
        secret_access_key,
        region,
    }
    .authorization(
        &Signed {
            method: &request.method,
            host,
            path: &request.path,
            query: &query,
            headers: &headers,
            payload_sha256: payload,
        },
        date,
    );
    expected == authorization
}

/// `text` with its `%XX` escapes decoded (an escape that is not one stays as it is).
fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escaped = bytes
            .get(at + 1..at + 3)
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .filter(|_| bytes[at] == b'%')
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The object an upload's completion names, and its entity tag: its parts, in the order the
/// body lists them, each with its ETag and every one but the last at least `min_part` bytes;
/// or the S3 error code that refuses it. The entity tag is the quoted hex MD5 of the tags
/// of the parts, a dash and the number of parts.
fn assemble(upload: &Upload, body: &[u8], min_part: u64) -> Result<(Kept, String), &'static str> {
    let text = String::from_utf8_lossy(body);
    let doc = roxmltree::Document::parse(&text).map_err(|_| "MalformedXML")?;
    let field = |part: roxmltree::Node<'_, '_>, name: &str| {
        let node = part.children().find(|n| n.tag_name().name() == name);
        node.and_then(|n| n.text()).map(str::to_owned)
    };
    let parts = doc.root_element().children().filter(|n| n.is_element());
    let named: Vec<(Option<u32>, Option<String>)> = parts
        .map(|p| {
            (
                field(p, "PartNumber").and_then(|n| n.parse().ok()),
                field(p, "ETag"),
            )
        })
        .collect();
    let mut object = Kept::default();
    let mut tags = Md5::new();
    for (i, (number, etag)) in named.iter().enumerate() {
        let kept = number.and_then(|n| upload.parts.get(&n));
        let kept = kept
            .filter(|(e, _)| Some(e) == etag.as_ref())
            .ok_or("InvalidPart")?;
        let small = i + 1 < named.len() && kept.1.len < min_part;
        (!small).then_some(()).ok_or("EntityTooSmall")?;
        object.extend(&kept.1);
        tags.update(kept.0.as_bytes());
    }
    let joined = format!("\"{}-{}\"", hex::encode(tags.finalize()), named.len());
    Ok((object, joined))
}

/// CompleteMultipartUpload ([`assemble`]); conditional on `If-None-Match: *`.
fn complete(state: &mut State, id: &str, key: &str, conditional: bool, body: &[u8]) -> Vec<u8> {
    let min_part = state.min_part.unwrap_or(super::MIN_PART_BYTES);
    let upload = state.uploads.get(id).filter(|u| u.key == key);
    let assembled = upload
        .ok_or("NoSuchUpload")
        .and_then(|u| assemble(u, body, min_part));
    let (object, joined) = match assembled {
        Ok(assembled) => assembled,
        Err(code) => return error("400 Bad Request", code),
    };
    if state.take(Failure::CompleteLate) {
        let body = "<Error><Code>InternalError</Code><Message>late</Message></Error>";
        return answer("200 OK", &[], body.as_bytes());
    }
    if state.take(Failure::Complete) {
        return error("500 Internal Server Error", "InternalError");
    }
    if conditional && state.objects.contains_key(key) {
        return error("412 Precondition Failed", "PreconditionFailed");
    }
    state.uploads.remove(id);
    state.store(key.to_owned(), object, Some(joined.clone()));
    state.stats.completed += 1;
    let done = format!(
        "<CompleteMultipartUploadResult><ETag>{joined}</ETag>\
         </CompleteMultipartUploadResult>"
    );
    answer("200 OK", &[], done.as_bytes())
}

/// A HEAD: the size, entity tag and time of last write of an object, and no body.
fn head(state: &State, key: &str) -> Vec<u8> {
    let Some(stored) = state.objects.get(key) else {
        return answer("404 Not Found", &[], b"");
    };
    let headers = [
        ("etag", stored.etag()),
        ("last-modified", http_date(stored.modified)),
    ];
    answer_sized("200 OK", stored.kept.len, &headers, b"")
}

/// A ListObjectsV2: at most `max-keys` of the keys under `prefix` that sort after the
/// continuation token (the last key of the page before) or, without one, after
/// `start-after`.
fn list(state: &State, request: &Request) -> Vec<u8> {
    let query = |name: &str| request.query.get(name).map(|value| unescape(value));
    let prefix = query("prefix").unwrap_or_default();
    let max_keys: usize = query("max-keys")
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    let token = query("continuation-token");
    let after = token.or_else(|| query("start-after"));
    let after = after.as_deref().unwrap_or("");
    let matching: Vec<(&String, &Stored)> = state
        .objects
        .iter()
        .filter(|(key, _)| key.starts_with(&prefix))
        .filter(|(key, _)| key.as_str() > after)
        .collect();
    let page = &matching[..matching.len().min(max_keys)];
    let mut body = String::from("<ListBucketResult>");
    for (key, stored) in page {
        body.push_str(&format!(
            "<Contents><Key>{key}</Key><Size>{}</Size>\
             <LastModified>{}</LastModified></Contents>",
            stored.kept.len,
            iso_date(stored.modified)
        ));
    }
    let more = page.last().filter(|_| matching.len() > page.len());
    let tail = match more {
        Some((last, _)) => format!(
            "<IsTruncated>true</IsTruncated>\
             <NextContinuationToken>{last}</NextContinuationToken>"
        ),
        None => "<IsTruncated>false</IsTruncated>".to_owned(),
    };
    body.push_str(&tail);
    body.push_str("</ListBucketResult>");
    answer("200 OK", &[], body.as_bytes())
}

/// A GET of the whole object, or of one `bytes=first-last` range.
fn get(state: &State, key: &str, range: Option<&String>) -> Vec<u8> {
    let Some(stored) = state.objects.get(key) else {
        return error("404 Not Found", "NoSuchKey");
    };
    let object = &stored.kept;
    let Some(range) = range else {
        return answer("200 OK", &[], &object.read(0, object.len));
    };
    let parsed = range
        .strip_prefix("bytes=")
        .and_then(|r| r.split_once('-'))
        .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)));
    let Some((first, last)) = parsed.filter(|(f, l)| f < &object.len && f <= l) else {
        return error("416 Range Not Satisfiable", "InvalidRange");
    };
    let end = (last + 1).min(object.len);
    let content_range = format!("bytes {first}-{}/{}", end - 1, object.len);
    let bytes = object.read(first, end);
    answer(
        "206 Partial Content",
        &[("content-range", content_range)],
        &bytes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a request signed over an escaped query (defect: `unescape` leaves `%2F`
    /// alone, so the fake signs the escape again and refuses a request it should take).
    #[test]
    fn escapes_are_decoded_and_what_is_not_one_stays() {
        assert_eq!(unescape("a%2Fb%41"), "a/bA");
        assert_eq!(unescape("100%"), "100%");
        assert_eq!(unescape("%zz%4"), "%zz%4");
        assert_eq!(unescape(""), "");
    }

    /// Catches: kept bytes that read back other than written (a zero run merged with
    /// its neighbour's length wrong, a range cut at the wrong run).
    #[test]
    fn kept_bytes_read_back_as_written() {
        let mut bytes = vec![0u8; 3 * RUN + 10];
        bytes[RUN + 5] = 7;
        bytes[3 * RUN + 9] = 9;
        let kept = Kept::of(&bytes);
        assert_eq!(kept.len, bytes.len() as u64);
        assert_eq!(kept.read(0, kept.len), bytes);
        assert_eq!(kept.read(RUN as u64, RUN as u64 + 6), bytes[RUN..RUN + 6]);
        let mut joined = Kept::of(&bytes[..RUN]);
        joined.extend(&Kept::of(&bytes[RUN..]));
        assert_eq!(joined.read(1, joined.len), bytes[1..]);
    }

    /// Catches: consecutive zero runs kept separately (a 5 GiB object of zeros would
    /// keep tens of thousands of entries), or merged with a wrong total.
    #[test]
    fn consecutive_zero_runs_are_kept_as_one_run() {
        let kept = Kept::of(&vec![0u8; 3 * RUN + 1]);
        assert_eq!(kept.runs.len(), 1);
        assert_eq!(kept.runs[0].as_ref().err(), Some(&(3 * RUN as u64 + 1)));
        assert_eq!(kept.len, 3 * RUN as u64 + 1);
        assert_eq!(kept.read(RUN as u64 - 1, RUN as u64 + 1), [0, 0]);
    }

    /// Asserts that a time is written in the two forms S3 writes it in: the HTTP date, to
    /// the weekday (the documented example, and the epoch, a Thursday), and the ISO form
    /// of a listing with its milliseconds.
    #[test]
    fn times_are_written_as_s3_writes_them() {
        assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(iso_date(1_255_369_830), "2009-10-12T17:50:30.000Z");
    }

    /// Asserts that the tag of a part is quoted and depends on the bytes of the part and
    /// nothing else: the same for the same bytes, another for other bytes, for zeros of
    /// another length, and for the same number of zeros with a one in another place.
    #[test]
    fn the_tag_of_a_part_depends_on_its_bytes() {
        let tag = part_etag(b"hello ");
        assert_eq!(tag, part_etag(b"hello "));
        assert_ne!(tag, part_etag(b"HELLO "));
        assert!(tag.starts_with('"'), "{tag}");
        assert!(tag.ends_with('"'), "{tag}");
        assert_eq!(tag.trim_matches('"').len(), 16);
        let zeros = |n: usize| part_etag(&vec![0_u8; n]);
        assert_eq!(zeros(3 * RUN), zeros(3 * RUN));
        assert_ne!(zeros(3 * RUN), zeros(3 * RUN + 1));
        let mut shifted = vec![0_u8; 2 * RUN];
        shifted[RUN] = 1;
        let mut other = vec![0_u8; 2 * RUN];
        other[RUN + 1] = 1;
        assert_ne!(part_etag(&shifted), part_etag(&other));
    }

    /// Asserts that the entity tag is the quoted hex MD5 of the bytes (the tag S3 gives an
    /// object of one PUT), and does not depend on the runs the bytes are kept in.
    #[test]
    fn the_entity_tag_is_the_quoted_md5_of_the_bytes() {
        let whole = Kept::of(b"abc");
        assert_eq!(etag_of(&whole), "\"900150983cd24fb0d6963f7d28e17f72\"");
        let mut joined = Kept::of(b"a");
        joined.extend(&Kept::of(b"bc"));
        assert_eq!(etag_of(&joined), etag_of(&whole));
    }
}
