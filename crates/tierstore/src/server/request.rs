//! What the server reads of a request: its path and query, its body, and the hashes the body is
//! held to.

use std::borrow::Cow;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::{Buf, Bytes, BytesMut};
use crc32fast::Hasher as Crc32;
use futures::StreamExt;
use futures::stream::unfold;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper::StatusCode;
use hyper::body::Body;
use hyper::header::HeaderMap;
use md5::Md5;
use percent_encoding::percent_decode_str;
use ring::digest::{Context as Sha1Context, SHA1_FOR_LEGACY_USE_ONLY};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

use crate::{Error, PartBody};

use super::reply::{PAYLOAD_HEADER, S3Error};

/// The error of a request body.
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The headers of checksums that are not supported: no crate for them is among the ones this
/// crate depends on.
const UNSUPPORTED_CHECKSUMS: [&str; 1] = ["x-amz-checksum-crc64nvme"];

/// The header of the one checksum that is held to a body that is not data of an object.
const MD5_HEADER: &str = "content-md5";

/// The value of a header, if there is one and it is text.
pub(crate) fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// What a path names.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// The service: no bucket.
    Root,
    /// The bucket the server serves.
    Bucket,
    /// An object of the bucket, by key.
    Object(String),
}

fn decode(text: &str) -> Option<String> {
    percent_decode_str(text)
        .decode_utf8()
        .ok()
        .map(Cow::into_owned)
}

/// The bucket and the key a path-style path names (`/bucket/key`), decoded.
///
/// # Errors
/// A path that is not percent-encoded UTF-8, or one that names another bucket than `bucket`.
pub(crate) fn resolve(bucket: &str, path: &str) -> Result<Target, S3Error> {
    let decoded = decode(path).ok_or_else(S3Error::invalid_uri)?;
    let rest = decoded.strip_prefix('/').unwrap_or(&decoded);
    let (name, key) = rest.split_once('/').unwrap_or((rest, ""));
    match (name, key) {
        ("", _) => Ok(Target::Root),
        (name, _) if name != bucket => Err(S3Error::no_such_bucket(name)),
        (_, "") => Ok(Target::Bucket),
        (_, key) => Ok(Target::Object(key.to_owned())),
    }
}

/// The pairs of a query string, decoded.
pub(crate) fn query_pairs(raw: Option<&str>) -> Option<Vec<(String, String)>> {
    raw.unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((decode(name)?, decode(value)?))
        })
        .collect()
}

/// The decoded query of a request.
#[derive(Debug)]
pub(crate) struct Query(Vec<(String, String)>);

impl Query {
    pub(crate) fn parse(raw: Option<&str>) -> Result<Self, S3Error> {
        let pairs = query_pairs(raw).ok_or_else(S3Error::invalid_uri)?;
        Ok(Self(pairs))
    }

    /// The value of the first parameter called `name`.
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        let found = self.0.iter().find(|(key, _)| key.as_str() == name);
        found.map(|(_, value)| value.as_str())
    }

    pub(crate) fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Whether any of the parameters is among `names`.
    pub(crate) fn has_any(&self, names: &[&str]) -> bool {
        names.iter().any(|name| self.has(name))
    }

    /// Whether every parameter is among `allowed`.
    pub(crate) fn only(&self, allowed: &[&str]) -> bool {
        self.0
            .iter()
            .all(|(key, _)| allowed.contains(&key.as_str()))
    }
}

/// A request body, read a piece at a time: it has at most a limit of bytes, and it must not be
/// idle: a wait for the next piece that takes longer than the idle timeout ends the read. The
/// time that the whole body takes is not bounded, so a body that is slow and steady is read, but
/// a body may be held to a least average rate ([`Reader::with_floor`]).
pub(crate) struct Reader<B> {
    body: Pin<Box<Limited<B>>>,
    limit: usize,
    idle: Duration,
    floor: Option<Floor>,
}

/// The least average rate of a body, counted from when its reader was made, once the grace
/// has passed.
struct Floor {
    min_bytes_per_sec: u64,
    grace: Duration,
    start: Instant,
    taken: u64,
}

impl Floor {
    /// Counts a piece of `len` bytes, and says whether the body is now slower than the floor.
    fn too_slow(&mut self, len: usize) -> bool {
        self.taken = self.taken.saturating_add(len as u64);
        let elapsed = self.start.elapsed();
        if elapsed <= self.grace {
            return false;
        }
        let wanted = u128::from(self.min_bytes_per_sec) * elapsed.as_millis();
        u128::from(self.taken) * 1000 < wanted
    }
}

impl<B> Reader<B>
where
    B: Body,
    B::Error: Into<BoxError>,
{
    /// A reader of `body` that gives at most `limit` bytes and waits at most `idle` for each
    /// piece.
    pub(crate) fn new(body: B, limit: usize, idle: Duration) -> Self {
        Self {
            body: Box::pin(Limited::new(body, limit)),
            limit,
            idle,
            floor: None,
        }
    }

    /// The same reader, held to an average of at least `min_bytes_per_sec` once `grace` has
    /// passed since it was made (zero: no floor). A piece that makes the average fall under it
    /// ends the read with a `RequestTimeout`.
    pub(crate) fn with_floor(mut self, min_bytes_per_sec: u64, grace: Duration) -> Self {
        self.floor = (min_bytes_per_sec > 0).then(|| Floor {
            min_bytes_per_sec,
            grace,
            start: Instant::now(),
            taken: 0,
        });
        self
    }

    /// The next piece of the body, or `None` at its end.
    ///
    /// # Errors
    /// `RequestTimeout` for no piece within the idle timeout, or for a body under its floor;
    /// `EntityTooLarge` for a body of
    /// more than the limit; `IncompleteBody` for one that could not be read.
    pub(crate) async fn next(&mut self) -> Result<Option<Bytes>, S3Error> {
        loop {
            let waiting = tokio::time::timeout(self.idle, self.body.frame());
            let Ok(frame) = waiting.await else {
                return Err(S3Error::request_timeout(self.idle));
            };
            match frame {
                None => return Ok(None),
                Some(Err(error)) => return Err(self.refusal(&error)),
                Some(Ok(frame)) => {
                    if let Ok(mut data) = frame.into_data() {
                        let piece = data.copy_to_bytes(data.remaining());
                        if let Some(floor) = &mut self.floor
                            && floor.too_slow(piece.len())
                        {
                            return Err(S3Error::too_slow(floor.min_bytes_per_sec));
                        }
                        return Ok(Some(piece));
                    }
                }
            }
        }
    }

    /// The error of a body that failed to be read.
    fn refusal(&self, error: &BoxError) -> S3Error {
        if error.is::<LengthLimitError>() {
            let limit = self.limit;
            let text = format!("The request body is over the limit of {limit} bytes.");
            S3Error::new(StatusCode::PAYLOAD_TOO_LARGE, "EntityTooLarge", text)
        } else {
            let text = "The request body could not be read in full.";
            S3Error::new(StatusCode::BAD_REQUEST, "IncompleteBody", text)
        }
    }

    /// The whole body, if it has at most the limit and comes to the hashes of `check`.
    ///
    /// # Errors
    /// As [`Reader::next`], and the error of [`Check::finish`].
    pub(crate) async fn read_all(mut self, mut check: Check) -> Result<Bytes, S3Error> {
        let mut whole = BytesMut::new();
        while let Some(piece) = self.next().await? {
            check.update(&piece);
            whole.extend_from_slice(&piece);
        }
        check.finish()?;
        Ok(whole.freeze())
    }
}

/// A hash of a body.
enum Hasher {
    Sha256(Sha256),
    Md5(Md5),
    Crc32(Crc32),
    Crc32c(u32),
    Sha1(Sha1Context),
}

impl Hasher {
    /// A SHA-1 hasher: the checksum a client asks for in `x-amz-checksum-sha1` is a SHA-1.
    fn sha1() -> Self {
        Self::Sha1(Sha1Context::new(&SHA1_FOR_LEGACY_USE_ONLY))
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Sha256(hasher) => Digest::update(hasher, data),
            Self::Md5(hasher) => Digest::update(hasher, data),
            Self::Crc32(hasher) => hasher.update(data),
            Self::Crc32c(state) => *state = crc32c::crc32c_append(*state, data),
            Self::Sha1(context) => context.update(data),
        }
    }

    /// The hash, as bytes: the digest, or the big-endian bytes of a CRC.
    fn finish(self) -> Vec<u8> {
        match self {
            Self::Sha256(hasher) => hasher.finalize().to_vec(),
            Self::Md5(hasher) => hasher.finalize().to_vec(),
            Self::Crc32(hasher) => hasher.finalize().to_be_bytes().to_vec(),
            Self::Crc32c(state) => state.to_be_bytes().to_vec(),
            Self::Sha1(context) => context.finish().as_ref().to_vec(),
        }
    }
}

/// A hash that a body must come to, as a header of the request claimed.
struct Want {
    header: &'static str,
    hasher: Hasher,
    expected: Vec<u8>,
}

/// The headers a request may claim a hash of its body in, each with the length in bytes of the
/// hash and a hasher for it.
fn claimable() -> [(&'static str, usize, Hasher); 5] {
    [
        (MD5_HEADER, 16, Hasher::Md5(Md5::new())),
        ("x-amz-checksum-crc32", 4, Hasher::Crc32(Crc32::new())),
        ("x-amz-checksum-crc32c", 4, Hasher::Crc32c(0)),
        ("x-amz-checksum-sha1", 20, Hasher::sha1()),
        ("x-amz-checksum-sha256", 32, Hasher::Sha256(Sha256::new())),
    ]
}

/// What the body of a request must hash to: the hash the signature covers (the
/// `x-amz-content-sha256` header), and each hash the request claims in `Content-MD5`,
/// `x-amz-checksum-crc32`, `x-amz-checksum-crc32c`, `x-amz-checksum-sha1` or
/// `x-amz-checksum-sha256`. The body is fed to the check a piece at a time, so a body of any
/// size is checked without being held.
pub(crate) struct Check(Vec<Want>);

impl Check {
    /// The check of a request that claims `claim` as the hash of its body in
    /// `x-amz-content-sha256`, and the hashes of its checksum headers. `data` says whether the
    /// body is the data of an object or of a part: only then are the `x-amz-checksum-*` headers
    /// read, since on any other request (the completion of an upload) they are the checksums of
    /// the object the call makes, not of its body. `Content-MD5` is held to every body.
    ///
    /// # Errors
    /// `NotImplemented` for a signed or chunked payload and, for the data of an object, for a
    /// checksum of an algorithm that is not supported (`x-amz-checksum-crc64nvme`);
    /// `InvalidDigest` for a checksum header whose value is not the base64 of a hash of the
    /// right length.
    pub(crate) fn new(claim: &str, headers: &HeaderMap, data: bool) -> Result<Self, S3Error> {
        let mut wants = Vec::new();
        match claim {
            "UNSIGNED-PAYLOAD" => {}
            _ if claim.starts_with("STREAMING-") => {
                let text = "signed or chunked payloads are not supported";
                return Err(S3Error::unsupported(text));
            }
            _ => wants.push(Want {
                header: PAYLOAD_HEADER,
                hasher: Hasher::Sha256(Sha256::new()),
                expected: hex::decode(claim).unwrap_or_default(),
            }),
        }
        for name in UNSUPPORTED_CHECKSUMS {
            if data && headers.contains_key(name) {
                let text = format!("{name} is not supported; use sha256, crc32, crc32c or md5");
                return Err(S3Error::unsupported(text));
            }
        }
        for (name, length, hasher) in claimable() {
            let Some(value) = headers.get(name).filter(|_| data || name == MD5_HEADER) else {
                continue;
            };
            let text = value.to_str().unwrap_or_default();
            let decoded = STANDARD.decode(text.trim()).ok();
            let Some(expected) = decoded.filter(|bytes| bytes.len() == length) else {
                return Err(S3Error::invalid_digest(name));
            };
            wants.push(Want {
                header: name,
                hasher,
                expected,
            });
        }
        Ok(Self(wants))
    }

    /// Feeds the next piece of the body to the hashes.
    pub(crate) fn update(&mut self, data: &[u8]) {
        for want in &mut self.0 {
            want.hasher.update(data);
        }
    }

    /// Compares the hashes of the whole body with the ones the request claimed.
    ///
    /// # Errors
    /// `BadDigest` for a checksum header whose hash is not the body's, and
    /// `XAmzContentSHA256Mismatch` for the hash the signature covers.
    pub(crate) fn finish(self) -> Result<(), S3Error> {
        for want in self.0 {
            if want.hasher.finish() != want.expected {
                return Err(S3Error::mismatch(want.header));
            }
        }
        Ok(())
    }
}

/// The answer to send for a streamed body that was refused (its hashes did not match, it was
/// too large, it stalled), which the backend sees only as a body that failed.
#[derive(Clone, Default)]
pub(crate) struct Refused(Arc<Mutex<Option<S3Error>>>);

impl Refused {
    fn slot(&self) -> MutexGuard<'_, Option<S3Error>> {
        // No method panics while holding the lock, so a poisoned slot is still whole.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, error: S3Error) {
        *self.slot() = Some(error);
    }

    /// The error that refused the body, if one did.
    pub(crate) fn take(&self) -> Option<S3Error> {
        self.slot().take()
    }
}

/// The pieces of a request body as the stream the backend stores a part from, held to `check`:
/// a body that is refused ends the stream with an error, after the pieces that were fine, and
/// `refused` has the answer to send. A backend stores nothing of a part whose stream fails, so
/// a part whose hashes do not match is not stored.
pub(crate) fn pieces<'a, B>(reader: Reader<B>, check: Check, refused: Refused) -> PartBody<'a>
where
    B: Body + Send + 'a,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    let state = (reader, Some(check), refused);
    unfold(state, |(mut reader, check, refused)| async move {
        let mut check = check?;
        let outcome = match reader.next().await {
            Ok(Some(piece)) => {
                check.update(&piece);
                return Some((Ok(piece), (reader, Some(check), refused)));
            }
            Ok(None) => check.finish(),
            Err(error) => Err(error),
        };
        refused.set(outcome.err()?);
        let failure = Error::Other("the request body was refused".to_owned());
        Some((Err(failure), (reader, None, refused)))
    })
    .boxed()
}
