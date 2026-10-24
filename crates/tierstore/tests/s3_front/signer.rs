//! A SigV4 signer for the requests of these tests, written apart from the server's check of
//! signatures, so that the two agree because both follow S3 and not because they share code.

use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

/// The date header at the fixed time of the server whose clock is set by hand
/// (`support::FIXED_MILLIS`).
pub const FIXED_STAMP: &str = "20270115T080000Z";

/// A request to sign. The host, the payload hash and the date are signed besides `headers`.
pub struct Request<'a> {
    pub method: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query: &'a [(&'a str, &'a str)],
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
    pub stamp: &'a str,
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("a key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// `text` percent-encoded, leaving the unreserved characters and the bytes of `keep`.
pub fn encode(text: &str, keep: &[u8]) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) || keep.contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The headers to send for `request`: its own, `host`, `x-amz-content-sha256`, `x-amz-date`
/// and `authorization`, all in lower case.
pub fn sign(request: &Request<'_>, key_id: &str, secret: &str) -> Vec<(String, String)> {
    let payload = hex::encode(Sha256::digest(request.body));
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    headers.push(("host".to_owned(), request.host.to_owned()));
    headers.push(("x-amz-content-sha256".to_owned(), payload.clone()));
    headers.push(("x-amz-date".to_owned(), request.stamp.to_owned()));
    headers.sort();
    let signed: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
    let signed = signed.join(";");
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();
    let mut pairs: Vec<(String, String)> = request
        .query
        .iter()
        .map(|(name, value)| (encode(name, b""), encode(value, b"")))
        .collect();
    pairs.sort();
    let pairs: Vec<String> = pairs.iter().map(|(n, v)| format!("{n}={v}")).collect();
    let query = pairs.join("&");
    let (method, path) = (request.method, encode(request.path, b"/"));
    let canonical = format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{payload}");
    let day = &request.stamp[..8];
    let scope = format!("{day}/us-east-1/s3/aws4_request");
    let digest = hex::encode(Sha256::digest(canonical.as_bytes()));
    let to_sign = format!("AWS4-HMAC-SHA256\n{}\n{scope}\n{digest}", request.stamp);
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), day.as_bytes());
    let k_region = hmac(&k_date, b"us-east-1");
    let k_service = hmac(&k_region, b"s3");
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex::encode(hmac(&k_signing, to_sign.as_bytes()));
    let credential = format!("Credential={key_id}/{scope}");
    let authorization = format!(
        "AWS4-HMAC-SHA256 {credential}, SignedHeaders={signed}, Signature={signature}"
    );
    headers.push(("authorization".to_owned(), authorization));
    headers
}

/// The `x-amz-date` form of a time given in seconds since the Unix epoch.
pub fn stamp_of(seconds: u64) -> String {
    let (days, rest) = ((seconds / 86_400) as i64, seconds % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (hour, minute, second) = (rest / 3600, rest % 3600 / 60, rest % 60);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// The `x-amz-date` of this moment.
pub fn now() -> String {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    stamp_of(since.as_secs())
}
