//! The tierstore client's wire, in process: a [`Dial`] whose connections are in-memory pipes, the
//! far end of each served by a task of the test, as a script says. No socket is opened; what the
//! client sends is the bytes the far end reads.
//!
//! [`Store`] is the far end that answers as a tierstore does: ranged `GET`s of the objects it
//! holds, each request's AWS Signature Version 4 checked over the request as received (with the
//! `hmac` crate, not the client's own HMAC), and refused 403 when it is not the one the key makes.

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use futures::future::BoxFuture;
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream};

use crate::tierstore::client::{Dial, Io};

/// What the far end of one connection does.
#[derive(Clone)]
pub(in crate::tierstore) enum Far {
    /// Reads the request and answers these bytes, then closes.
    Answer(Vec<u8>),
    /// Reads the request and answers as `Store` does.
    Store(Arc<Store>),
    /// Reads the request and never answers, holding the connection open.
    Silent,
    /// Reads the request, answers these bytes (a head and part of a body), and sends nothing
    /// more, holding the connection open.
    Stalls(Vec<u8>),
    /// The connection is refused.
    Refused,
    /// The connection is never made.
    Hangs,
}

/// A request as the far end read it: its head, verbatim.
pub(in crate::tierstore) type Seen = Arc<Mutex<Vec<String>>>;

/// A [`Dial`] that serves each connection as the next [`Far`] of its script says (the last one
/// again once the script is used up), noting the addresses dialled and the requests read.
pub(in crate::tierstore) struct Scripted {
    script: Mutex<VecDeque<Far>>,
    last: Mutex<Option<Far>>,
    pub(in crate::tierstore) dialled: Mutex<Vec<SocketAddr>>,
    pub(in crate::tierstore) seen: Seen,
}

impl Scripted {
    pub(in crate::tierstore) fn new(script: Vec<Far>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            last: Mutex::new(None),
            dialled: Mutex::default(),
            seen: Seen::default(),
        })
    }

    pub(in crate::tierstore) fn requests(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(in crate::tierstore) fn addresses(&self) -> Vec<SocketAddr> {
        self.dialled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn next(&self) -> Far {
        let mut script = self.script.lock().unwrap_or_else(PoisonError::into_inner);
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(far) = script.pop_front() {
            *last = Some(far);
        }
        last.clone().expect("a script of at least one connection")
    }
}

impl Dial for Scripted {
    fn dial(&self, addr: SocketAddr) -> BoxFuture<'_, std::io::Result<Box<dyn Io>>> {
        self.dialled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(addr);
        let far = self.next();
        let seen = Arc::clone(&self.seen);
        Box::pin(async move {
            match far {
                Far::Refused => Err(std::io::ErrorKind::ConnectionRefused.into()),
                Far::Hangs => std::future::pending().await,
                far => {
                    let (near, end) = tokio::io::duplex(1 << 16);
                    tokio::spawn(serve(end, far, seen));
                    Ok(Box::new(near) as Box<dyn Io>)
                }
            }
        })
    }
}

/// The head of the request on `end`, up to its blank line.
async fn head(end: &mut DuplexStream) -> String {
    let mut read = Vec::new();
    let mut buf = [0_u8; 1024];
    while !read.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = end.read(&mut buf).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        read.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&read).into_owned()
}

/// Serves `end` as `far` says, noting the request in `seen`.
async fn serve(mut end: DuplexStream, far: Far, seen: Seen) {
    let request = head(&mut end).await;
    seen.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(request.clone());
    let answer = match &far {
        Far::Answer(bytes) | Far::Stalls(bytes) => bytes.clone(),
        Far::Store(store) => store.answer(&request),
        _ => Vec::new(),
    };
    let _ = end.write_all(&answer).await;
    if matches!(far, Far::Answer(_) | Far::Store(_)) {
        let _ = end.shutdown().await;
        return;
    }
    // Silent and Stalls hold the connection open, sending nothing more.
    std::future::pending::<()>().await;
}

/// `head` and `body` as an HTTP/1.1 answer that closes its connection.
pub(in crate::tierstore) fn answer(head: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!(
        "{head}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// The value of header `name` of the request head `request`, if it has one.
pub(in crate::tierstore) fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// A tierstore of one bucket, in memory, that takes requests signed with one key pair.
pub(in crate::tierstore) struct Store {
    bucket: String,
    id: String,
    secret: String,
    objects: BTreeMap<String, Vec<u8>>,
}

impl Store {
    pub(in crate::tierstore) fn new(bucket: &str, id: &str, secret: &str) -> Self {
        Self {
            bucket: bucket.to_owned(),
            id: id.to_owned(),
            secret: secret.to_owned(),
            objects: BTreeMap::new(),
        }
    }

    pub(in crate::tierstore) fn holding(mut self, key: &str, bytes: &[u8]) -> Self {
        self.objects.insert(key.to_owned(), bytes.to_vec());
        self
    }

    /// The answer to the request head `request`.
    fn answer(&self, request: &str) -> Vec<u8> {
        if !self.signed(request) {
            return answer("HTTP/1.1 403 Forbidden", b"");
        }
        let path = request.split(' ').nth(1).unwrap_or("");
        let key = path.strip_prefix(&format!("/{}/", self.bucket));
        let Some(object) = key.and_then(|key| self.objects.get(key)) else {
            return answer("HTTP/1.1 404 Not Found", b"");
        };
        let range = header(request, "range").and_then(|r| r.strip_prefix("bytes="));
        let (first, last) = range.and_then(|r| r.split_once('-')).expect("a range");
        let parse = |n: &str| n.parse::<usize>().expect("a byte offset");
        let (first, last) = (parse(first), parse(last));
        if first >= object.len() {
            return answer("HTTP/1.1 416 Range Not Satisfiable", b"");
        }
        let last = last.min(object.len() - 1);
        let head = format!(
            "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes {first}-{last}/{}",
            object.len()
        );
        answer(&head, &object[first..=last])
    }

    /// Whether `request` carries the signature the key pair makes over it as received.
    fn signed(&self, request: &str) -> bool {
        let auth = header(request, "authorization").expect("a signed request");
        let field = |name: &str| {
            auth.split([' ', ','])
                .find_map(|part| part.strip_prefix(name))
                .unwrap_or("")
                .to_owned()
        };
        let (credential, signed, signature) = (
            field("Credential="),
            field("SignedHeaders="),
            field("Signature="),
        );
        let mut scope = credential.splitn(2, '/');
        let (id, scope) = (scope.next().unwrap_or(""), scope.next().unwrap_or(""));
        let date = header(request, "x-amz-date").unwrap_or("");
        let mut parts = scope.split('/');
        let (day, region) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let mut line = request.lines().next().unwrap_or("").split(' ');
        let (method, path) = (line.next().unwrap_or(""), line.next().unwrap_or(""));
        let headers: String = signed
            .split(';')
            .map(|name| format!("{name}:{}\n", header(request, name).unwrap_or("")))
            .collect();
        let payload = header(request, "x-amz-content-sha256").unwrap_or("");
        let canonical = format!("{method}\n{path}\n\n{headers}\n{signed}\n{payload}");
        let digest = hex::encode(Sha256::digest(canonical.as_bytes()));
        let to_sign = format!("AWS4-HMAC-SHA256\n{date}\n{scope}\n{digest}");
        let mut key = mac(format!("AWS4{}", self.secret).as_bytes(), day.as_bytes());
        for part in [region.as_bytes(), b"s3", b"aws4_request"] {
            key = mac(&key, part);
        }
        id == self.id && hex::encode(mac(&key, to_sign.as_bytes())) == signature
    }
}

/// HMAC-SHA256 with the `hmac` crate.
fn mac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).expect("HMAC takes any key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}
