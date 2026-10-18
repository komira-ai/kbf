//! Enrollment on the frontend (`docs/design/install-and-enrollment.md`): the operator's
//! token and node API ([`api`], `docs/api.md`), over the registry in the state store
//! (`kbf-enroll-registry`).
//!
//! Two things the API needs are not here, and are traits so it can be built and tested
//! without them: the CA pin that goes into a token string ([`PinSource`]; the CA's owner
//! implements it) and the random bytes of a token ([`Random`]; [`OsRandom`] in production,
//! a fixed sequence in tests).

pub mod api;

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
