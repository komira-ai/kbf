//! The issuer's signing keys: a JWKS, kept in memory.
//!
//! A key is looked up by `kid`. An unknown `kid` makes one refetch, at most once per
//! [`KeyCache::refetch_interval`], and is then refused. A failed fetch with keys already
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

struct State {
    keys: BTreeMap<String, RsaKey>,
    /// Unix seconds of the last fetch attempt (a failed one counts), if any.
    last_attempt: Option<u64>,
}

/// The keys held and the rule for fetching more.
pub struct KeyCache {
    source: Arc<dyn KeySource>,
    state: Mutex<State>,
    refetch_interval: Duration,
}

impl KeyCache {
    /// A cache over `source` that refetches for an unknown `kid` at most once per
    /// `refetch_interval`.
    #[must_use]
    pub fn new(source: Arc<dyn KeySource>, refetch_interval: Duration) -> Self {
        Self {
            source,
            state: Mutex::new(State {
                keys: BTreeMap::new(),
                last_attempt: None,
            }),
            refetch_interval,
        }
    }

    /// The minimum time between two fetches.
    #[must_use]
    pub const fn refetch_interval(&self) -> Duration {
        self.refetch_interval
    }

    /// The key `kid` names, fetching if it is unknown and the interval allows.
    pub(crate) async fn key(&self, kid: &str, now: u64) -> Result<RsaKey, Refusal> {
        // Held across the fetch, so concurrent unknown-kid tokens make one fetch.
        let mut state = self.state.lock().await;
        if let Some(key) = state.keys.get(kid) {
            return Ok(key.clone());
        }
        let due = state
            .last_attempt
            .is_none_or(|at| now.saturating_sub(at) >= self.refetch_interval.as_secs());
        if due {
            state.last_attempt = Some(now);
            match self.source.jwks().await.and_then(|t| parse_jwks(&t)) {
                Ok(keys) => state.keys = keys,
                Err(why) => {
                    tracing::warn!(%why, "remote operator: fetching the issuer's keys failed");
                    if state.keys.is_empty() {
                        return Err(Refusal::KeysUnavailable);
                    }
                }
            }
        } else if state.keys.is_empty() {
            return Err(Refusal::KeysUnavailable);
        }
        state.keys.get(kid).cloned().ok_or(Refusal::UnknownKey)
    }
}
