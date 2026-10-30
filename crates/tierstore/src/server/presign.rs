//! Presigned PUT URLs: SigV4 query-string authentication, for one create-only write.
//!
//! A client that holds a URL made by [`presign_put`] (or by any SigV4 presigner, such as the
//! ones of the AWS SDKs) writes one object with a plain `PUT` of the URL: no credential and no
//! library on its side. The server checks the URL exactly as the AWS documentation has it:
//! `X-Amz-Algorithm`, `X-Amz-Credential`, `X-Amz-Date`, `X-Amz-Expires`, `X-Amz-SignedHeaders`
//! and `X-Amz-Signature` in the query; a canonical request of the method, the path, the query
//! sorted and encoded without `X-Amz-Signature`, the signed headers (`host` at least) and the
//! literal payload `UNSIGNED-PAYLOAD`.
//!
//! # Presigning keys
//!
//! Only a presigning key may sign a URL (`--presign-key-file`, one file per key, mode 0600).
//! The full and the read-only keys are refused for a URL, and a presigning key is refused for a
//! request signed in the header: it can never be used for anything but a presigned `PUT`. The
//! file holds the key and its ceiling, all of it required (a key without a ceiling is refused at
//! start):
//!
//! ```json
//! {"access_key_id": "pack-presign", "secret_access_key": "...",
//!  "ceiling": {"prefix": "cas/b/", "max_expires_secs": 600, "max_object_bytes": 134217728}}
//! ```
//!
//! A URL signed by the key is good for:
//!
//! - a `PUT` of one object whose key, after the path is percent-decoded, starts with the
//!   prefix of the ceiling (a byte prefix, no normalization; the prefix ends in `/` and the key is
//!   longer than it) and is a valid name: at most 1024 bytes, no control character (NUL included), no `..` anywhere and no `.` segment; never a
//!   key under `_journal/` or `_tiering/`;
//! - no multipart call, no `If-Match` (nothing is replaced), and no other method;
//! - an `X-Amz-Expires` of at most the smaller of the ceiling's `max_expires_secs` and the
//!   server's `--presign-max-expires`, and the URL is refused from the second
//!   `X-Amz-Date + X-Amz-Expires` on, with no grace (a date more than 15 minutes ahead of the
//!   server's clock is refused too);
//! - a body of at most `max_object_bytes` (at most 5 GiB, the limit of one S3 PUT, and each key file chooses its own), checked on
//!   `Content-Length` and on the bytes received: a request over the limit is answered 413 and
//!   stores nothing. A put is also held to `--max-request-bytes` ([`ServerConfig::max_request_bytes`],
//!   256 MiB by default), so a key whose cap is larger needs that raised as well: the cap in force is
//!   the smaller of the two. A declared length over it is refused before the body is read. The body
//!   is streamed to the tier and never held whole (an S3 tier takes it as a multipart upload of
//!   16 MiB parts, a directory tier writes it to a temporary file).
//!
//! # What the store guarantees
//!
//! The object appears only once its whole body has arrived and passed the checks of the body:
//! the front streams the body to the backend's streamed put (never holding it whole), which
//! makes the object visible only after the last byte, create-only, and (on a storage node) writes
//! to the hot tier only: a full hot tier is `503 SlowDown` with `Retry-After` and nothing is
//! stored, never a write to the cold tier, so the hot tier's create-only write is the one arbiter
//! of a race. It publishes
//! the object atomically and refuses a key that exists (`412 PreconditionFailed`, the
//! stored object untouched), so the first complete `PUT` of a key wins and a connection that is
//! cut leaves nothing. The signature does not cover the body (`UNSIGNED-PAYLOAD`: the URL is made
//! before the body exists), so whoever reads the object must check its content, as the frontend
//! does by hashing every byte after the upload. The query string, which carries the signature, is
//! never logged or put in an error document.

use std::fmt;
use std::path::{Path, PathBuf};

use hmac::Mac;
use hyper::Method;
use hyper::http::request::Parts;
use percent_encoding::utf8_percent_encode;
use serde::Deserialize;

use crate::journal::{JOURNAL_PREFIX, TIERING_PREFIX, is_reserved};

use super::ServerConfig;
use super::auth::{
    MALFORMED, MISMATCH, PATH, Refusal, SKEWED, Signed, UNKNOWN_KEY, UNRESERVED, canonical, encode,
    hmac, keyed, signing_key, string_to_sign_for,
};
use super::keys::{AccessKey, Role, read_private_text};
use super::ops::MULTIPART;
use super::request::{Query, Target, header, query_pairs};
use super::time::{amz_date, parse_amz_date};

/// The payload hash of a presigned request.
const UNSIGNED: &str = "UNSIGNED-PAYLOAD";

/// How far ahead of the server's clock the date of a URL may be.
const SKEW_MILLIS: u64 = 15 * 60 * 1000;

/// The most bytes of a body under any presigning key: the largest segment.
pub const MAX_OBJECT_BYTES: u64 = 5 << 30;

/// The most seconds of `X-Amz-Expires` that AWS accepts.
pub const MAX_EXPIRES_SECS: u64 = 604_800;

/// The flag whose files hold the presigning keys.
const FLAG: &str = "--presign-key-file";

pub(crate) const PRESIGN_ONLY: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: this access key signs presigned PUT URLs only.",
};

pub(crate) const PUT_ONLY: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: a presigned URL of this key is good for one PUT only.",
};

const WEAK: Refusal = Refusal {
    code: "AccessDenied",
    message: "The host header must be signed.",
};

const EXPIRED: Refusal = Refusal {
    code: "AccessDenied",
    message: "Request has expired.",
};

const TOO_LONG: Refusal = Refusal {
    code: "AuthorizationQueryParametersError",
    message: "X-Amz-Expires is zero, or longer than this server accepts.",
};

const OUT_OF_SCOPE: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: the key is outside the prefix this access key may write.",
};

const NO_OVERWRITE: Refusal = Refusal {
    code: "AccessDenied",
    message: "Access Denied: a presigned PUT creates an object and does not replace one.",
};

/// Why a name or a URL cannot be made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PresignError {
    /// The key or the prefix is not a name a presigned URL may cover; the text says why.
    #[error("not a valid name: {0}")]
    Name(&'static str),
    /// The presigning key is not usable; the text says why.
    #[error("invalid presigning key: {0}")]
    Key(&'static str),
    /// The expiry is zero or over 7 days.
    #[error("X-Amz-Expires is from 1 second to 7 days")]
    Expires,
    /// The endpoint is not `http://host[:port]` or `https://host[:port]`.
    #[error("the endpoint is not http(s)://host[:port]")]
    Endpoint,
}

/// Whether `text` is a name that a key or a prefix may be: at most 1024 bytes, no control
/// character (NUL included), no `..` anywhere and no `.` as a whole path segment. (A `..`
/// anywhere is refused and not only as a segment, so that no encoding of one gets by.)
///
/// # Errors
/// [`PresignError::Name`] with the reason.
pub fn check_name(text: &str) -> Result<(), PresignError> {
    if text.len() > 1024 {
        return Err(PresignError::Name("over 1024 bytes"));
    }
    if text.chars().any(char::is_control) {
        return Err(PresignError::Name("a control character"));
    }
    if text.contains("..") {
        return Err(PresignError::Name("contains .."));
    }
    if text.split('/').any(|segment| segment == ".") {
        return Err(PresignError::Name("a . segment"));
    }
    Ok(())
}

/// Whether `prefix` is one a presigning key may have: a name, not empty, and neither under
/// `_journal/` or `_tiering/` nor a prefix of either.
///
/// # Errors
/// [`PresignError::Name`] with the reason.
pub fn validate_prefix(prefix: &str) -> Result<(), PresignError> {
    if prefix.is_empty() {
        return Err(PresignError::Name("empty"));
    }
    if prefix.starts_with('/') {
        return Err(PresignError::Name("starts with /"));
    }
    check_name(prefix)?;
    let reserved = [JOURNAL_PREFIX, TIERING_PREFIX];
    if is_reserved(prefix) || reserved.iter().any(|name| name.starts_with(prefix)) {
        return Err(PresignError::Name("reserved"));
    }
    Ok(())
}

/// What a presigning key is limited to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// The prefix the key of an object must start with.
    pub prefix: String,
    /// The most seconds of `X-Amz-Expires`.
    pub max_expires_secs: u64,
    /// The most bytes of a body.
    pub max_object_bytes: u64,
}

impl Grant {
    /// Whether the grant is one a key may have.
    ///
    /// # Errors
    /// [`PresignError`], saying which part.
    pub fn validate(&self) -> Result<(), PresignError> {
        if validate_prefix(&self.prefix).is_err() {
            return Err(PresignError::Key(
                "the ceiling prefix is not a valid prefix",
            ));
        }
        if !self.prefix.ends_with('/') {
            return Err(PresignError::Key("the ceiling prefix does not end in /"));
        }
        if !(1..=MAX_EXPIRES_SECS).contains(&self.max_expires_secs) {
            return Err(PresignError::Key("max_expires_secs is from 1 to 604800"));
        }
        if !(1..=MAX_OBJECT_BYTES).contains(&self.max_object_bytes) {
            return Err(PresignError::Key(
                "max_object_bytes is from 1 to 5368709120",
            ));
        }
        Ok(())
    }

    /// Whether the call may be done with a URL of this grant: a `PUT` of an object, not a part,
    /// not a replacement, under the prefix.
    ///
    /// # Errors
    /// [`PUT_ONLY`], [`NO_OVERWRITE`] or [`OUT_OF_SCOPE`].
    pub(super) fn permit(
        &self,
        parts: &Parts,
        target: &Target,
        query: &Query,
    ) -> Result<(), Refusal> {
        let Target::Object(key) = target else {
            return Err(PUT_ONLY);
        };
        if parts.method != Method::PUT || query.has_any(&MULTIPART) {
            return Err(PUT_ONLY);
        }
        if header(&parts.headers, "if-match").is_some() {
            return Err(NO_OVERWRITE);
        }
        let inside =
            key.len() > self.prefix.len() && key.starts_with(&self.prefix) && !is_reserved(key);
        if inside && check_name(key).is_ok() {
            Ok(())
        } else {
            Err(OUT_OF_SCOPE)
        }
    }
}

/// A key that signs presigned PUT URLs, and the most such a URL may do.
#[derive(Clone, PartialEq, Eq)]
pub struct PresignKey {
    key: AccessKey,
    grant: Grant,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantFile {
    prefix: String,
    max_expires_secs: u64,
    max_object_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    access_key_id: String,
    secret_access_key: String,
    ceiling: GrantFile,
}

impl PresignKey {
    /// The key with this id and secret, limited to `grant`.
    ///
    /// # Errors
    /// [`PresignError::Key`] for an empty id or secret, an id with a `/`, `,`, `=` or a
    /// character that is not printable ASCII (a credential scope has to carry it), and a grant
    /// that [`Grant::validate`] refuses.
    pub fn new(id: &str, secret: &str, grant: Grant) -> Result<Self, PresignError> {
        let plain = |c: char| c.is_ascii_graphic() && !"/,=".contains(c);
        if id.is_empty() || secret.is_empty() || !id.chars().all(plain) {
            return Err(PresignError::Key(
                "the id or the secret is empty, or the id is odd",
            ));
        }
        grant.validate()?;
        Ok(Self {
            key: AccessKey::new(id, secret),
            grant,
        })
    }

    /// The key in the file `path`: JSON of exactly `access_key_id`, `secret_access_key` and
    /// `ceiling` (see the module docs), mode 0600 or tighter. A file with no ceiling is refused.
    ///
    /// # Errors
    /// What is wrong, led by `flag` and the path. No message carries the secret.
    pub fn from_file(flag: &str, path: &Path) -> Result<Self, String> {
        let fail = |why: &str| format!("{flag} {}: {why}", path.display());
        let text = read_private_text(flag, path)?;
        // The error of the parser is dropped: for a value of the wrong type it quotes the value.
        let parsed = serde_json::from_str::<KeyFile>(&text);
        let file = parsed.map_err(|_| {
            fail("not JSON of exactly access_key_id, secret_access_key and a ceiling")
        })?;
        let grant = Grant {
            prefix: file.ceiling.prefix,
            max_expires_secs: file.ceiling.max_expires_secs,
            max_object_bytes: file.ceiling.max_object_bytes,
        };
        Self::new(&file.access_key_id, &file.secret_access_key, grant)
            .map_err(|why| fail(&why.to_string()))
    }

    /// The id of the key.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.key.id
    }

    /// What the key is limited to.
    #[must_use]
    pub fn grant(&self) -> &Grant {
        &self.grant
    }
}

impl fmt::Debug for PresignKey {
    /// Shows the id and never the secret.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PresignKey")
            .field("id", &self.key.id)
            .field("grant", &self.grant)
            .finish_non_exhaustive()
    }
}

impl ServerConfig {
    /// The presigning key with this id, if the server has one.
    pub(super) fn presign_key(&self, id: &str) -> Option<&PresignKey> {
        self.presign_keys.iter().find(|key| key.id() == id)
    }
}

/// The presigning keys in the files of the flag `--presign-key-file`, in the order of the flags.
///
/// # Errors
/// The first file that cannot be used, as [`PresignKey::from_file`] says.
pub(super) fn presign_keys(paths: &[PathBuf]) -> Result<Vec<PresignKey>, String> {
    paths
        .iter()
        .map(|path| PresignKey::from_file(FLAG, path))
        .collect()
}

/// Refuses a server in which two keys have one id: a request names its key by id alone.
///
/// # Errors
/// What is wrong, naming the id (never a secret).
pub(super) fn ensure_distinct(others: &[&str], presign: &[PresignKey]) -> Result<(), String> {
    let mut seen: std::collections::BTreeSet<&str> = others.iter().copied().collect();
    for key in presign {
        if !seen.insert(key.id()) {
            let id = key.id();
            return Err(format!(
                "the access key id {id:?} is used twice among the key flags"
            ));
        }
    }
    Ok(())
}

/// The query parameter `name`, if the URL has exactly one.
fn one<'a>(pairs: &'a [(String, String)], name: &str) -> Result<&'a str, Refusal> {
    let mut found = pairs.iter().filter(|(key, _)| key == name);
    let first = found.next().ok_or(MALFORMED)?;
    if found.next().is_some() {
        return Err(MALFORMED);
    }
    Ok(first.1.as_str())
}

/// Checks a request with no `Authorization` header as a presigned URL.
///
/// # Errors
/// The [`Refusal`] that says what is wrong with it. A URL whose key is not a presigning key of
/// the server (the full key included) is an unknown key.
pub(super) fn verify_query(config: &ServerConfig, parts: &Parts) -> Result<Signed, Refusal> {
    let pairs = query_pairs(parts.uri.query()).ok_or(MALFORMED)?;
    if !pairs.iter().any(|(name, _)| name == "X-Amz-Algorithm") {
        return Err(super::auth::UNSIGNED);
    }
    if one(&pairs, "X-Amz-Algorithm")? != "AWS4-HMAC-SHA256" {
        return Err(MALFORMED);
    }
    let mut scope = one(&pairs, "X-Amz-Credential")?.split('/');
    let (id, date, region) = (scope.next(), scope.next(), scope.next());
    let tail = (scope.next(), scope.next(), scope.next());
    let (Some(id), Some(date), Some(region)) = (id, date, region) else {
        return Err(MALFORMED);
    };
    if tail != (Some("s3"), Some("aws4_request"), None) {
        return Err(MALFORMED);
    }
    let stamp = one(&pairs, "X-Amz-Date")?;
    let expires = one(&pairs, "X-Amz-Expires")?
        .parse::<u64>()
        .map_err(|_| MALFORMED)?;
    let signed: Vec<&str> = one(&pairs, "X-Amz-SignedHeaders")?.split(';').collect();
    let signature = one(&pairs, "X-Amz-Signature")?;
    let key = config.presign_key(id).filter(|_| region == config.region);
    let key = key.ok_or(UNKNOWN_KEY)?;
    if !signed.contains(&"host") {
        return Err(WEAK);
    }
    let started = parse_amz_date(stamp).ok_or(MALFORMED)?;
    if stamp.get(..8) != Some(date) {
        return Err(MALFORMED);
    }
    let now = config.clock.now();
    if started > now.saturating_add(SKEW_MILLIS) {
        return Err(SKEWED);
    }
    let longest = key
        .grant
        .max_expires_secs
        .min(config.presign_max_expires.as_secs());
    if expires == 0 || expires > longest {
        return Err(TOO_LONG);
    }
    let request = canonical(parts, &signed, UNSIGNED, true).ok_or(MALFORMED)?;
    let signing = signing_key(&key.key.secret, date, region);
    let mut mac = keyed(&signing);
    mac.update(string_to_sign_for(stamp, (date, region), &request).as_bytes());
    let tag = hex::decode(signature).ok();
    if tag.is_none_or(|tag| mac.verify_slice(&tag).is_err()) {
        return Err(MISMATCH);
    }
    if now >= started.saturating_add(expires.saturating_mul(1000)) {
        return Err(EXPIRED);
    }
    Ok(Signed {
        payload: UNSIGNED.to_owned(),
        role: Role::PresignPut,
        presign: Some(key.grant.clone()),
    })
}

/// What a presigned URL is made for.
#[derive(Clone, Copy, Debug)]
pub struct PresignRequest<'a> {
    /// The server, `http://host[:port]` or `https://host[:port]`: its authority is the `host`
    /// header the URL is signed for.
    pub endpoint: &'a str,
    /// The region of the credential scope.
    pub region: &'a str,
    /// The bucket.
    pub bucket: &'a str,
    /// The key of the object (not encoded).
    pub key: &'a str,
    /// The seconds the URL is good for, from `now`: 1 to 604800.
    pub expires_secs: u64,
    /// The time, in milliseconds since the Unix epoch.
    pub now: u64,
}

/// A presigned URL for a `PUT` of one object, signed with `key` by the standard SigV4 algorithm
/// for query-string authentication, so that the same URL is good against tierstore, MinIO or S3.
/// The URL carries the signature in its query and must be handled as a secret until it expires.
///
/// # Errors
/// [`PresignError`] for a key that is not a valid name, an expiry out of range, or an endpoint
/// that is not `http(s)://authority`.
pub fn presign_put(key: &AccessKey, request: &PresignRequest<'_>) -> Result<String, PresignError> {
    check_name(request.key)?;
    if request.key.is_empty() {
        return Err(PresignError::Name("empty"));
    }
    if !(1..=MAX_EXPIRES_SECS).contains(&request.expires_secs) {
        return Err(PresignError::Expires);
    }
    presign_url("PUT", key, request, &[])
}

/// A presigned URL for the method `method` with the query parameters `extra` signed too, whatever
/// the key and the expiry are: the tests make the URLs that a server must refuse with it.
pub(crate) fn presign_url(
    method: &str,
    key: &AccessKey,
    request: &PresignRequest<'_>,
    extra: &[(&str, &str)],
) -> Result<String, PresignError> {
    let (scheme, host) = request
        .endpoint
        .split_once("://")
        .filter(|(scheme, host)| {
            ["http", "https"].contains(scheme) && !host.is_empty() && !host.contains('/')
        })
        .ok_or(PresignError::Endpoint)?;
    let stamp = amz_date(request.now);
    let (date, region) = (&stamp[..8], request.region);
    let credential = format!("{}/{date}/{region}/s3/aws4_request", key.id);
    let path = format!("/{}/{}", request.bucket, request.key);
    let path = utf8_percent_encode(&path, PATH).to_string();
    let mut pairs: Vec<(String, String)> = vec![
        ("X-Amz-Algorithm".to_owned(), "AWS4-HMAC-SHA256".to_owned()),
        ("X-Amz-Credential".to_owned(), credential),
        ("X-Amz-Date".to_owned(), stamp.clone()),
        ("X-Amz-Expires".to_owned(), request.expires_secs.to_string()),
        ("X-Amz-SignedHeaders".to_owned(), "host".to_owned()),
    ];
    pairs.extend(
        extra
            .iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned())),
    );
    let mut pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(name, value)| (encode(name, UNRESERVED), encode(value, UNRESERVED)))
        .collect();
    pairs.sort();
    let query: Vec<String> = pairs.iter().map(|(n, v)| format!("{n}={v}")).collect();
    let query = query.join("&");
    let canonical = format!("{method}\n{path}\n{query}\nhost:{host}\n\nhost\n{UNSIGNED}");
    let signing = signing_key(&key.secret, date, region);
    let to_sign = string_to_sign_for(&stamp, (date, region), &canonical);
    let signature = hex::encode(hmac(&signing, to_sign.as_bytes()));
    Ok(format!(
        "{scheme}://{host}{path}?{query}&X-Amz-Signature={signature}"
    ))
}

#[cfg(test)]
#[path = "presign_tests.rs"]
pub(super) mod tests;
