//! What the server reads of a request: its path and query, its body, and the hash of the
//! body that the signature covers.

use std::borrow::Cow;

use bytes::Bytes;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper::StatusCode;
use hyper::body::Body;
use hyper::header::HeaderMap;
use percent_encoding::percent_decode_str;
use sha2::{Digest, Sha256};

use super::reply::S3Error;

/// The error of a request body.
pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;

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

/// The body of a request, if it has at most `limit` bytes.
///
/// # Errors
/// `EntityTooLarge` for a longer body; `IncompleteBody` for one that could not be read.
pub(crate) async fn read_body<B>(body: B, limit: usize) -> Result<Bytes, S3Error>
where
    B: Body,
    B::Error: Into<BoxError>,
{
    match Limited::new(body, limit).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(error) if error.is::<LengthLimitError>() => {
            let text = format!("The request body is over the limit of {limit} bytes.");
            Err(S3Error::new(StatusCode::PAYLOAD_TOO_LARGE, "EntityTooLarge", text))
        }
        Err(_) => {
            let text = "The request body could not be read in full.";
            Err(S3Error::new(StatusCode::BAD_REQUEST, "IncompleteBody", text))
        }
    }
}

/// Checks the body against the hash the signature covers (the `x-amz-content-sha256` header).
///
/// # Errors
/// A body with another hash; a claim of a signed stream, which is not supported.
pub(crate) fn check_payload(claim: &str, body: &Bytes) -> Result<(), S3Error> {
    match claim {
        "UNSIGNED-PAYLOAD" => Ok(()),
        _ if claim.eq_ignore_ascii_case(&hex::encode(Sha256::digest(body))) => Ok(()),
        _ if claim.starts_with("STREAMING-") => {
            Err(S3Error::unsupported("signed or chunked payloads are not supported"))
        }
        _ => {
            let text = "The x-amz-content-sha256 header is not the SHA-256 of the body.";
            Err(S3Error::new(StatusCode::BAD_REQUEST, "XAmzContentSHA256Mismatch", text))
        }
    }
}
