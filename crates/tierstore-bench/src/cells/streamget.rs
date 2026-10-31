//! A signed GET that is read as it arrives, to time the first byte and the last.
//!
//! The S3 client of `objstore-s3` answers a read with the body whole, so it cannot say when the
//! first byte came. This one sends the same request (path-style, SigV4, plain HTTP, the
//! `x-amz-content-sha256` of an empty body) and reads the answer chunk by chunk. Its signer is a
//! copy of that crate's, which keeps it private; the test below holds it to the example of the
//! S3 documentation for "GET Object" with a `Range` header, the same one that crate's signer is
//! held to.
//!
//! Like every call of [`crate::s3::BenchS3`], a read is timed, written to the
//! [`crate::requestlog::RequestLog`] before it returns, and held to the rate cap; a failure at
//! the store or on the wire is a measurement ([`Streamed::error`]), not an error of the call.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac as _};
use reqwest::header::{AUTHORIZATION, HeaderName, HeaderValue};
use sha2::{Digest as _, Sha256};

use crate::cells::budget::Throttle;
use crate::config::StoreSpec;
use crate::error::BenchError;
use crate::requestlog::{RequestLog, RequestRecord};
use crate::role::Role;
use crate::s3::{Timeouts, unix_ms};

/// The hash of an empty body, as SigV4 spells it.
pub(crate) const EMPTY_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// What one streamed read did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Streamed {
    /// When it was sent, milliseconds since the Unix epoch.
    pub started_unix_ms: u64,
    /// Until the first byte of the body arrived; `None` if none did.
    pub ttfb: Option<Duration>,
    /// Until the body ended or the read failed.
    pub total: Duration,
    /// Bytes of the body that arrived.
    pub bytes: u64,
    /// The status of the answer, if there was one.
    pub status: Option<u16>,
    /// A short class of failure (`not-found`, `service`, `transport`, `body`); `None` for a
    /// whole answer to the request.
    pub error: Option<&'static str>,
}

impl Streamed {
    /// Whether the read succeeded.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

/// A client for one bucket of one store role, for reads that are streamed.
pub struct StreamGet {
    role: Role,
    bucket: String,
    base: String,
    host: String,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    client: reqwest::Client,
    log: Arc<RequestLog>,
    cap: Option<Arc<Throttle>>,
}

impl StreamGet {
    /// A client for the bucket `spec` names, logging to `log`. Does no I/O.
    ///
    /// # Errors
    /// [`BenchError::Request`] if the endpoint is not `http://host[:port]`;
    /// [`BenchError::Setup`] if the HTTP client cannot be made.
    pub fn new(
        role: Role,
        spec: &StoreSpec,
        timeouts: Timeouts,
        log: Arc<RequestLog>,
    ) -> Result<Self, BenchError> {
        let url = reqwest::Url::parse(&spec.endpoint)
            .ok()
            .filter(|u| u.scheme() == "http")
            .ok_or(BenchError::Request("endpoint must be http://host[:port]"))?;
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
            None => url.host_str().unwrap_or_default().to_owned(),
        };
        let client = reqwest::Client::builder()
            .connect_timeout(timeouts.connect)
            .timeout(timeouts.request)
            .build()
            .map_err(|e| BenchError::Setup(format!("http client: {e}")))?;
        Ok(Self {
            role,
            bucket: spec.bucket.clone(),
            base: spec.endpoint.trim_end_matches('/').to_owned(),
            host,
            region: spec.region.clone(),
            access_key_id: spec.access_key_id.clone(),
            secret_access_key: spec.secret_access_key.expose().to_owned(),
            client,
            log,
            cap: None,
        })
    }

    /// The same client, held to `cap`; the wait is not part of a read's times.
    #[must_use]
    pub fn with_cap(mut self, cap: Arc<Throttle>) -> Self {
        self.cap = Some(cap);
        self
    }

    /// Reads `key`, the whole object or `range` (`offset`, `len`), as it arrives.
    ///
    /// # Errors
    /// [`BenchError`] if the key is malformed or the log cannot be written; whatever goes wrong
    /// at the store is in the [`Streamed`] that comes back.
    pub async fn get(&self, key: &str, range: Option<(u64, u64)>) -> Result<Streamed, BenchError> {
        objstore_s3::ObjectKey::new(key)?;
        if let Some(cap) = &self.cap {
            cap.acquire().await;
        }
        let started_unix_ms = unix_ms();
        let clock = Instant::now();
        let range_header =
            range.map(|(offset, len)| format!("bytes={offset}-{}", offset + len.max(1) - 1));
        let mut headers: Vec<(&str, String)> = vec![
            ("x-amz-content-sha256", EMPTY_SHA256.to_owned()),
            ("x-amz-date", amz_date(SystemTime::now())),
        ];
        headers.extend(range_header.map(|r| ("range", r)));
        let path = format!("/{}/{key}", self.bucket);
        let authorization = authorization(
            &Credentials {
                access_key_id: &self.access_key_id,
                secret_access_key: &self.secret_access_key,
                region: &self.region,
            },
            &self.host,
            &path,
            &headers,
        );
        let mut request = self.client.get(format!("{}{path}", self.base));
        for (name, value) in &headers {
            request = request.header(
                HeaderName::from_static(name),
                HeaderValue::from_str(value).expect("a signed header value is visible ASCII"),
            );
        }
        request = request.header(AUTHORIZATION, authorization);
        let streamed = self.read(request, started_unix_ms, clock).await;
        self.log.record(&RequestRecord {
            unix_ms: started_unix_ms,
            role: self.role,
            method: "GET".to_owned(),
            bucket: self.bucket.clone(),
            key: key.to_owned(),
            status: streamed.status,
            latency_us: u64::try_from(streamed.total.as_micros()).unwrap_or(u64::MAX),
            bytes: streamed.bytes,
            error_kind: streamed.error.map(str::to_owned),
        })?;
        Ok(streamed)
    }

    async fn read(
        &self,
        request: reqwest::RequestBuilder,
        started_unix_ms: u64,
        clock: Instant,
    ) -> Streamed {
        let mut streamed = Streamed {
            started_unix_ms,
            ttfb: None,
            total: Duration::ZERO,
            bytes: 0,
            status: None,
            error: None,
        };
        match request.send().await {
            Err(_) => streamed.error = Some("transport"),
            Ok(mut response) => {
                let status = response.status().as_u16();
                streamed.status = Some(status);
                streamed.error = match status {
                    200 | 206 => None,
                    404 => Some("not-found"),
                    _ => Some("service"),
                };
                while streamed.error.is_none() {
                    match response.chunk().await {
                        Ok(Some(chunk)) => {
                            streamed.bytes += chunk.len() as u64;
                            if streamed.ttfb.is_none() {
                                streamed.ttfb = Some(clock.elapsed());
                            }
                        }
                        Ok(None) => break,
                        Err(_) => streamed.error = Some("body"),
                    }
                }
            }
        }
        streamed.total = clock.elapsed();
        streamed
    }
}

/// The signing identity and scope.
pub(crate) struct Credentials<'a> {
    pub(crate) access_key_id: &'a str,
    pub(crate) secret_access_key: &'a str,
    pub(crate) region: &'a str,
}

/// The `Authorization` header of a GET with no query, signing `host` and `headers` (lowercase
/// names, as they are sent) with the empty-body hash.
pub(crate) fn authorization(
    credentials: &Credentials<'_>,
    host: &str,
    path: &str,
    headers: &[(&str, String)],
) -> String {
    let amz_date = headers
        .iter()
        .find(|(name, _)| *name == "x-amz-date")
        .map(|(_, value)| value.as_str())
        .expect("a request is signed with its date");
    let date = &amz_date[..8];
    let scope = format!("{date}/{}/s3/aws4_request", credentials.region);
    let mut signed: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| ((*name).to_owned(), value.trim().to_owned()))
        .chain([("host".to_owned(), host.to_owned())])
        .collect();
    signed.sort();
    let names = signed
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers: String = signed
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();
    let canonical = format!("GET\n{path}\n\n{canonical_headers}\n{names}\n{EMPTY_SHA256}");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical.as_bytes()))
    );
    let k_date = hmac(
        format!("AWS4{}", credentials.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac(&k_date, credentials.region.as_bytes());
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex::encode(hmac(&k_signing, to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={names}, Signature={signature}",
        credentials.access_key_id
    )
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac =
        <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(key).expect("HMAC takes a key of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// `YYYYMMDDTHHMMSSZ` of `time`.
pub(crate) fn amz_date(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day) = civil_from_days(i64::try_from(seconds / 86_400).unwrap_or(0));
    let of_day = seconds % 86_400;
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60
    )
}

/// Year, month and day of a count of days since 1970-01-01 (proleptic Gregorian), after Howard
/// Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).expect("a day of the month");
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).expect("a month");
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use objstore_s3::s3::fake::FakeS3;
    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpListener;

    use super::*;
    use crate::config::Secret;
    use crate::s3::BenchS3;

    fn spec(endpoint: &str) -> StoreSpec {
        StoreSpec {
            endpoint: endpoint.to_owned(),
            bucket: "kbf-fake".to_owned(),
            region: "us-east-1".to_owned(),
            access_key_id: "fake-key".to_owned(),
            secret_access_key: Secret::new("fake-secret"),
        }
    }

    fn log() -> Arc<RequestLog> {
        Arc::new(RequestLog::to_writer(std::io::sink()))
    }

    /// Catches a slip in the canonical request, the string to sign or the key derivation: the
    /// signature is the documented one for "GET Object" with a Range header.
    #[test]
    fn the_signer_reproduces_the_documented_get_object_example() {
        let headers = [
            ("range", "bytes=0-9".to_owned()),
            ("x-amz-content-sha256", EMPTY_SHA256.to_owned()),
            ("x-amz-date", "20130524T000000Z".to_owned()),
        ];
        let auth = authorization(
            &Credentials {
                access_key_id: "AKIAIOSFODNN7EXAMPLE",
                secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                region: "us-east-1",
            },
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            &headers,
        );
        assert!(auth.ends_with(
            "Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        ));
        assert!(auth.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, "
        ));
    }

    /// Catches calendar arithmetic off by a day at the epoch, a leap day and a month end, and a
    /// date that is not in the form the signer wants.
    #[test]
    fn the_date_is_the_signers_form() {
        let at = |s: u64| UNIX_EPOCH + Duration::from_secs(s);
        assert_eq!(amz_date(at(0)), "19700101T000000Z");
        assert_eq!(amz_date(at(1_369_353_600)), "20130524T000000Z");
        assert_eq!(amz_date(at(951_868_799)), "20000229T235959Z");
        assert_eq!(amz_date(at(4_107_542_400)), "21000301T000000Z");
    }

    /// Catches a read that loses bytes, reports no first byte, or logs nothing: a whole object
    /// and a range of it, each one record in the log.
    #[tokio::test]
    async fn a_read_gives_its_bytes_a_first_byte_time_and_one_log_record() {
        let fake = FakeS3::start().await;
        let sink = Arc::new(RequestLog::to_writer(std::io::sink()));
        let put = BenchS3::new(
            Role::Cold,
            &spec(fake.endpoint()),
            Timeouts::default(),
            sink,
        )
        .unwrap();
        put.put("k/a", Bytes::from(vec![5u8; 100_000]))
            .await
            .unwrap();

        let log = log();
        let get = StreamGet::new(
            Role::Cold,
            &spec(fake.endpoint()),
            Timeouts::default(),
            log.clone(),
        )
        .unwrap()
        .with_cap(Arc::new(Throttle::new(1000)));
        let whole = get.get("k/a", None).await.unwrap();
        assert!(whole.is_ok());
        assert_eq!((whole.bytes, whole.status), (100_000, Some(200)));
        assert!(whole.ttfb.unwrap() <= whole.total);
        let part = get.get("k/a", Some((10, 4096))).await.unwrap();
        assert!(part.is_ok());
        assert_eq!(part.bytes, 4096);
        assert_eq!(log.count(), 2);
    }

    /// Catches a missing key read as an object, and a store that cannot be reached read as an
    /// answer: a 404 is `not-found`, a refused connection is `transport`, neither has a first
    /// byte.
    #[tokio::test]
    async fn a_missing_key_and_a_dead_store_are_measurements() {
        let fake = FakeS3::start().await;
        let get = StreamGet::new(
            Role::Cold,
            &spec(fake.endpoint()),
            Timeouts::default(),
            log(),
        )
        .unwrap();
        let missing = get.get("k/none", None).await.unwrap();
        assert_eq!(missing.error, Some("not-found"));
        assert_eq!(missing.status, Some(404));
        assert!(missing.ttfb.is_none());
        assert!(!missing.is_ok());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let refused = StreamGet::new(Role::Cold, &spec(&dead), Timeouts::default(), log())
            .unwrap()
            .get("k/a", None)
            .await
            .unwrap();
        assert_eq!((refused.error, refused.status), (Some("transport"), None));
    }

    /// Catches an answer cut short being counted as a whole read: a server that promises 100
    /// bytes and sends 10 gives `body`, with the 10 bytes counted and a first byte time.
    #[tokio::test]
    async fn an_answer_cut_short_is_a_body_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut seen = [0u8; 2048];
            tokio::io::AsyncReadExt::read(&mut socket, &mut seen)
                .await
                .unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n0123456789")
                .await
                .unwrap();
        });
        let get = StreamGet::new(Role::Cold, &spec(&endpoint), Timeouts::default(), log()).unwrap();
        let cut = get.get("k/a", None).await.unwrap();
        assert_eq!(cut.error, Some("body"));
        assert_eq!((cut.bytes, cut.status), (10, Some(200)));
        assert!(cut.ttfb.is_some());
    }

    /// Catches a status that is neither success nor 404 passing as one, and the refusals before
    /// any request: an endpoint that is not plain HTTP, and a key the store would refuse.
    #[tokio::test]
    async fn other_statuses_and_bad_input_are_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut seen = [0u8; 2048];
            tokio::io::AsyncReadExt::read(&mut socket, &mut seen)
                .await
                .unwrap();
            socket
                .write_all(b"HTTP/1.1 503 Busy\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let get = StreamGet::new(Role::Cold, &spec(&endpoint), Timeouts::default(), log()).unwrap();
        let busy = get.get("k/a", None).await.unwrap();
        assert_eq!((busy.error, busy.status), (Some("service"), Some(503)));

        for bad in ["https://store.example:9000", "not a url", "http://"] {
            let refused = StreamGet::new(Role::Cold, &spec(bad), Timeouts::default(), log());
            assert!(matches!(refused, Err(BenchError::Request(_))), "{bad}");
        }
        assert!(matches!(
            get.get("a//b", None).await,
            Err(BenchError::Key(_))
        ));
        let portless = StreamGet::new(
            Role::Cold,
            &spec("http://127.0.0.1"),
            Timeouts::default(),
            log(),
        );
        assert!(portless.is_ok());
    }
}
