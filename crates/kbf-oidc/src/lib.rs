//! Verifies a GitHub Actions OIDC token against an allow-list
//! (`docs/design/updates.md` section 5.1). The caller is the coordinator (`kbf-coordinator`),
//! which verifies the token of a CI job; nothing else uses it.
//!
//! One rule set, in this order (RFC 8725): the token's shape and header ([`jwt`]: `alg`
//! is `RS256` by fiat, no key-locating headers), the key by `kid` and the signature
//! ([`keys`]), `iss`, `aud`, the times (`exp`, `nbf`, `iat` with a 60 s leeway, and a
//! limit on `exp - iat`), `repository_id`, `job_workflow_ref`, `ref` and `environment`,
//! `event_name`, `runner_environment`, and last the `jti`, which is remembered until the
//! token's `exp` so a second use is refused. Nothing in a token is read before its
//! signature holds. `sub` is not matched: it is a string of several parts that GitHub can
//! change the form of; `repository_id` and `job_workflow_ref` say the same exactly.

mod config;
mod https;
mod jwt;
mod keys;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

pub use config::{AllowList, ConfigError, DEFAULT_SERVICES, LEEWAY_SECS, OidcConfig};
pub use https::{HttpsKeys, Roots};
pub use keys::{KeyCache, KeySource};

/// The most `jti`s remembered. A full cache refuses new tokens rather than forget one.
pub const MAX_REMEMBERED_JTIS: usize = 10_000;

/// Why a token is refused. [`Refusal::name`] is what the caller is told; the claim values
/// are never repeated back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// Not a well formed compact JWS.
    #[error("malformed token: {0}")]
    Malformed(&'static str),
    /// The header's `alg` is not `RS256`.
    #[error("the algorithm is not RS256")]
    Algorithm,
    /// The header names a key location or is critical.
    #[error("the header carries a key-locating or critical parameter")]
    HeaderParameter,
    /// No key has this `kid`.
    #[error("no signing key has this kid")]
    UnknownKey,
    /// The issuer's keys could not be fetched and none are held.
    #[error("the issuer's keys are unavailable")]
    KeysUnavailable,
    /// The signature does not hold.
    #[error("the signature does not hold")]
    BadSignature,
    /// A required claim is absent or of the wrong type.
    #[error("claim {0} is absent or has the wrong type")]
    MissingClaim(&'static str),
    /// `iss` is not the configured issuer.
    #[error("iss does not match")]
    Issuer,
    /// `aud` is not the configured audience.
    #[error("aud does not match")]
    Audience,
    /// `exp` is past.
    #[error("the token has expired")]
    Expired,
    /// `nbf` is in the future.
    #[error("the token is not valid yet")]
    NotYetValid,
    /// `iat` is in the future.
    #[error("the token was issued in the future")]
    IssuedInFuture,
    /// `exp - iat` is over the limit, or `exp` is not after `iat`.
    #[error("the token's lifetime is not within the limit")]
    Lifetime,
    /// `repository_id` or `repository` does not match.
    #[error("repository does not match")]
    Repository,
    /// `job_workflow_ref` is not an allowed workflow at an allowed ref.
    #[error("job_workflow_ref does not match")]
    Workflow,
    /// `ref` is not an allowed ref.
    #[error("ref does not match")]
    Ref,
    /// `environment` is not an allowed environment.
    #[error("environment does not match")]
    Environment,
    /// `event_name` is not an allowed event.
    #[error("event_name does not match")]
    Event,
    /// `runner_environment` does not match.
    #[error("runner_environment does not match")]
    Runner,
    /// The `jti` was seen.
    #[error("the token was already used")]
    Replayed,
    /// The replay cache is full of live entries.
    #[error("too many live tokens")]
    ReplayCacheFull,
}

impl Refusal {
    /// A short stable name for the answer and for metrics.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Malformed(_) => "malformed",
            Self::Algorithm => "algorithm",
            Self::HeaderParameter => "header_parameter",
            Self::UnknownKey => "unknown_key",
            Self::KeysUnavailable => "keys_unavailable",
            Self::BadSignature => "signature",
            Self::MissingClaim(_) => "missing_claim",
            Self::Issuer => "issuer",
            Self::Audience => "audience",
            Self::Expired => "expired",
            Self::NotYetValid => "not_yet_valid",
            Self::IssuedInFuture => "issued_in_future",
            Self::Lifetime => "lifetime",
            Self::Repository => "repository",
            Self::Workflow => "workflow",
            Self::Ref => "ref",
            Self::Environment => "environment",
            Self::Event => "event",
            Self::Runner => "runner",
            Self::Replayed => "replayed",
            Self::ReplayCacheFull => "replay_cache_full",
        }
    }
}

/// What a verified token says about its run (for the log; none of it decides anything
/// after [`Verifier::verify`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The token's `jti`.
    pub jti: String,
    /// The workflow run's `job_workflow_ref`.
    pub job_workflow_ref: String,
    /// The `ref`, if the token had one.
    pub git_ref: Option<String>,
    /// The `sha` of the commit, if the token had one.
    pub sha: Option<String>,
    /// Which entry of the allow-list admitted the token (its place in the file, from 0).
    pub entry: usize,
    /// The services that entry lets the token set (`["kbf"]` when the file names none).
    pub services: Vec<String>,
}

/// The current time in seconds since the epoch.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The system clock.
#[must_use]
pub fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    })
}

/// Checks tokens against one allow-list.
pub struct Verifier {
    config: AllowList,
    keys: KeyCache,
    seen: Mutex<BTreeMap<String, u64>>,
    clock: Clock,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verifier")
            .field("issuer", &self.config.issuer())
            .finish_non_exhaustive()
    }
}

fn text<'a>(claims: &'a Map<String, Value>, name: &'static str) -> Result<&'a str, Refusal> {
    claims
        .get(name)
        .and_then(Value::as_str)
        .ok_or(Refusal::MissingClaim(name))
}

fn whole(claims: &Map<String, Value>, name: &'static str) -> Result<i64, Refusal> {
    claims
        .get(name)
        .and_then(Value::as_i64)
        .ok_or(Refusal::MissingClaim(name))
}

fn is_one_of(list: &[String], value: &str) -> bool {
    list.iter().any(|v| v == value)
}

impl Verifier {
    /// A verifier for `config` (an [`AllowList`], or one [`OidcConfig`]) that reads keys from
    /// `source`, refetching for an unknown `kid` at most once per `refetch_interval`, and
    /// reads time from `clock`.
    #[must_use]
    pub fn new(
        config: impl Into<AllowList>,
        source: Arc<dyn KeySource>,
        refetch_interval: Duration,
        clock: Clock,
    ) -> Self {
        Self {
            config: config.into(),
            keys: KeyCache::new(source, refetch_interval),
            seen: Mutex::new(BTreeMap::new()),
            clock,
        }
    }

    /// Checks `token`. On success the `jti` is spent: the same token is refused next time.
    ///
    /// # Errors
    /// The first [`Refusal`] in the rule set's order.
    pub async fn verify(&self, token: &str) -> Result<Verified, Refusal> {
        let parsed = jwt::parse(token)?;
        let now = (self.clock)();
        let key = self.keys.key(&parsed.kid, now).await?;
        if !jwt::signature_holds(&key.n, &key.e, parsed.signing_input, &parsed.signature) {
            return Err(Refusal::BadSignature);
        }
        let claims = &parsed.claims;
        // The first entry that admits the token wins; when none does, the refusal is the
        // first entry's, which for a one-entry list is the entry's own.
        let mut first_refusal = None;
        let mut admitted = None;
        for (entry, c) in self.config.entries().iter().enumerate() {
            match Self::check(c, claims, now) {
                Ok(found) => {
                    admitted = Some((entry, c, found));
                    break;
                }
                Err(refusal) => {
                    first_refusal.get_or_insert(refusal);
                }
            }
        }
        let Some((entry, c, (exp, workflow))) = admitted else {
            return Err(first_refusal.unwrap_or(Refusal::Repository));
        };
        let jti = text(claims, "jti")?;
        if jti.is_empty() || jti.len() > 256 {
            return Err(Refusal::MissingClaim("jti"));
        }
        self.spend(jti, exp, now)?;
        Ok(Verified {
            jti: jti.to_owned(),
            job_workflow_ref: workflow.to_owned(),
            git_ref: text(claims, "ref").ok().map(str::to_owned),
            sha: text(claims, "sha").ok().map(str::to_owned),
            entry,
            services: c.services.clone(),
        })
    }

    /// The claims against one entry; returns `exp` and the workflow.
    fn check<'a>(
        c: &OidcConfig,
        claims: &'a Map<String, Value>,
        now: u64,
    ) -> Result<(i64, &'a str), Refusal> {
        if text(claims, "iss")? != c.issuer {
            return Err(Refusal::Issuer);
        }
        if !audience_is(claims, &c.audience)? {
            return Err(Refusal::Audience);
        }
        let exp = Self::times(c, claims, now)?;
        if text(claims, "repository_id")? != c.repository_id
            || c.repository
                .as_deref()
                .is_some_and(|r| text(claims, "repository").ok() != Some(r))
        {
            return Err(Refusal::Repository);
        }
        let workflow = text(claims, "job_workflow_ref")?;
        if !is_one_of(&c.job_workflow_refs, workflow) {
            return Err(Refusal::Workflow);
        }
        if !c.refs.is_empty() && !is_one_of(&c.refs, text(claims, "ref")?) {
            return Err(Refusal::Ref);
        }
        if !c.environments.is_empty() && !is_one_of(&c.environments, text(claims, "environment")?) {
            return Err(Refusal::Environment);
        }
        if !is_one_of(&c.event_names, text(claims, "event_name")?) {
            return Err(Refusal::Event);
        }
        if text(claims, "runner_environment")? != c.runner_environment {
            return Err(Refusal::Runner);
        }
        Ok((exp, workflow))
    }

    /// `exp`, `nbf` and `iat` against `now`; returns `exp`.
    fn times(c: &OidcConfig, claims: &Map<String, Value>, now: u64) -> Result<i64, Refusal> {
        let leeway = i64::try_from(LEEWAY_SECS).unwrap_or(60);
        let now = i64::try_from(now).unwrap_or(i64::MAX);
        let (exp, iat) = (whole(claims, "exp")?, whole(claims, "iat")?);
        if now.saturating_sub(leeway) >= exp {
            return Err(Refusal::Expired);
        }
        if let Some(nbf) = claims.get("nbf") {
            let nbf = nbf.as_i64().ok_or(Refusal::MissingClaim("nbf"))?;
            if nbf > now.saturating_add(leeway) {
                return Err(Refusal::NotYetValid);
            }
        }
        if iat > now.saturating_add(leeway) {
            return Err(Refusal::IssuedInFuture);
        }
        let limit = i64::try_from(c.max_lifetime_secs).unwrap_or(i64::MAX);
        if exp <= iat || exp.saturating_sub(iat) > limit {
            return Err(Refusal::Lifetime);
        }
        Ok(exp)
    }

    /// Remembers `jti` until `exp` (plus the leeway the token is still accepted for).
    fn spend(&self, jti: &str, exp: i64, now: u64) -> Result<(), Refusal> {
        let until = u64::try_from(exp).unwrap_or(0).saturating_add(LEEWAY_SECS);
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        seen.retain(|_, until| *until > now);
        if seen.contains_key(jti) {
            return Err(Refusal::Replayed);
        }
        if seen.len() >= MAX_REMEMBERED_JTIS {
            return Err(Refusal::ReplayCacheFull);
        }
        seen.insert(jti.to_owned(), until);
        Ok(())
    }
}

/// `aud` is one string equal to `want`, or an array of exactly that one string.
fn audience_is(claims: &Map<String, Value>, want: &str) -> Result<bool, Refusal> {
    match claims.get("aud") {
        Some(Value::String(aud)) => Ok(aud == want),
        Some(Value::Array(list)) => {
            Ok(matches!(list.as_slice(), [Value::String(aud)] if aud == want))
        }
        _ => Err(Refusal::MissingClaim("aud")),
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_edges;
#[cfg(test)]
mod tests_entries;

#[cfg(any(test, feature = "testkit"))]
#[allow(clippy::missing_panics_doc, clippy::missing_errors_doc)]
pub mod testkit;
