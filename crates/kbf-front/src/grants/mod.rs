//! Input read grants: a stateless token that lets a task on a worker read the inputs of
//! its one Action from the frontend, and nothing else.

mod scope;
mod token;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use kbf_auth::{
    AuthenticationMetadata, Authorizer, Authorizers, BearerAuthenticator, BoxFuture, DenyAuthorizer,
};
use kbf_types::Digest;
use serde_json::json;
use tonic::Status;

pub use self::scope::Scope;
pub use self::token::{Claims, ClaimsError, GrantError, GrantKey, KeyError, MIN_SECRET_LEN, Token};

/// Issues and verifies read grants.
pub struct Grants {
    key: GrantKey,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    denied: Mutex<HashMap<String, u64>>,
}

impl fmt::Debug for Grants {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Grants").finish_non_exhaustive()
    }
}

impl Grants {
    /// Grants under `key`, with `clock` giving unix seconds.
    #[must_use]
    pub fn new(key: GrantKey, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self {
            key,
            clock,
            denied: Mutex::new(HashMap::new()),
        }
    }

    /// Grants under `key` that read the system clock.
    #[must_use]
    pub fn system(key: GrantKey) -> Self {
        Self::new(
            key,
            Arc::new(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs())
            }),
        )
    }

    /// The time now, in unix seconds, by the clock grants are checked against.
    #[must_use]
    pub fn now(&self) -> u64 {
        (self.clock)()
    }

    /// The token that lets build `build` read the inputs of `action` before `not_after`.
    ///
    /// # Errors
    /// The build id is not valid.
    pub fn issue(&self, build: &str, action: Digest, not_after: u64) -> Result<Token, ClaimsError> {
        token::mint(
            &self.key,
            &Claims {
                build: build.to_owned(),
                action,
                not_after,
            },
        )
    }

    /// The claims of `token`.
    ///
    /// # Errors
    /// The token is malformed, forged, expired or revoked.
    pub fn verify(&self, text: &str) -> Result<Claims, GrantError> {
        let claims = token::open(&self.key, text)?;
        let now = (self.clock)();
        if now >= claims.not_after {
            return Err(GrantError::Expired);
        }
        if self
            .denied()
            .get(&claims.build)
            .is_some_and(|until| now < *until)
        {
            return Err(GrantError::Revoked);
        }
        Ok(claims)
    }

    /// Refuses every grant of `build` until `not_after`.
    pub fn revoke(&self, build: &str, not_after: u64) {
        let now = (self.clock)();
        let mut denied = self.denied();
        denied.retain(|_, until| *until > now);
        if not_after > now {
            let until = denied.entry(build.to_owned()).or_insert(0);
            *until = (*until).max(not_after);
        }
    }

    /// The authenticator of the job-facing listener.
    #[must_use]
    pub fn authenticator(self: &Arc<Self>) -> BearerAuthenticator {
        let grants = Arc::clone(self);
        BearerAuthenticator::new(move |bearer| {
            let claims = grants
                .verify(bearer)
                .map_err(|e| Status::unauthenticated(e.to_string()))?;
            Ok(AuthenticationMetadata::new(
                Some(json!({ "grant": claims.build })),
                Some(json!({ "grant": claims.to_json() })),
            ))
        })
    }

    /// The authorizers of the job-facing listener.
    #[must_use]
    pub fn authorizers() -> Authorizers {
        let deny: Arc<dyn Authorizer> = Arc::new(DenyAuthorizer);
        Authorizers {
            capabilities: Arc::clone(&deny),
            cas_get: Arc::new(GrantAuthorizer),
            cas_put: Arc::clone(&deny),
            cas_find_missing: Arc::clone(&deny),
            ac_get: Arc::clone(&deny),
            execute: deny,
        }
    }

    fn denied(&self) -> MutexGuard<'_, HashMap<String, u64>> {
        self.denied.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many revocations are remembered (a test seam).
    #[cfg(feature = "testkit")]
    #[must_use]
    pub fn revoked_len(&self) -> usize {
        self.denied().len()
    }
}

/// Allows a call whose metadata carries a read grant, and refuses any other.
#[derive(Debug)]
struct GrantAuthorizer;

impl Authorizer for GrantAuthorizer {
    fn authorize<'a>(
        &'a self,
        metadata: &'a AuthenticationMetadata,
        instance_names: &'a [&'a str],
    ) -> BoxFuture<'a, Vec<Result<(), Status>>> {
        let answer = if Claims::of(metadata).is_some() {
            Ok(())
        } else {
            Err(Status::permission_denied("a read grant is needed"))
        };
        Box::pin(std::future::ready(vec![answer; instance_names.len()]))
    }
}
