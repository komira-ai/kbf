//! Ingest of the pack a task wrote (stub).

use kbf_proto::reapi::ActionResult;
use kbf_types::Digest;

use super::{IssuedPackKey, PackKeyMint, UntrustedLimits};
use crate::backend::Backend;
use crate::proxy::packed::Packs;

/// Why a pack was refused (also the metric label).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// No object under the key.
    Missing,
    /// The key or the writer is not the issued one.
    WrongKey,
    /// Pack, blob count, blob or manifest record over its cap.
    TooLarge,
    /// Footer, order, overlap, gap or zero-size record.
    Malformed,
    /// A record's bytes do not hash to its digest.
    HashMismatch,
    /// The record at offset 0 is not a valid manifest.
    BadManifest,
    /// A digest the manifest names is not in the pack.
    Incomplete,
}

impl RejectReason {
    /// Every reason, in the order of the metric labels.
    pub const ALL: [Self; 7] = [
        Self::Missing,
        Self::WrongKey,
        Self::TooLarge,
        Self::Malformed,
        Self::HashMismatch,
        Self::BadManifest,
        Self::Incomplete,
    ];

    /// The metric label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::WrongKey => "wrong_key",
            Self::TooLarge => "too_large",
            Self::Malformed => "malformed",
            Self::HashMismatch => "hash_mismatch",
            Self::BadManifest => "bad_manifest",
            Self::Incomplete => "incomplete",
        }
    }
}

/// The store, the index or the action cache could not answer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct StoreError(pub String);

/// What [`ingest`] made of a pack.
#[derive(Debug, PartialEq)]
pub enum Ingested {
    /// The action ran.
    Ran {
        /// The result as the frontend rebuilt it.
        result: ActionResult,
        /// Whether it was written to the action cache.
        cached: bool,
    },
    /// The pack is not acceptable.
    Reject(RejectReason),
    /// The store could not be read.
    Unavailable(StoreError),
}

/// Reads, verifies and commits the pack a task wrote under `key` for `action`.
pub async fn ingest(
    _packs: &Packs,
    _backend: &Backend,
    _mint: &PackKeyMint,
    _lease: &str,
    _key: &IssuedPackKey,
    _action: &Digest,
    _limits: &UntrustedLimits,
) -> Ingested {
    Ingested::Unavailable(StoreError("not implemented".to_owned()))
}
