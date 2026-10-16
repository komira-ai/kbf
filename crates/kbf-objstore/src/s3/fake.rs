//! An S3 service on loopback, for tests (feature `testkit`): PUT, ranged GET, HEAD,
//! DELETE, and multipart upload (create, parts, complete, abort), with S3's rules on
//! part sizes enforced at completion and conditional writes (`If-None-Match: *`) on a
//! PUT and a completion.
//!
//! It keeps what a test asks about: the uploads begun and not yet completed or aborted
//! ([`FakeS3::incomplete_uploads`]), how many single PUTs and parts it took and the
//! largest request body ([`FakeS3::stats`]). A test can fail one part
//! ([`FakeS3::fail_part`]) or a completion ([`FakeS3::fail_complete`]).
//!
//! Bytes are kept compactly: a run of zeros is counted, not stored, so an object of
//! many GiB of zeros costs the test nothing to hold. It does not check signatures.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{Credentials, S3Config, S3Store};
use crate::{Capabilities, ObjectKey};

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

/// One multipart upload in progress.
#[derive(Debug)]
struct Upload {
    key: String,
    parts: BTreeMap<u32, (String, Kept)>,
}

#[derive(Debug, Default)]
struct State {
    objects: BTreeMap<String, Kept>,
    uploads: BTreeMap<String, Upload>,
    next: u64,
    fail_parts: BTreeSet<u32>,
    fail_complete: bool,
    stats: FakeStats,
    requests: Vec<String>,
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
            while let Ok((sock, _)) = listener.accept().await {
                tokio::spawn(serve(sock, Arc::clone(&shared)));
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

    /// Answers the next upload of part `number` (of any upload) with a 500.
    pub fn fail_part(&self, number: u32) {
        self.lock().fail_parts.insert(number);
    }

    /// Answers the next completion with a 200 whose body is an `<Error>`, as S3 does
    /// when a completion fails after its answer started; the upload stays open.
    pub fn fail_complete(&self) {
        self.lock().fail_complete = true;
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
        self.lock().objects.get(key.as_str()).map(|k| k.len)
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

async fn serve(mut sock: TcpStream, state: Arc<Mutex<State>>) {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; RUN];
    loop {
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let Some(mut request) = parse_head(&head) else {
            return;
        };
        let len: usize = request
            .headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        buf.reserve((head_end + len).saturating_sub(buf.len()));
        while buf.len() < head_end + len {
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let rest = buf.split_off(head_end + len);
        request.body = buf.split_off(head_end);
        buf = rest;
        let answer = {
            let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
            handle(&mut state, &request)
        };
        if sock.write_all(&answer).await.is_err() {
            return;
        }
    }
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
    let mut out = format!("HTTP/1.1 {status}\r\ncontent-length: {}\r\n", body.len());
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
    state.requests.push(format!(
        "{} {}?{}",
        request.method,
        request.path,
        query.join("&")
    ));
    let body_len = request.body.len() as u64;
    state.stats.largest_body = state.stats.largest_body.max(body_len);
    // `/bucket/key`: the key is everything after the bucket.
    let key = request
        .path
        .trim_start_matches('/')
        .split_once('/')
        .map(|(_, k)| k.to_owned());
    let Some(key) = key.filter(|k| !k.is_empty()) else {
        return match request.method.as_str() {
            "PUT" => answer("200 OK", &[], b""),
            _ => error("403 Forbidden", "AccessDenied"),
        };
    };
    let q = |name: &str| request.query.get(name).cloned();
    let conditional = request.headers.get("if-none-match").map(String::as_str) == Some("*");
    match (request.method.as_str(), q("uploadId"), q("partNumber")) {
        ("POST", None, None) if request.query.contains_key("uploads") => {
            state.next += 1;
            let id = format!("upload-{}", state.next);
            state.uploads.insert(
                id.clone(),
                Upload {
                    key,
                    parts: BTreeMap::new(),
                },
            );
            let body = format!(
                "<InitiateMultipartUploadResult><Bucket>b</Bucket><Key>k</Key>\
                 <UploadId>{id}</UploadId></InitiateMultipartUploadResult>"
            );
            answer("200 OK", &[], body.as_bytes())
        }
        ("PUT", Some(id), Some(number)) => {
            let Ok(number) = number.parse::<u32>() else {
                return error("400 Bad Request", "InvalidArgument");
            };
            if state.fail_parts.remove(&number) {
                return error("500 Internal Server Error", "InternalError");
            }
            let Some(upload) = state.uploads.get_mut(&id) else {
                return error("404 Not Found", "NoSuchUpload");
            };
            if !(1..=10_000).contains(&number) || body_len > super::MAX_PART_BYTES {
                return error("400 Bad Request", "InvalidArgument");
            }
            let etag = format!("\"{id}-{number}-{body_len}\"");
            upload
                .parts
                .insert(number, (etag.clone(), Kept::of(&request.body)));
            state.stats.parts += 1;
            answer("200 OK", &[("etag", etag)], b"")
        }
        ("POST", Some(id), None) => complete(state, &id, &key, conditional, &request.body),
        ("DELETE", Some(id), None) => {
            if state.uploads.remove(&id).is_none() {
                return error("404 Not Found", "NoSuchUpload");
            }
            state.stats.aborted += 1;
            answer("204 No Content", &[], b"")
        }
        ("PUT", None, None) => {
            if conditional && state.objects.contains_key(&key) {
                return error("412 Precondition Failed", "PreconditionFailed");
            }
            state.objects.insert(key, Kept::of(&request.body));
            state.stats.single_puts += 1;
            answer("200 OK", &[], b"")
        }
        ("GET", None, None) => get(state, &key, request.headers.get("range")),
        ("HEAD", None, None) => match state.objects.contains_key(&key) {
            true => answer("200 OK", &[], b""),
            false => answer("404 Not Found", &[], b""),
        },
        ("DELETE", None, None) => {
            state.objects.remove(&key);
            answer("204 No Content", &[], b"")
        }
        _ => error("501 Not Implemented", "NotImplemented"),
    }
}

/// CompleteMultipartUpload: the parts the body names, in order, each with its ETag,
/// every one but the last at least 5 MiB.
fn complete(state: &mut State, id: &str, key: &str, conditional: bool, body: &[u8]) -> Vec<u8> {
    let Some(upload) = state.uploads.get(id) else {
        return error("404 Not Found", "NoSuchUpload");
    };
    if upload.key != key {
        return error("400 Bad Request", "InvalidRequest");
    }
    let text = String::from_utf8_lossy(body);
    let Ok(doc) = roxmltree::Document::parse(&text) else {
        return error("400 Bad Request", "MalformedXML");
    };
    let mut named = Vec::new();
    for part in doc
        .root_element()
        .children()
        .filter(|n| n.tag_name().name() == "Part")
    {
        let field = |name: &str| {
            part.children()
                .find(|n| n.tag_name().name() == name)
                .and_then(|n| n.text())
                .map(str::to_owned)
        };
        let number = field("PartNumber").and_then(|n| n.parse::<u32>().ok());
        let (Some(number), Some(etag)) = (number, field("ETag")) else {
            return error("400 Bad Request", "MalformedXML");
        };
        named.push((number, etag));
    }
    if named.is_empty() || named.windows(2).any(|w| w[0].0 >= w[1].0) {
        return error("400 Bad Request", "InvalidPartOrder");
    }
    let mut object = Kept::default();
    for (i, (number, etag)) in named.iter().enumerate() {
        let Some((kept_etag, bytes)) = upload.parts.get(number) else {
            return error("400 Bad Request", "InvalidPart");
        };
        if kept_etag != etag {
            return error("400 Bad Request", "InvalidPart");
        }
        if i + 1 < named.len() && bytes.len < super::MIN_PART_BYTES {
            return error("400 Bad Request", "EntityTooSmall");
        }
        object.extend(bytes);
    }
    if std::mem::take(&mut state.fail_complete) {
        let body = "<Error><Code>InternalError</Code><Message>late</Message></Error>";
        return answer("200 OK", &[], body.as_bytes());
    }
    if conditional && state.objects.contains_key(key) {
        return error("412 Precondition Failed", "PreconditionFailed");
    }
    state.uploads.remove(id);
    state.objects.insert(key.to_owned(), object);
    state.stats.completed += 1;
    let body =
        "<CompleteMultipartUploadResult><ETag>\"done\"</ETag></CompleteMultipartUploadResult>";
    answer("200 OK", &[], body.as_bytes())
}

/// A GET, whole or of one `bytes=first-last` range.
fn get(state: &State, key: &str, range: Option<&String>) -> Vec<u8> {
    let Some(object) = state.objects.get(key) else {
        return error("404 Not Found", "NoSuchKey");
    };
    let Some(range) = range else {
        return answer("200 OK", &[], &object.read(0, object.len));
    };
    let parsed = range
        .strip_prefix("bytes=")
        .and_then(|r| r.split_once('-'))
        .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)));
    let Some((first, last)) = parsed else {
        return error("400 Bad Request", "InvalidRange");
    };
    if first >= object.len || last < first {
        return error("416 Range Not Satisfiable", "InvalidRange");
    }
    let end = (last + 1).min(object.len);
    let content_range = format!("bytes {first}-{}/{}", end - 1, object.len);
    answer(
        "206 Partial Content",
        &[("content-range", content_range)],
        &object.read(first, end),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
