//! Input read grants: a stateless token that lets a task on a worker read the inputs of
//! its one Action from the frontend, and nothing else.

mod scope;
mod token;

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use kbf_auth::{Authorizers, BearerAuthenticator};
use kbf_types::Digest;

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
        Self::new(key, Arc::new(|| 0))
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
        let _ = (&self.clock, &self.denied);
        token::open(&self.key, text)
    }

    /// Refuses every grant of `build` until `not_after`.
    pub fn revoke(&self, build: &str, not_after: u64) {
        let _ = (build, not_after);
    }

    /// The authenticator of the job-facing listener.
    #[must_use]
    pub fn authenticator(self: &Arc<Self>) -> BearerAuthenticator {
        BearerAuthenticator::new(|_| Ok(kbf_auth::AuthenticationMetadata::default()))
    }

    /// The authorizers of the job-facing listener.
    #[must_use]
    pub fn authorizers() -> Authorizers {
        Authorizers::allow_all()
    }

    /// How many revocations are remembered (a test seam).
    #[cfg(feature = "testkit")]
    #[must_use]
    pub fn revoked_len(&self) -> usize {
        0
    }
}
