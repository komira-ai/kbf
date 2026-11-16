//! The signature of a call, as tierstore's trees interface checks it: AWS Signature Version 4
//! with the node's access keys, the method `POST`, the gRPC path, no query, and the signed
//! headers `host` (the call's authority), `x-amz-content-sha256` (the hex SHA-256 of the whole
//! request body, the gRPC frames) and `x-amz-date`, in the scope `<date>/<region>/s3`. The
//! secret signs and is never sent.
//!
//! The functions here are the whole of SigV4 the client needs; the fake checks a call with the
//! same canonical request. The tests hold them to the values an
//! independent SigV4 implementation gives, and HMAC to RFC 4231's vectors.

use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest as _, Sha256};

/// The algorithm, as a signature names it.
pub(crate) const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// The service of a signature's scope: tierstore checks the trees interface as its S3 front.
pub(crate) const SERVICE: &str = "s3";

/// The headers a call signs, sorted, as `SignedHeaders` lists them.
pub(crate) const SIGNED: &str = "host;x-amz-content-sha256;x-amz-date";

/// The block size of SHA-256, which HMAC pads its key to.
const BLOCK: usize = 64;

/// HMAC-SHA256 of `data` under `key` (RFC 2104): a key longer than a block is hashed first.
pub(crate) fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut block = [0_u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let padded = |pad: u8| block.map(|b| b ^ pad);
    let inner = Sha256::new()
        .chain_update(padded(0x36))
        .chain_update(data)
        .finalize();
    Sha256::new()
        .chain_update(padded(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// The hex SHA-256 of `bytes`, as `x-amz-content-sha256` carries it.
pub(crate) fn payload_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// `path` as SigV4's canonical URI writes it: every byte but the unreserved ones
/// (`A-Z a-z 0-9 - _ . ~`) and `/` as `%XX`.
pub(crate) fn canonical_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~/".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// The canonical request of a call with no query: `method`, `path`, the `headers` (name and
/// value, sorted by name, lower case), the names they sign, and the payload hash.
pub(crate) fn canonical_request(
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    payload: &str,
) -> String {
    let mut text = format!("{method}\n{}\n\n", canonical_path(path));
    for (name, value) in headers {
        let _ = writeln!(text, "{name}:{}", value.trim());
    }
    let names: Vec<&str> = headers.iter().map(|(name, _)| *name).collect();
    let _ = write!(text, "\n{}\n{payload}", names.join(";"));
    text
}

/// The signature, in hex, of `canonical` made at `stamp` (`x-amz-date`) in the scope of
/// `region` and `service`, with `secret`.
pub(crate) fn signature(
    secret: &str,
    (region, service): (&str, &str),
    stamp: &str,
    canonical: &str,
) -> String {
    let date = stamp.get(..8).unwrap_or(stamp);
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let digest = hex::encode(Sha256::digest(canonical.as_bytes()));
    let to_sign = format!("{ALGORITHM}\n{stamp}\n{scope}\n{digest}");
    let mut key = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    for part in [region, service, "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    hex::encode(hmac(&key, to_sign.as_bytes()))
}

/// The `Authorization` header of a call signed with key `id`.
pub(crate) fn authorization(id: &str, region: &str, stamp: &str, signature: &str) -> String {
    let date = stamp.get(..8).unwrap_or(stamp);
    format!(
        "{ALGORITHM} Credential={id}/{date}/{region}/{SERVICE}/aws4_request, \
         SignedHeaders={SIGNED}, Signature={signature}"
    )
}

/// What a call's head carries: the three headers to set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Head {
    /// `x-amz-date`.
    pub(crate) stamp: String,
    /// `x-amz-content-sha256`.
    pub(crate) payload: String,
    /// `authorization`.
    pub(crate) authorization: String,
}

/// The head of a `POST` of `body` to `path` on `authority`, signed at `at` with key `(id,
/// secret)` in `region`.
pub(crate) fn sign(
    (id, secret): (&str, &str),
    region: &str,
    authority: &str,
    path: &str,
    body: &[u8],
    at: SystemTime,
) -> Head {
    let stamp = amz_date(at);
    let payload = payload_hash(body);
    let headers = [
        ("host", authority),
        ("x-amz-content-sha256", payload.as_str()),
        ("x-amz-date", stamp.as_str()),
    ];
    let canonical = canonical_request("POST", path, &headers, &payload);
    let signed = signature(secret, (region, SERVICE), &stamp, &canonical);
    let authorization = authorization(id, region, &stamp, &signed);
    Head {
        stamp,
        payload,
        authorization,
    }
}

/// `at` as `x-amz-date` spells it, `YYYYMMDDTHHMMSSZ` in UTC. A time before the epoch is
/// spelled as the epoch.
pub(crate) fn amz_date(at: SystemTime) -> String {
    let secs = at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let (year, month, day) = civil(days);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// The year, month and day of the `days`th day after the Unix epoch, in the proleptic
/// Gregorian calendar (Howard Hinnant's `civil_from_days`, for days on or after the epoch).
pub(crate) fn civil(days: u64) -> (u64, u64, u64) {
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

#[cfg(test)]
#[path = "sign_tests.rs"]
mod tests;
