//! Enrollment on the frontend (`docs/design/install-and-enrollment.md`): the operator's
//! token and node API ([`api`], `docs/api.md`), over the registry in the state store
//! (`kbf-enroll-registry`).
//!
//! Two things the API needs are not here, and are traits so it can be built and tested
//! without them: the CA pin that goes into a token string ([`PinSource`]; [`ca::CaPins`]
//! implements it from the CA the frontend loaded) and the random bytes of a token
//! ([`Random`]; [`OsRandom`] in production, a fixed sequence in tests).
//!
//! The other half is the enroll listener a node calls (`--enroll-listen`):
//!
//! - [`service`] and [`renew`]: `Enroll` and `Renew` over the registry and the CA;
//! - [`ca`]: the CA at first start (`--ca-dir`, `--ca-signer`);
//! - [`tls`]: the listener's own certificate, renewed at half its life;
//! - [`listener`] and [`limits`]: the socket, and what bounds it (body size, rate per
//!   source address, calls in flight);
//! - [`gate`]: the start gate on deny rows (design section 8.3);
//! - [`flags`] and [`start`]: the flags and the start.

pub mod api;
pub mod ca;
pub mod flags;
pub mod gate;
pub mod limits;
pub mod listener;
mod renew;
pub mod service;
pub mod start;
pub mod tls;

#[cfg(test)]
mod testkit;

use std::io::Read as _;

use kbf_types::CaPin;

/// Where the CA pin of a new token string comes from: the SHA-256 of the root
/// certificate's DER. The API cannot read the CA itself.
pub trait PinSource: Send + Sync {
    /// The pin now.
    ///
    /// # Errors
    /// The CA is not available: a token cannot be made without a pin a node can check.
    fn pin(&self) -> Result<CaPin, PinError>;
}

/// Why no pin is available. The text is shown to the operator, so it names no secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PinError(
    /// Why the CA has no pin now.
    pub String,
);

/// The source of a token's id and secret.
pub trait Random: Send + Sync {
    /// Fills `buf` with bytes that are unpredictable.
    ///
    /// # Errors
    /// The source failed; nothing may be used from `buf`.
    fn fill(&self, buf: &mut [u8]) -> std::io::Result<()>;
}

/// The operating system's random bytes (`/dev/urandom`).
#[derive(Debug, Clone, Copy, Default)]
pub struct OsRandom;

impl Random for OsRandom {
    fn fill(&self, buf: &mut [u8]) -> std::io::Result<()> {
        std::fs::File::open("/dev/urandom")?.read_exact(buf)
    }
}
