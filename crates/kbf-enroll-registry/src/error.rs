//! The two ways a call here fails: an operator call with [`RegistryError`], an enroll or
//! renew call with the [`RefusalReason`] the frontend sends.

use kbf_state::StoreError;
use kbf_types::RefusalReason;

/// Why an operator call (create or revoke a token, revoke a node, list) failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// The request is not acceptable; the text says what is wrong, never a secret.
    #[error("invalid request: {0}")]
    Invalid(&'static str),
    /// The token or node named does not exist.
    #[error("not found")]
    NotFound,
    /// A stored row does not decode; the text names the family.
    #[error("stored row is unreadable: {0}")]
    Corrupt(&'static str),
    /// Other writers kept winning the compare-and-set.
    #[error("gave up after {} attempts against other writers", crate::ATTEMPTS)]
    Contended,
    /// The state store failed or this handle is fenced.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// A row that does not decode. Carries the family name only.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Corrupt(pub(crate) &'static str);

impl From<Corrupt> for RegistryError {
    fn from(c: Corrupt) -> Self {
        Self::Corrupt(c.0)
    }
}

/// A store call or a decode that failed, before it is told as a [`RegistryError`] or a
/// [`Fail`].
#[derive(Debug, Clone)]
pub(crate) enum Internal {
    Store(StoreError),
    Corrupt(Corrupt),
}

impl From<StoreError> for Internal {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<Corrupt> for Internal {
    fn from(c: Corrupt) -> Self {
        Self::Corrupt(c)
    }
}

impl From<Internal> for RegistryError {
    fn from(e: Internal) -> Self {
        match e {
            Internal::Store(e) => Self::Store(e),
            Internal::Corrupt(c) => Self::Corrupt(c.0),
        }
    }
}

impl From<Internal> for Fail {
    fn from(e: Internal) -> Self {
        match e {
            Internal::Store(e) => Self::from(e),
            Internal::Corrupt(c) => Self::from(c),
        }
    }
}

/// An enroll or renew failure on its way to the caller: always a [`RefusalReason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fail(pub(crate) RefusalReason);

impl From<RefusalReason> for Fail {
    fn from(reason: RefusalReason) -> Self {
        Self(reason)
    }
}

/// A fenced handle is not the signer (default 4: there is no `enroll/signer` row); any
/// other store failure is the store being unavailable.
impl From<StoreError> for Fail {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Fenced => Self(RefusalReason::NotSigner),
            StoreError::Failed(_) => Self(RefusalReason::StoreUnavailable),
        }
    }
}

/// A row nobody can read cannot be decided on: the store, as far as the caller can tell,
/// is unavailable (a retry on another frontend may work).
impl From<Corrupt> for Fail {
    fn from(_: Corrupt) -> Self {
        Self(RefusalReason::StoreUnavailable)
    }
}
