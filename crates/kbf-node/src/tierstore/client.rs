//! [`TierClient`]: the daemon's client of the tierstore on loopback. It makes one kind of
//! request, a ranged `GET` of an object of the tierstore's bucket, signed with AWS Signature
//! Version 4, and is an [`ObjectStore`] that reads and does nothing else, which is all
//! `kbf_pack::Reader` asks of a store.
//!
//! **The request stays on the host.** The endpoint is a loopback IP address, checked before the
//! client is made ([`super::Endpoint`]), so no name is resolved. The HTTP client is built with
//! proxies off, from the environment and from the system's settings alike (`no_proxy`), so a
//! proxy variable or a macOS proxy setting cannot carry the request off the host, and with
//! redirects off (`Policy::none`), so an answer that points elsewhere is a failed read, never a
//! second request.
//!
//! **No answer but the bytes asked for is taken.** `206` with a `Content-Range` that is the
//! range asked for (cut at the object's end) and a body of exactly its length is the only
//! success; the body is read in chunks and a body longer than the range is refused at the first
//! byte too many, so no more than the range is ever held. `404` is [`ObjectStoreError::NotFound`]
//! and `416` [`ObjectStoreError::InvalidRange`], which the reader takes as a pack that is not
//! there; `200` (the range ignored), a redirect, a `Content-Range` that is missing, does not
//! parse or is another range, and a body of another length are [`ObjectStoreError::Protocol`];
//! any other status is [`ObjectStoreError::Service`]; a request that cannot be sent or whose
//! answer does not come within the request timeout is [`ObjectStoreError::Transport`]. None of
//! these is tried again: the plane reads the blob from the frontend instead.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use reqwest::header::CONTENT_RANGE;
use sha2::{Digest as _, Sha256};

use super::Endpoint;
use crate::pack_read::KeyPair;

/// The SHA-256 of an empty body, in hex: what a `GET` signs as its payload.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// What the client refuses to do: anything but read.
const READ_ONLY: &str = "the tierstore client only reads";

/// A client of one bucket of the tierstore on loopback (the module docs).
pub(crate) struct TierClient {
    http: reqwest::Client,
    /// `http://<authority>`, the endpoint without a last `/`.
    base: String,
    /// The `Host` header the request carries and signs: the endpoint's authority.
    host: String,
    bucket: String,
    keys: KeyPair,
    region: String,
    request_timeout: Duration,
}

impl TierClient {
    /// A client of `bucket` at `endpoint`, signing with `keys` for `region`; no I/O.
    ///
    /// # Errors
    /// The HTTP client cannot be built.
    pub(crate) fn new(
        endpoint: &Endpoint,
        bucket: String,
        keys: KeyPair,
        region: &str,
        timeouts: (Duration, Duration),
    ) -> Result<Self, reqwest::Error> {
        let (connect_timeout, request_timeout) = timeouts;
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect_timeout)
            .build()?;
        Ok(Self {
            http,
            base: format!("http://{}", endpoint.authority),
            host: endpoint.authority.clone(),
            bucket,
            keys,
            region: region.to_owned(),
            request_timeout,
        })
    }

    /// The bytes of `range` of the object `key`, as the module docs say.
    async fn read(&self, key: &ObjectKey, range: ByteRange) -> Result<Bytes, ObjectStoreError> {
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
        let sent = self
            .http
            .get(format!("{}{path}", self.base))
            .header("range", &range_header)
            .header("x-amz-content-sha256", EMPTY_SHA256)
            .header("x-amz-date", &amz_date)
            .header("authorization", authorization)
            .timeout(self.request_timeout)
            .send()
            .await;
        let mut answer = sent.map_err(transport)?;
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
        let mut body = BytesMut::new();
        while let Some(chunk) = answer.chunk().await.map_err(transport)? {
            if (body.len() + chunk.len()) as u64 > want {
                return Err(refused(format!(
                    "the body is longer than the {want} bytes of its range"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        if body.len() as u64 != want {
            let got = body.len();
            return Err(refused(format!(
                "the body has {got} of the {want} bytes of its range"
            )));
        }
        Ok(body.freeze())
    }
}

/// A failure to send a request or to read its answer.
fn transport(error: reqwest::Error) -> ObjectStoreError {
    ObjectStoreError::Transport(Arc::new(error))
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
