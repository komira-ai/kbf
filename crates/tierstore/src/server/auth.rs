//! AWS Signature Version 4 for S3, header form, checked on the server's side.
//!
//! A request is accepted when it carries the `Authorization` header of one of the configured
//! access keys (the key is picked by the id in the credential), signed over `host`,
//! `x-amz-content-sha256` and `x-amz-date` at least, made within 15 minutes of the server's
//! clock, with the signature that the secret of that key gives. The role of the key comes with
//! the answer, for the caller to hold the call to it. A request with no `Authorization` header is checked as a presigned URL (see `presign`); signed
//! streams are not accepted.

use hmac::{Hmac, KeyInit, Mac};
use hyper::StatusCode;
use hyper::http::request::Parts;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use sha2::{Digest, Sha256};

use super::ServerConfig;
use super::grant::{self, GRANT_HEADER};
use super::keys::Role;
use super::presign::{self, Grant, PRESIGN_ONLY};
use super::reply::S3Error;
use super::request::{header, query_pairs};
use super::time::amz_date;

/// How far the time of a request may be from the server's, either way.
const SKEW_MILLIS: u64 = 15 * 60 * 1000;

/// The headers a request signs at least.
const REQUIRED: [&str; 3] = ["host", "x-amz-content-sha256", "x-amz-date"];

/// What SigV4 leaves unencoded in a query, and in a path but for the `/`.
pub(super) const UNRESERVED: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

pub(super) const PATH: &AsciiSet = &UNRESERVED.remove(b'/');

/// Why a request was refused, as the error S3 sends.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub(crate) code: &'static str,
    pub(crate) message: &'static str,
}

pub(crate) const UNSIGNED: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: requests must be signed with AWS Signature Version 4.",
};

pub(crate) const MALFORMED: Refusal = Refusal {
    code: "AuthorizationHeaderMalformed",
    message: "The Authorization header, or the scope of its credential, is malformed.",
};

pub(crate) const UNKNOWN_KEY: Refusal = Refusal {
    code: "InvalidAccessKeyId",
    message: "The access key id or the region is not one this server accepts.",
};

pub(crate) const WEAK: Refusal = Refusal {
    code: "AccessDenied",
    message: "The host, x-amz-content-sha256 and x-amz-date headers must be signed.",
};

pub(crate) const SKEWED: Refusal = Refusal {
    code: "RequestTimeTooSkewed",
    message: "The time of the request is too far from the time of the server.",
};

pub(crate) const MISMATCH: Refusal = Refusal {
    code: "SignatureDoesNotMatch",
    message: "The request signature does not match the one calculated from the request.",
};

pub(crate) const READ_ONLY: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: this access key may read objects and list the bucket only.",
};

/// A request whose signature is good: the hash of its body that the signature covers, and the
/// role of the key that signed it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Signed {
    pub(crate) payload: String,
    pub(crate) role: Role,
    /// What a presigned URL is limited to, if it was signed by a presigning key.
    pub(crate) presign: Option<Grant>,
}

impl From<Refusal> for S3Error {
    fn from(refusal: Refusal) -> Self {
        Self::new(StatusCode::FORBIDDEN, refusal.code, refusal.message)
    }
}

/// The fields of an `Authorization` header.
#[derive(Debug, PartialEq, Eq)]
struct Authorization<'a> {
    key_id: &'a str,
    /// The day of the credential scope, `YYYYMMDD`.
    date: &'a str,
    region: &'a str,
    /// The names of the signed headers.
    signed: Vec<&'a str>,
    /// The signature, in hex.
    signature: &'a str,
}

impl<'a> Authorization<'a> {
    /// The fields of `AWS4-HMAC-SHA256 Credential=<key>/<date>/<region>/s3/aws4_request,
    /// SignedHeaders=<names>, Signature=<hex>`, if that is what `text` is.
    fn parse(text: &'a str) -> Option<Self> {
        let fields = text.strip_prefix("AWS4-HMAC-SHA256")?;
        let (mut credential, mut signed, mut signature) = (None, None, None);
        for field in fields.split(',') {
            let (name, value) = field.trim().split_once('=')?;
            match name {
                "Credential" => credential = Some(value),
                "SignedHeaders" => signed = Some(value),
                "Signature" => signature = Some(value),
                _ => {}
            }
        }
        let mut scope = credential?.split('/');
        let key_id = scope.next()?;
        let date = scope.next()?;
        let region = scope.next()?;
        let tail = (scope.next()?, scope.next()?, scope.next());
        (tail == ("s3", "aws4_request", None)).then_some(())?;
        Some(Self {
            key_id,
            date,
            region,
            signed: signed?.split(';').collect(),
            signature: signature?,
        })
    }
}

pub(super) fn encode(text: &str, set: &'static AsciiSet) -> String {
    utf8_percent_encode(text, set).to_string()
}

/// The header `name` as the canonical request has it: its values trimmed, with the runs of
/// white space inside them folded to one space, joined by commas.
pub(crate) fn canonical_header(parts: &Parts, name: &str) -> Option<String> {
    let values: Vec<String> = parts
        .headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(|value| value.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    (!values.is_empty()).then(|| format!("{name}:{}\n", values.join(",")))
}

/// The canonical request of SigV4: method, path, query, signed headers and payload hash.
fn canonical_request(parts: &Parts, auth: &Authorization<'_>, payload: &str) -> Option<String> {
    canonical(parts, &auth.signed, payload, false)
}

/// The canonical request of SigV4 for the headers `signed`; for a presigned URL the parameter
/// `X-Amz-Signature` is left out of the query (`drop_signature`).
pub(super) fn canonical(
    parts: &Parts,
    signed: &[&str],
    payload: &str,
    drop_signature: bool,
) -> Option<String> {
    let path = percent_decode_str(parts.uri.path()).decode_utf8().ok()?;
    let mut names = signed.to_vec();
    names.sort_unstable();
    let headers = names.iter().map(|name| canonical_header(parts, name));
    let headers: String = headers.collect::<Option<String>>()?;
    let mut pairs: Vec<(String, String)> = query_pairs(parts.uri.query())?
        .iter()
        .filter(|(name, _)| !(drop_signature && name == "X-Amz-Signature"))
        .map(|(name, value)| (encode(name, UNRESERVED), encode(value, UNRESERVED)))
        .collect();
    pairs.sort();
    let query: Vec<String> = pairs
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    let method = parts.method.as_str();
    let (path, query, signed) = (encode(&path, PATH), query.join("&"), names.join(";"));
    Some(format!(
        "{method}\n{path}\n{query}\n{headers}\n{signed}\n{payload}"
    ))
}

pub(super) fn keyed(key: &[u8]) -> Hmac<Sha256> {
    <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts a key of any length")
}

pub(super) fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = keyed(key);
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// The key a request of `date` in `region` is signed with.
pub(super) fn signing_key(secret: &str, date: &str, region: &str) -> Vec<u8> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac(&k_date, region.as_bytes());
    let k_service = hmac(&k_region, b"s3");
    hmac(&k_service, b"aws4_request")
}

/// The string SigV4 signs.
fn string_to_sign(stamp: &str, auth: &Authorization<'_>, request: &str) -> String {
    string_to_sign_for(stamp, (auth.date, auth.region), request)
}

/// The string SigV4 signs for a request of `stamp`, in the scope of a `(date, region)`.
pub(super) fn string_to_sign_for(stamp: &str, scope: (&str, &str), request: &str) -> String {
    let scope = format!("{}/{}/s3/aws4_request", scope.0, scope.1);
    let digest = hex::encode(Sha256::digest(request.as_bytes()));
    format!("AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{digest}")
}

/// Checks the signature of a request.
///
/// Gives the value of its `x-amz-content-sha256` header, the hash of the body that the
/// signature covers, which the caller checks the body against, and the role of the key it is
/// signed with. The key is the one whose id the credential names, in the server's region. A
/// request with the header of a read grant is not checked as SigV4 at all (see `grant`).
///
/// # Errors
/// The [`Refusal`] that says what is wrong with the request.
pub(crate) fn verify(config: &ServerConfig, parts: &Parts) -> Result<Signed, Refusal> {
    if parts.headers.contains_key(GRANT_HEADER) {
        return grant::verify(config, parts);
    }
    let Some(text) = header(&parts.headers, "authorization") else {
        return presign::verify_query(config, parts);
    };
    let auth = Authorization::parse(text).ok_or(MALFORMED)?;
    if config.presign_key(auth.key_id).is_some() {
        return Err(PRESIGN_ONLY);
    }
    let known = config.key(auth.key_id);
    let (secret, role) = known
        .filter(|_| auth.region == config.region)
        .ok_or(UNKNOWN_KEY)?;
    if !REQUIRED.iter().all(|name| auth.signed.contains(name)) {
        return Err(WEAK);
    }
    let stamp = header(&parts.headers, "x-amz-date").ok_or(MALFORMED)?;
    let now = config.clock.now();
    let early = amz_date(now.saturating_sub(SKEW_MILLIS));
    let late = amz_date(now.saturating_add(SKEW_MILLIS));
    if !(early..=late).contains(&stamp.to_owned()) {
        return Err(SKEWED);
    }
    if stamp.get(..8) != Some(auth.date) {
        return Err(MALFORMED);
    }
    let payload = header(&parts.headers, "x-amz-content-sha256");
    let payload = payload.ok_or(MALFORMED)?;
    let request = canonical_request(parts, &auth, payload).ok_or(MALFORMED)?;
    let key = signing_key(secret, auth.date, auth.region);
    let mut mac = keyed(&key);
    mac.update(string_to_sign(stamp, &auth, &request).as_bytes());
    let signature = hex::decode(auth.signature).ok();
    if signature.is_none_or(|tag| mac.verify_slice(&tag).is_err()) {
        return Err(MISMATCH);
    }
    Ok(Signed {
        payload: payload.to_owned(),
        role,
        presign: None,
    })
}

/// A request, signed as a client would sign it with the full key, for the tests of the server.
#[cfg(test)]
pub(crate) fn sign(
    config: &ServerConfig,
    method: &str,
    uri: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> hyper::Request<http_body_util::Full<bytes::Bytes>> {
    let (id, secret) = (&config.access_key_id, &config.secret_access_key);
    sign_as(config, (id, secret), method, uri, extra, body)
}

/// A request, signed as a client would sign it with the key `(id, secret)`, for the tests of
/// the server.
#[cfg(test)]
pub(crate) fn sign_as(
    config: &ServerConfig,
    key: (&str, &str),
    method: &str,
    uri: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> hyper::Request<http_body_util::Full<bytes::Bytes>> {
    let payload = hex::encode(Sha256::digest(body));
    sign_claiming(config, key, method, uri, extra, &payload, body)
}

/// A request signed with the key `(id, secret)` that claims `payload` as the hash of its body
/// (the value of `x-amz-content-sha256`), whatever `body` is, for the tests that send a body
/// they do not hold: they sign the head, and give the request the body they play.
#[cfg(test)]
pub(crate) fn sign_claiming(
    config: &ServerConfig,
    key: (&str, &str),
    method: &str,
    uri: &str,
    extra: &[(&str, &str)],
    payload: &str,
    body: &[u8],
) -> hyper::Request<http_body_util::Full<bytes::Bytes>> {
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::Request;

    let (id, secret) = key;
    let stamp = amz_date(config.clock.now());
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "tier.test")
        .header("x-amz-content-sha256", payload)
        .header("x-amz-date", &stamp);
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    let (mut parts, ()) = builder.body(()).expect("a request").into_parts();
    let mut names: Vec<String> = parts
        .headers
        .keys()
        .map(|key| key.as_str().to_owned())
        .collect();
    names.sort();
    let auth = Authorization {
        key_id: id,
        date: &stamp[..8],
        region: &config.region,
        signed: names.iter().map(String::as_str).collect(),
        signature: "",
    };
    let request = canonical_request(&parts, &auth, payload).expect("a canonical request");
    let key = signing_key(secret, auth.date, auth.region);
    let to_sign = string_to_sign(&stamp, &auth, &request);
    let signature = hex::encode(hmac(&key, to_sign.as_bytes()));
    let (key_id, date, region) = (auth.key_id, auth.date, auth.region);
    let signed = names.join(";");
    let scope = format!("{key_id}/{date}/{region}/s3/aws4_request");
    let value = format!(
        "AWS4-HMAC-SHA256 Credential={scope}, SignedHeaders={signed}, Signature={signature}"
    );
    parts
        .headers
        .insert("authorization", value.parse().expect("a header value"));
    Request::from_parts(parts, Full::new(Bytes::copy_from_slice(body)))
}
