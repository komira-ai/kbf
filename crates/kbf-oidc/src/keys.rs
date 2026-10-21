//! The issuer's signing keys: a JWKS, kept in memory.
//!
//! A key is looked up by `kid`. An unknown `kid` makes one refetch, at most once per
//! `refetch_interval`, and is then refused. A failed fetch with keys already
//! held keeps verifying with them; a failed first fetch fails closed.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::Refusal;
use super::jwt::jwk_integer;

/// Where the keys come from. The production source is [`super::https::HttpsKeys`].
pub trait KeySource: Send + Sync {
    /// The issuer's JWKS document (JSON text).
    fn jwks(&self) -> BoxFuture<'_, Result<String, String>>;
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    #[serde(rename = "use")]
    usage: Option<String>,
    alg: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

/// An RSA public key: modulus and exponent, big-endian.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RsaKey {
    pub n: Vec<u8>,
    pub e: Vec<u8>,
}

/// The usable keys of a JWKS document, by `kid`. A key that is not an RSA signature key
/// for RS256, or has no `kid`, is skipped; a document with none is an error.
pub(crate) fn parse_jwks(text: &str) -> Result<BTreeMap<String, RsaKey>, String> {
    let doc: Jwks = serde_json::from_str(text).map_err(|e| format!("not a JWKS: {e}"))?;
    let mut out = BTreeMap::new();
    for key in doc.keys {
        let usable = key.kty == "RSA"
            && key.usage.as_deref().is_none_or(|u| u == "sig")
            && key.alg.as_deref().is_none_or(|a| a == "RS256");
        let (Some(kid), true) = (key.kid, usable) else {
            continue;
        };
        let (Some(n), Some(e)) = (
            key.n.as_deref().and_then(jwk_integer),
            key.e.as_deref().and_then(jwk_integer),
        ) else {
            continue;
        };
        out.insert(kid, RsaKey { n, e });
    }
    if out.is_empty() {
        return Err("the JWKS holds no RS256 signing key".to_owned());
    }
    Ok(out)
}

/// How long a fetched key set is trusted without being fetched again: a key the issuer has
/// withdrawn is refused within this long (and within the refetch interval after it), whether or
/// not a token with an unknown `kid` ever comes.
pub const MAX_KEY_AGE: Duration = Duration::from_secs(3600);

struct Held {
    keys: BTreeMap<String, RsaKey>,
    /// Unix seconds of the last fetch that gave keys, if any.
    fetched_at: Option<u64>,
    /// Unix seconds of the last fetch attempt (a failed one counts), if any.
    last_attempt: Option<u64>,
}

/// The keys held and the rule for fetching more.
///
/// A fetch is made when a `kid` is unknown or the key set is older than [`MAX_KEY_AGE`], and
/// never more than once per `refetch_interval`. A failed fetch keeps the keys already held
/// (the issuer being down is not a reason to refuse every deploy; a withdrawn key is then
/// trusted until a fetch succeeds); a failed first fetch fails closed.
///
/// The held keys are behind a lock that is never held across a fetch: a token whose key is held
/// is answered at once while a slow issuer is being waited on. Fetches are serialized, so
/// concurrent unknown-kid tokens make one.
pub struct KeyCache {
    source: Arc<dyn KeySource>,
    held: std::sync::Mutex<Held>,
    fetching: Mutex<()>,
    refetch_interval: Duration,
}

impl KeyCache {
    /// A cache over `source` that refetches at most once per `refetch_interval`.
    #[must_use]
    pub fn new(source: Arc<dyn KeySource>, refetch_interval: Duration) -> Self {
        Self {
            source,
            held: std::sync::Mutex::new(Held {
                keys: BTreeMap::new(),
                fetched_at: None,
                last_attempt: None,
            }),
            fetching: Mutex::new(()),
            refetch_interval,
        }
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The key `kid` names if it is held and the set it is in is not too old.
    fn fresh(&self, kid: &str, now: u64) -> Option<RsaKey> {
        let held = self.held();
        let age = now.saturating_sub(held.fetched_at?);
        (age < MAX_KEY_AGE.as_secs())
            .then(|| held.keys.get(kid).cloned())
            .flatten()
    }

    /// The key `kid` names, fetching if it is unknown or the set is old and the interval allows.
    pub(crate) async fn key(&self, kid: &str, now: u64) -> Result<RsaKey, Refusal> {
        if let Some(key) = self.fresh(kid, now) {
            return Ok(key);
        }
        let held_key = self.held().keys.get(kid).cloned();
        // A key held but old: if a fetch is already under way, use what is held rather than
        // wait for it (an unknown `kid` has nothing to use and waits).
        let _fetching = match (held_key, self.fetching.try_lock()) {
            (_, Ok(guard)) => guard,
            (Some(key), Err(_)) => return Ok(key),
            (None, Err(_)) => self.fetching.lock().await,
        };
        // Another token's fetch may have done it while this one waited.
        if let Some(key) = self.fresh(kid, now) {
            return Ok(key);
        }
        let due = {
            let mut held = self.held();
            let due = held
                .last_attempt
                .is_none_or(|at| now.saturating_sub(at) >= self.refetch_interval.as_secs());
            if due {
                held.last_attempt = Some(now);
            }
            due
        };
        if due {
            match self.source.jwks().await.and_then(|t| parse_jwks(&t)) {
                Ok(keys) => {
                    let mut held = self.held();
                    held.keys = keys;
                    held.fetched_at = Some(now);
                }
                Err(why) => {
                    tracing::warn!(%why, "oidc: fetching the issuer's keys failed");
                }
            }
        }
        let held = self.held();
        if held.keys.is_empty() {
            return Err(Refusal::KeysUnavailable);
        }
        held.keys.get(kid).cloned().ok_or(Refusal::UnknownKey)
    }
}
