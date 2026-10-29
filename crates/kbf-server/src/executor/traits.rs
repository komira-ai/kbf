//! What the executor needs from the rest of the server, as the smallest seams it can say.
//!
//! Each trait has a real implementation elsewhere (the frontend's ingest of an untrusted
//! pack, the tier store's presigned `PUT`, the CAS read for an action and the read grant) and
//! a fake in the tests. None of them is implemented here.
//!
//! The real [`Ingest`] is a thin adapter over the frontend's ingest of an untrusted pack and
//! belongs to the change that wires the executor in, which builds it from what the server
//! owns: the pack index, a CAS backend over the same index, and the key mint with its secret.
//! It maps this module's types to the frontend's one for one: the build id of a farm lease
//! ([`build_id`](crate::executor::build_id)) for the lease, a key the mint issued for
//! [`PackKey`], [`UntrustedLimits`] filled out with the frontend's own bounds for the fields it
//! does not have, and the frontend's reasons for [`RejectReason`].

use std::fmt;
use std::time::Duration;

use kbf_proto::reapi::{ActionResult, Digest};
use kbf_types::LeaseId;

use crate::executor::task::{PackKey, Secret};

/// The bounds the frontend holds a pack an untrusted task wrote to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UntrustedLimits {
    /// The most bytes one pack holds.
    pub max_pack_bytes: u64,
    /// The most bytes one blob in it holds.
    pub max_blob_bytes: u64,
}

/// Why a pack was refused. Every reason is the farm's to run again with a new key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// No object under the key.
    Missing,
    /// The key or its writer is not the one issued.
    WrongKey,
    /// The pack, its blob count, a blob or the manifest is over its cap.
    TooLarge,
    /// The footer or the records are not a pack.
    Malformed,
    /// A record's bytes do not hash to its digest.
    HashMismatch,
    /// The first record is not a manifest, or it names a digest the pack lacks.
    BadManifest,
    /// An output the manifest names is not in the pack.
    Incomplete,
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Missing => "no object under the key",
            Self::WrongKey => "not the issued key",
            Self::TooLarge => "over a size cap",
            Self::Malformed => "not a pack",
            Self::HashMismatch => "a record does not hash to its digest",
            Self::BadManifest => "bad manifest",
            Self::Incomplete => "an output is missing",
        })
    }
}

/// What the frontend made of the pack a task wrote.
#[derive(Clone, Debug)]
pub enum Ingested {
    /// The action ran. `result` is its result as the frontend rebuilt it from its own
    /// hashing, with every output in the CAS; `cached` says whether the frontend also wrote
    /// it to the action cache.
    Ran {
        /// The rebuilt result.
        result: Box<ActionResult>,
        /// Whether it went to the action cache.
        cached: bool,
    },
    /// The pack is not acceptable.
    Reject(RejectReason),
    /// The store could not be read; asking again may work.
    Unavailable(String),
}

/// Why a pack key was not issued.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct MintError(pub String);

/// Issues, reads, verifies and commits the pack a task wrote. Called after a run ended with
/// the task tool's exit 0, and never with a lock of the farm held.
#[tonic::async_trait]
pub trait Ingest: Send + Sync {
    /// The key `lease` may write the pack of `action` under, good for a run of `wall`. Issued
    /// by the same secret that [`ingest`](Self::ingest) verifies it with: a key the ingest did
    /// not issue for this lease and this action is refused there. Called once for a farm
    /// lease, before the url and the grant of the same run are asked for.
    ///
    /// # Errors
    /// [`MintError`].
    async fn mint(
        &self,
        lease: LeaseId,
        action: &Digest,
        wall: Duration,
    ) -> Result<PackKey, MintError>;

    /// What `key` holds, for `action`, run under `lease`. Never trusts the pack.
    async fn ingest(
        &self,
        lease: LeaseId,
        key: &PackKey,
        action: &Digest,
        limits: &UntrustedLimits,
    ) -> Ingested;
}

/// Why a presigned `PUT` was not minted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresignError {
    /// The store will not sign this (an expiry above its ceiling, a key outside its prefix).
    Refused(String),
    /// The store could not be reached.
    Unavailable(String),
}

/// Mints the one-shot, create-only `PUT` url a task writes its pack to. The url is a secret.
#[tonic::async_trait]
pub trait Presign: Send + Sync {
    /// A url for `key` that stays good for `expires`, on the store proxy of `host`, for an
    /// object of at most `max_bytes`.
    ///
    /// # Errors
    /// [`PresignError`].
    async fn put_url(
        &self,
        host: &str,
        key: &PackKey,
        expires: Duration,
        max_bytes: u64,
    ) -> Result<Secret, PresignError>;
}

/// What the executor reads of an action before it runs one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionFacts {
    /// The image to run: `docker://<repo>@sha256:<digest>`, or the same without the scheme.
    pub image: String,
    /// The action's timeout; `None` when it names none, and the executor applies its own.
    pub timeout: Option<Duration>,
    /// Whether the image sets an `ENTRYPOINT`. The lease server appends the task tool's
    /// arguments to it, so the secrets would go to that program and the tool would not run:
    /// such an action is refused.
    pub has_entrypoint: bool,
}

/// What a task reads its inputs with.
#[derive(Clone, Debug)]
pub struct ReadGrant {
    /// The base url of the read endpoint (an IP literal: the job network has no DNS).
    pub url: String,
    /// The bearer token.
    pub token: Secret,
}

/// Why the inputs of an action could not be had.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputsError {
    /// The action cannot run here, whatever is tried again: no image, an image without a
    /// digest. The caller's to fix.
    Invalid(String),
    /// The CAS or the grant service could not be reached.
    Unavailable(String),
}

/// The CAS read of an action and the read grant of its task.
#[tonic::async_trait]
pub trait Inputs: Send + Sync {
    /// The image and timeout of `action`.
    ///
    /// # Errors
    /// [`InputsError`].
    async fn action(&self, action: &Digest) -> Result<ActionFacts, InputsError>;

    /// A grant that reads the closure of `action` for `lease` for `expires`.
    ///
    /// # Errors
    /// [`InputsError`].
    async fn read_grant(
        &self,
        lease: LeaseId,
        action: &Digest,
        expires: Duration,
    ) -> Result<ReadGrant, InputsError>;
}

#[cfg(test)]
#[path = "traits_tests.rs"]
mod tests;
