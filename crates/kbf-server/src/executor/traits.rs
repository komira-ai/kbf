//! What the executor needs from the rest of the server, as the smallest seams it can say.
//!
//! Each trait has a real implementation, [`Front`](crate::executor::Front), which is built from
//! what the frontend owns: the packs, a CAS backend over the same packs, the key mint and the
//! preparation of a run. It maps this module's types to the frontend's one for one: the build id
//! of a run ([`build_id`](crate::executor::build_id)) for the run, a put directory the
//! mint issued for [`PackKey`], [`UntrustedLimits`] filled out with the frontend's own bounds for the
//! fields it does not have, and the frontend's reasons for [`RejectReason`]. The tests have fakes.

use std::fmt;
use std::time::Duration;

use crate::executor::task::RunId;
use kbf_proto::reapi::{ActionResult, Digest};

use crate::executor::config::InputTree;
use crate::executor::mac::System;
use crate::executor::size_class::SizeClass;
use crate::executor::task::PackKey;

/// The bounds the frontend holds a pack an untrusted task wrote to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UntrustedLimits {
    /// The most bytes one pack holds.
    pub max_pack_bytes: u64,
    /// The most bytes one blob in it holds.
    pub max_blob_bytes: u64,
    /// The most records of a one-pass object.
    pub max_object_entries: usize,
    /// The most bytes of a one-pass object's manifest.
    pub max_object_manifest_bytes: u64,
    /// The bytes of a one-pass object's tail each record may account for.
    pub object_bytes_per_entry: u64,
}

/// Why a pack was refused. Every reason ends the run so that the operation runs again with a new key.
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
    /// A key is as old as the longest it can be in use, or the janitor of `cas/b/` has claimed
    /// it for its delete: the pack is not read.
    Expired,
    /// The janitor of `cas/b/` has claimed a key of the run for its delete: the pack is not read.
    Claimed,
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
            Self::Expired => "the key is too old to be ingested",
            Self::Claimed => "the key is claimed for deletion",
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

/// Why a put directory was not issued, and whose to fix: the same run asked again gives the same
/// answer when it is refused, and may give another when the store was unavailable.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MintError {
    /// The job cannot be prepared as asked (an input missing or over budget, a digest that is
    /// not one, a run no key can carry): the caller's to fix, and settled as such.
    #[error("{0}")]
    Refused(String),
    /// The store could not answer: the operation runs again as its next attempt.
    #[error("{0}")]
    Unavailable(String),
}

/// Issues, reads, verifies and commits what a task wrote. Called after a run ended with
/// the task tool's exit 0, and never with a lock held.
#[tonic::async_trait]
pub trait Ingest: Send + Sync {
    /// The put directory `run` may commit the run of `action` into, good for a run of `wall`.
    /// Issued by the same secret that [`ingest`](Self::ingest) verifies it with: a directory the
    /// ingest did not issue for this run and this action is refused there. Called once for a
    /// run. The run's input tree is written here too, as `tree` says, for
    /// [`Inputs::tree`].
    ///
    /// # Errors
    /// [`MintError`].
    async fn mint(
        &self,
        run: RunId,
        action: &Digest,
        wall: Duration,
        tree: InputTree,
    ) -> Result<PackKey, MintError>;

    /// What `key` holds, for `action`, run under `run`. Never trusts the pack.
    async fn ingest(
        &self,
        run: RunId,
        key: &PackKey,
        action: &Digest,
        limits: &UntrustedLimits,
    ) -> Ingested;
}

/// What the executor reads of an action before it runs one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionFacts {
    /// The image to launch the job from, pinned by digest, without the `docker://` scheme: the
    /// kbf image, which keeps the task tool as its `ENTRYPOINT`.
    pub image: String,
    /// The action's timeout; `None` when it names none, and the executor applies its own.
    pub timeout: Option<Duration>,
    /// The system the action's platform asks for: a Linux host, or a Mac
    /// ([`crate::executor::mac`]).
    pub system: System,
    /// The class the job service sizes the action's job by: its target and kind, read from
    /// its `Command` ([`SizeClass::of_action`]).
    pub size_class: SizeClass,
}

/// Why the inputs of an action could not be had.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputsError {
    /// The action cannot run here, whatever is tried again: an image that is not the
    /// served base. The caller's to fix.
    Invalid(String),
    /// The CAS could not be reached, or no tree was written for the run.
    Unavailable(String),
}

/// The CAS read of an action and the action tree of its run.
#[tonic::async_trait]
pub trait Inputs: Send + Sync {
    /// The image and timeout of `action`.
    ///
    /// # Errors
    /// [`InputsError`].
    async fn action(&self, action: &Digest) -> Result<ActionFacts, InputsError>;

    /// The action tree [`Ingest::mint`] wrote for the run of `run` that commits into a put
    /// directory, as it was asked to write it: with [`InputTree::Manifests`], its root
    /// manifest's digest, 64 lowercase hex digits; with [`InputTree::Provider`], the key of its
    /// tree list. Handed out once.
    ///
    /// # Errors
    /// [`InputsError`]; by default [`InputsError::Unavailable`], for inputs that write no tree.
    async fn tree(&self, run: RunId) -> Result<String, InputsError> {
        Err(InputsError::Unavailable(format!(
            "no input tree was written for {run}"
        )))
    }
}

#[cfg(test)]
#[path = "traits_tests.rs"]
mod tests;
