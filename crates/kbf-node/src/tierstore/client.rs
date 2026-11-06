//! [`TierClient`]: the daemon's client of the tierstore on loopback. It makes one kind of
//! request, a ranged `GET` of an object of the tierstore's bucket, signed with AWS Signature
//! Version 4, and is an [`ObjectStore`] that reads and does nothing else, which is all
//! `kbf_pack::Reader` asks of a store.
//!
//! **The request stays on the host.** The endpoint is a loopback IP address, checked before the
//! client is made ([`super::Endpoint`]). Each read opens one connection to that address
//! ([`Dial`]; [`Tcp`] in the daemon) and makes one HTTP/1.1 exchange over it with hyper's
//! connection API: there is no proxy, from the environment or the system, because nothing reads
//! one; no name is resolved; and no redirect is followed, because nothing makes a second request.
//! The `Host` header is sent exactly as it is signed, the endpoint's authority as given.
//!
//! **No answer but the bytes asked for is taken.** `206` with a `Content-Range` that is the
//! range asked for (cut at the object's end) and a body of exactly its length is the only
//! success; the body is read with a limit of that length, so a longer one is refused and no more
//! than the range is ever held. `404` is [`ObjectStoreError::NotFound`] and `416`
//! [`ObjectStoreError::InvalidRange`], which the reader takes as a pack that is not there; `200`
//! (the range ignored), a redirect, a `Content-Range` that is missing, does not parse or is
//! another range, and a body of another length are [`ObjectStoreError::Protocol`]; any other
//! status is [`ObjectStoreError::Service`]; a connection that cannot be made within the connect
//! timeout, an exchange that fails, and a read whose answer and body are not all in within the
//! request timeout are [`ObjectStoreError::Transport`]. None of these is tried again: the plane
//! reads the blob from the frontend instead.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::future::BoxFuture;
use http_body_util::{BodyExt as _, Empty, LengthLimitError, Limited};
use hyper::header::{CONTENT_RANGE, HOST};
use hyper_util::rt::TokioIo;
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};

use super::Endpoint;
use crate::pack_read::KeyPair;

/// The SHA-256 of an empty body, in hex: what a `GET` signs as its payload.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// What the client refuses to do: anything but read.
const READ_ONLY: &str = "the tierstore client only reads";

/// A connection the client makes its exchange over.
pub(crate) trait Io: AsyncRead + AsyncWrite + Send + Unpin + 'static {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Io for T {}

/// How the client reaches the tierstore: one connection to its address for each read.
pub(crate) trait Dial: Send + Sync + 'static {
    /// A connection to `addr`.
    fn dial(&self, addr: SocketAddr) -> BoxFuture<'_, std::io::Result<Box<dyn Io>>>;
}

/// The daemon's [`Dial`]: a TCP connection to the address.
pub(crate) struct Tcp;

impl Dial for Tcp {
    fn dial(&self, addr: SocketAddr) -> BoxFuture<'_, std::io::Result<Box<dyn Io>>> {
        Box::pin(
            async move { Ok(Box::new(tokio::net::TcpStream::connect(addr).await?) as Box<dyn Io>) },
        )
    }
}

/// A client of one bucket of the tierstore on loopback (the module docs).
pub(crate) struct TierClient {
    dial: Arc<dyn Dial>,
    addr: SocketAddr,
    /// The `Host` header the request carries and signs: the endpoint's authority.
    host: String,
    bucket: String,
    keys: KeyPair,
    region: String,
    connect_timeout: Duration,
    request_timeout: Duration,
}

impl TierClient {
    /// A client of `bucket` at `endpoint`, reached through `dial`, signing with `keys` for
    /// `region`, within `timeouts` (connect, request); no I/O.
    pub(crate) fn new(
        endpoint: &Endpoint,
        bucket: String,
        keys: KeyPair,
        region: &str,
        timeouts: (Duration, Duration),
        dial: Arc<dyn Dial>,
    ) -> Self {
        let (connect_timeout, request_timeout) = timeouts;
        Self {
            dial,
            addr: endpoint.addr,
            host: endpoint.authority.clone(),
            bucket,
            keys,
            region: region.to_owned(),
            connect_timeout,
            request_timeout,
        }
    }

    /// The bytes of `range` of the object `key`, as the module docs say: all of it within the
    /// request timeout.
    async fn read(&self, key: &ObjectKey, range: ByteRange) -> Result<Bytes, ObjectStoreError> {
        let within = tokio::time::timeout(self.request_timeout, self.read_once(key, range)).await;
        within.unwrap_or_else(|_| Err(timed_out("answer", self.request_timeout)))
    }

    async fn read_once(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let path = format!("/{}/{key}", self.bucket);
        let range_header = format!("bytes={}-{}", range.offset(), range.last());
        let amz_date = amz_date(SystemTime::now());
        let authorization = sign(&Signed {
            host: &self.host,
            path: &path,
            range: &range_header,
            amz_date: &amz_date,
            id: &self.keys.id,
            secret: &self.keys.secret,
            region: &self.region,
        });
        let request = hyper::Request::get(path.as_str())
            .header(HOST, &self.host)
            .header("range", &range_header)
            .header("x-amz-content-sha256", EMPTY_SHA256)
            .header("x-amz-date", &amz_date)
            .header("authorization", authorization)
            .body(Empty::<Bytes>::new());
        let request = request
            .map_err(|e| ObjectStoreError::Protocol(format!("ranged read of {key}: {e}")))?;
        let dialled = tokio::time::timeout(self.connect_timeout, self.dial.dial(self.addr)).await;
        let io = dialled
            .map_err(|_| timed_out("connection", self.connect_timeout))?
            .map_err(transport)?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(io))
            .await
            .map_err(transport)?;
        // The connection is driven beside the exchange; its failure is the exchange's.
        tokio::spawn(async move { drop(connection.await) });
        let answer = sender.send_request(request).await.map_err(transport)?;
        let status = answer.status().as_u16();
        let refused =
            |why: String| ObjectStoreError::Protocol(format!("ranged read of {key}: {why}"));
        match status {
            206 => {}
            404 => return Err(ObjectStoreError::NotFound(key.clone())),
            416 => {
                return Err(ObjectStoreError::InvalidRange {
                    key: key.clone(),
                    range,
                });
            }
            200 => return Err(refused("answered 200: the store ignored Range".to_owned())),
            300..=399 => {
                return Err(refused(format!(
                    "answered {status}: redirects are not followed"
                )));
            }
            _ => {
                return Err(ObjectStoreError::Service {
                    status,
                    code: String::new(),
                    message: format!("ranged read of {key} answered {status}"),
                });
            }
        }
        let content_range = answer
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok());
        let want = partial(content_range, range).map_err(refused)?;
        let limit = usize::try_from(want).unwrap_or(usize::MAX);
        let collected = Limited::new(answer.into_body(), limit).collect().await;
        let body = match collected {
            Ok(body) => body.to_bytes(),
            Err(error) if error.is::<LengthLimitError>() => {
                return Err(refused(format!(
                    "the body is longer than the {want} bytes of its range"
                )));
            }
            Err(error) => return Err(ObjectStoreError::Transport(Arc::from(error))),
        };
        if body.len() as u64 != want {
            let got = body.len();
            return Err(refused(format!(
                "the body has {got} of the {want} bytes of its range"
            )));
        }
        Ok(body)
    }
}

/// A failure to connect, or of the exchange.
fn transport(error: impl std::error::Error + Send + Sync + 'static) -> ObjectStoreError {
    ObjectStoreError::Transport(Arc::new(error))
}

/// The `what` of a read that did not come within `limit`.
fn timed_out(what: &str, limit: Duration) -> ObjectStoreError {
    let why = format!("the tierstore's {what} did not come within {limit:?}");
    transport(std::io::Error::new(std::io::ErrorKind::TimedOut, why))
}

/// The length of the body a `206` answer to `range` must have, from its `Content-Range`: the
/// range asked for, cut at the object's end.
///
/// # Errors
/// What is wrong with the header.
fn partial(content_range: Option<&str>, range: ByteRange) -> Result<u64, String> {
    let header = content_range.ok_or("206 without Content-Range")?;
    let parsed = header.strip_prefix("bytes ").and_then(|rest| {
        let (span, total) = rest.split_once('/')?;
        let (first, last) = span.split_once('-')?;
        let number = |text: &str| text.parse::<u64>().ok();
        Some((number(first)?, number(last)?, number(total)?))
    });
    let Some((first, last, total)) = parsed else {
        return Err(format!("Content-Range {header:?} does not parse"));
    };
    let want_last = range.last().min(total.saturating_sub(1));
    if first != range.offset() || last != want_last || last < first {
        let (offset, end) = (range.offset(), range.last());
        return Err(format!(
            "asked for bytes {offset}-{end} and got Content-Range {header:?}"
        ));
    }
    Ok(last - first + 1)
}

/// What one request signs.
struct Signed<'a> {
    host: &'a str,
    path: &'a str,
    range: &'a str,
    amz_date: &'a str,
    id: &'a str,
    secret: &'a str,
    region: &'a str,
}

/// The `Authorization` header of a ranged `GET` of `req.path`, AWS Signature Version 4 in its
/// header form: the headers `host`, `range`, `x-amz-content-sha256` and `x-amz-date` are signed,
/// the payload is the empty body, there is no query.
fn sign(req: &Signed<'_>) -> String {
    let date = req.amz_date.get(..8).unwrap_or(req.amz_date);
    let scope = format!("{date}/{}/s3/aws4_request", req.region);
    let signed_headers = "host;range;x-amz-content-sha256;x-amz-date";
    let canonical = format!(
        "GET\n{}\n\nhost:{}\nrange:{}\nx-amz-content-sha256:{EMPTY_SHA256}\nx-amz-date:{}\n\n\
         {signed_headers}\n{EMPTY_SHA256}",
        req.path, req.host, req.range, req.amz_date
    );
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        req.amz_date,
        hex::encode(Sha256::digest(canonical.as_bytes()))
    );
    let key = format!("AWS4{}", req.secret);
    let mut signing = hmac(key.as_bytes(), date.as_bytes());
    for part in [req.region.as_bytes(), b"s3", b"aws4_request"] {
        signing = hmac(&signing, part);
    }
    let signature = hex::encode(hmac(&signing, to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, \
         Signature={signature}",
        req.id
    )
}

/// The block size of SHA-256, which HMAC pads its key to.
const BLOCK: usize = 64;

/// HMAC-SHA256 of `data` under `key` (RFC 2104): a key longer than a block is hashed first.
fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut block = [0_u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let padded = |pad: u8| block.iter().map(|b| b ^ pad).collect::<Vec<u8>>();
    let inner = Sha256::new()
        .chain_update(padded(0x36))
        .chain_update(data)
        .finalize();
    let outer = Sha256::new()
        .chain_update(padded(0x5c))
        .chain_update(inner)
        .finalize();
    outer.to_vec()
}

/// `at` as `x-amz-date` spells it, `YYYYMMDDTHHMMSSZ` in UTC. A time before the epoch is spelled
/// as the epoch.
fn amz_date(at: SystemTime) -> String {
    let secs = at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let (year, month, day) = civil(days);
    let mut text = String::with_capacity(16);
    let _ = write!(
        text,
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    );
    text
}

/// The year, month and day of the `days`th day after the Unix epoch, in the proleptic Gregorian
/// calendar (Howard Hinnant's `civil_from_days`, for days on or after the epoch).
fn civil(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

impl ObjectStore for TierClient {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    async fn put_new(
        &self,
        _: &ObjectKey,
        _: Bytes,
        _: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        Err(ObjectStoreError::Unsupported(READ_ONLY))
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.read(key, range).await
    }

    async fn delete(&self, _: &ObjectKey) -> Result<(), ObjectStoreError> {
        Err(ObjectStoreError::Unsupported(READ_ONLY))
    }

    async fn list(
        &self,
        _: &KeyPrefix,
        _: Option<&ListToken>,
        _: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        Err(ObjectStoreError::Unsupported(READ_ONLY))
    }
}

#[cfg(test)]
mod tests;
