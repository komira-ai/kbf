//! Who may call the update API (FP section 4, Q7).
//!
//! One function decides: [`Authorize::authorize`]. Today it is [`BearerAuth`]: the
//! operator token in `Authorization: Bearer`, compared in constant time against a token
//! the [`TokenSource`] loads (`operator_token_file` in `installer.toml`, created by
//! `setup`). GitHub OIDC (#1088) will be a second impl of the same trait, for remote
//! callers; nothing else changes.
//!
//! Order inside `BearerAuth`, and why:
//! 1. the rate limit, before anything is read or hashed, so a flood costs a map lookup;
//! 2. a token in the query string is refused (it ends up in logs and proxies);
//! 3. the header is parsed and bounded;
//! 4. both tokens are hashed with SHA-256 and the digests compared in constant time (a
//!    digest has one length, so the compare leaks neither content nor length).
//!
//! A denial is logged with its reason and the peer key, never with a header, a query or
//! any part of a token.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use hyper::header::AUTHORIZATION;
use hyper::{Request, StatusCode};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::traits::Clock;

/// Who the caller is, once authorized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Principal {
    /// Holds the operator token.
    Operator,
}

/// A secret, never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Vec<u8>);

impl Secret {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Secret {
        Secret(bytes.into())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Why a caller was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Denied {
    #[error("rate limited")]
    RateLimited { retry_after_s: u64 },
    #[error("no credentials")]
    MissingCredentials,
    #[error("not a bearer credential")]
    BadScheme,
    #[error("wrong token")]
    BadToken,
    #[error("token in the query string")]
    TokenInQuery,
    #[error("malformed credentials")]
    Malformed,
    /// The server has no usable token: fail closed.
    #[error("no token is configured")]
    TokenUnavailable,
}

impl Denied {
    pub fn status(self) -> StatusCode {
        match self {
            Denied::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            Denied::MissingCredentials | Denied::BadScheme | Denied::BadToken => {
                StatusCode::UNAUTHORIZED
            }
            Denied::TokenInQuery | Denied::Malformed => StatusCode::BAD_REQUEST,
            Denied::TokenUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    /// The code the client sees. The three 401 reasons share one, so the answer is not an
    /// oracle for which part was wrong.
    pub fn code(self) -> &'static str {
        match self {
            Denied::RateLimited { .. } => "rate_limited",
            Denied::MissingCredentials | Denied::BadScheme | Denied::BadToken => "unauthorized",
            Denied::TokenInQuery => "token_in_query",
            Denied::Malformed => "malformed_credentials",
            Denied::TokenUnavailable => "auth_unavailable",
        }
    }
}

/// The one function that decides whether a request may use the update API.
pub trait Authorize: Send + Sync {
    fn authorize(&self, req: &Request<Bytes>) -> Result<Principal, Denied>;
}

/// Where the expected token comes from. Loaded on every call, so rotating the file needs
/// no restart; where the file lives is Q7's `operator_token_file`.
pub trait TokenSource: Send + Sync {
    /// `None` when no token exists (fail closed).
    fn token(&self) -> Option<Secret>;
}

/// The token in a file: hex, one line (`setup` writes 32 random bytes as 64 hex digits).
/// A shorter token is refused, so a hand-written weak one cannot be used by mistake.
pub struct FileTokenSource {
    path: PathBuf,
}

/// The shortest token accepted.
pub const MIN_TOKEN_LEN: usize = 32;

impl FileTokenSource {
    pub fn new(path: impl Into<PathBuf>) -> FileTokenSource {
        FileTokenSource { path: path.into() }
    }
}

impl TokenSource for FileTokenSource {
    fn token(&self) -> Option<Secret> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        let token = text.trim();
        (token.len() >= MIN_TOKEN_LEN).then(|| Secret::new(token))
    }
}

/// The digest used to compare tokens. A trait so a test can see when it runs.
pub trait TokenHasher: Send + Sync {
    fn digest(&self, data: &[u8]) -> [u8; 32];
}

pub struct Sha256Hasher;

impl TokenHasher for Sha256Hasher {
    fn digest(&self, data: &[u8]) -> [u8; 32] {
        Sha256::digest(data).into()
    }
}

/// How fast one client may call: `burst` calls at once, then `per_second` more each
/// second. Every call counts, valid or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimit {
    pub burst: u32,
    pub per_second: u32,
}

impl Default for RateLimit {
    fn default() -> RateLimit {
        RateLimit {
            burst: 20,
            per_second: 5,
        }
    }
}

/// The key of the client a request came from: set by the proxy from the peer address
/// (request extension). A request without one shares the bucket of "unknown".
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PeerKey(pub String);

const UNKNOWN_PEER: &str = "unknown";
/// The buckets kept; a peer beyond them shares the bucket of "unknown".
const MAX_PEERS: usize = 1024;
const MAX_TOKEN_LEN: usize = 512;

/// One bucket, in milli-tokens.
struct Bucket {
    milli: u64,
    at_ms: u64,
}

struct Limiter {
    rate: RateLimit,
    buckets: BTreeMap<String, Bucket>,
}

impl Limiter {
    /// Takes one token for `key`, or says in how many seconds there will be one.
    fn take(&mut self, key: &str, now_ms: u64) -> Result<(), u64> {
        let cap = u64::from(self.rate.burst) * 1000;
        let rate = self.rate;
        let key = if self.buckets.contains_key(key) || self.buckets.len() < MAX_PEERS {
            key
        } else {
            UNKNOWN_PEER
        };
        let b = self.buckets.entry(key.to_owned()).or_insert(Bucket {
            milli: cap,
            at_ms: now_ms,
        });
        let gained = now_ms.saturating_sub(b.at_ms) * u64::from(rate.per_second);
        b.milli = (b.milli + gained).min(cap);
        b.at_ms = now_ms;
        if b.milli >= 1000 {
            b.milli -= 1000;
            return Ok(());
        }
        let per_s = u64::from(rate.per_second.max(1));
        Err((1000 - b.milli).div_ceil(per_s * 1000).max(1))
    }
}

/// The operator token as a bearer credential.
pub struct BearerAuth {
    tokens: Box<dyn TokenSource>,
    hasher: Box<dyn TokenHasher>,
    clock: Arc<dyn Clock>,
    limiter: Mutex<Limiter>,
}

impl BearerAuth {
    pub fn new(
        tokens: Box<dyn TokenSource>,
        hasher: Box<dyn TokenHasher>,
        clock: Arc<dyn Clock>,
        rate: RateLimit,
    ) -> BearerAuth {
        BearerAuth {
            tokens,
            hasher,
            clock,
            limiter: Mutex::new(Limiter {
                rate,
                buckets: BTreeMap::new(),
            }),
        }
    }

    fn check(&self, req: &Request<Bytes>) -> Result<Principal, Denied> {
        let key = req
            .extensions()
            .get::<PeerKey>()
            .map_or(UNKNOWN_PEER, |k| k.0.as_str());
        let taken = self
            .limiter
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take(key, self.clock.mono_ms());
        if let Err(retry_after_s) = taken {
            return Err(Denied::RateLimited { retry_after_s });
        }
        if query_has_token(req.uri().query()) {
            return Err(Denied::TokenInQuery);
        }
        let mut values = req.headers().get_all(AUTHORIZATION).iter();
        let Some(value) = values.next() else {
            return Err(Denied::MissingCredentials);
        };
        if values.next().is_some() {
            return Err(Denied::Malformed);
        }
        let value = value.as_bytes();
        let token = match value.split_at_checked(7) {
            Some((scheme, rest)) if scheme.eq_ignore_ascii_case(b"bearer ") => rest,
            _ => return Err(Denied::BadScheme),
        };
        if token.is_empty() || token.len() > MAX_TOKEN_LEN {
            return Err(Denied::BadToken);
        }
        let Some(expected) = self.tokens.token() else {
            return Err(Denied::TokenUnavailable);
        };
        let presented = self.hasher.digest(token);
        let wanted = self.hasher.digest(&expected.0);
        if bool::from(presented.ct_eq(&wanted)) {
            Ok(Principal::Operator)
        } else {
            Err(Denied::BadToken)
        }
    }
}

impl Authorize for BearerAuth {
    fn authorize(&self, req: &Request<Bytes>) -> Result<Principal, Denied> {
        let outcome = self.check(req);
        if let Err(denied) = &outcome {
            // The path only: not the query, not a header.
            tracing::warn!(reason = %denied, path = req.uri().path(), "update API request denied");
        }
        outcome
    }
}

/// A query that names a credential. Any key that looks like one is refused whatever its
/// value, so the answer does not depend on whether the value was right.
fn query_has_token(query: Option<&str>) -> bool {
    let Some(query) = query else { return false };
    query.split('&').any(|pair| {
        let key = pair.split('=').next().unwrap_or("").to_ascii_lowercase();
        matches!(
            key.as_str(),
            "token" | "access_token" | "bearer" | "operator_token" | "api_key" | "authorization"
        )
    })
}
