//! The input closure of an Action: every digest a task must be able to read to run it.
//!
//! [`collect_closure`] reads the Action, then the input root `Directory` and every `Directory`
//! under it through the frontend's own read path ([`Backend::read_blob`]), and asks the store
//! ([`Backend::find_missing`]) whether the Command and every file the tree names are held. The
//! answer is the set of the input root's tree (its Directory blobs and its files) beside the
//! Action and the Command.
//!
//! - **Complete or an error.** A blob that is not held is [`ClosureError::Missing`], never a
//!   smaller closure: a run whose inputs cannot all be read is not started.
//! - **Bounded.** The tree is cut off as soon as it holds more digests, or more bytes of
//!   blobs, than [`ClosureLimits`] allows, before the Directories under the digest that went
//!   over are read.
//! - **Each digest once.** A Directory that several parents name (a Merkle tree repeats them)
//!   is read when first reached and never again; a file named twice is one digest.
//! - **Deterministic.** The closure is a sorted set, and the error of two failures is the
//!   one of the Directory read first in level order (the reads of a level are issued
//!   together and answered in order), so the same store gives the same answer.

use std::collections::BTreeSet;

use bytes::Bytes;
use futures::stream;
use futures::{StreamExt as _, TryStreamExt as _};
use kbf_proto::reapi;
use kbf_storage::StorageError;
use kbf_types::Digest;
use prost::Message as _;

use crate::backend::Backend;
use crate::cache::is_empty_blob;
use crate::grants::Scope;
use crate::wire;

/// The most Directory blobs of one level read at once.
const DIRECTORIES_IN_FLIGHT: usize = 16;

/// The most digests one FindMissing call names.
const FILES_PER_ASK: usize = 4096;

/// The most FindMissing calls open at once.
const ASKS_IN_FLIGHT: usize = 4;

/// How large an input closure may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClosureLimits {
    /// The most distinct digests of the input tree (Directory blobs and files).
    pub max_digests: usize,
    /// The most bytes of the distinct blobs of the input tree, by the sizes their digests say.
    pub max_bytes: u64,
}

impl ClosureLimits {
    /// Half a million digests (a toolchain tree is about a hundred thousand files) and 32 GiB.
    pub const DEFAULT: Self = Self {
        max_digests: 500_000,
        max_bytes: 32 << 30,
    };
}

/// Why an input closure could not be collected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ClosureError {
    /// A blob of the closure is not held (the smallest digest of those found missing).
    #[error("{what} {digest} is not in the store")]
    Missing {
        /// Which blob it is: the Action, its Command, a Directory or a file.
        what: &'static str,
        /// Its digest.
        digest: Digest,
    },
    /// The store could not answer.
    #[error("the store could not be read for {digest}: {why}")]
    Unavailable {
        /// The blob asked for.
        digest: Digest,
        /// What the store said.
        why: String,
    },
    /// A blob is held but is not what the Action says it is, or a digest in it is malformed.
    #[error("{what} {digest} is not usable: {why}")]
    Invalid {
        /// Which blob it is.
        what: &'static str,
        /// Its digest.
        digest: Digest,
        /// What is wrong with it.
        why: String,
    },
    /// The tree names more distinct digests than [`ClosureLimits::max_digests`].
    #[error("the input tree has more than {limit} digests")]
    TooManyDigests {
        /// The limit.
        limit: usize,
    },
    /// The tree's blobs add up to more than [`ClosureLimits::max_bytes`].
    #[error("the input tree is more than {limit} bytes")]
    TooManyBytes {
        /// The limit.
        limit: u64,
    },
}

/// What a task reads to run one Action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputClosure {
    action: Digest,
    command: Digest,
    inputs: BTreeSet<Digest>,
    bytes: u64,
}

impl InputClosure {
    /// The Action.
    #[must_use]
    pub const fn action(&self) -> &Digest {
        &self.action
    }

    /// The Action's Command.
    #[must_use]
    pub const fn command(&self) -> &Digest {
        &self.command
    }

    /// The digests of the input root's tree, sorted: the Directory blobs (the root first
    /// among them only by its digest) and the files, each once.
    pub fn inputs(&self) -> impl Iterator<Item = &Digest> {
        self.inputs.iter()
    }

    /// How many digests [`InputClosure::inputs`] has.
    #[must_use]
    pub fn input_count(&self) -> usize {
        self.inputs.len()
    }

    /// The bytes of the blobs of [`InputClosure::inputs`], by their digests' sizes.
    #[must_use]
    pub const fn input_bytes(&self) -> u64 {
        self.bytes
    }

    /// The scope of a read grant for this closure: the Action, the Command and the inputs.
    #[must_use]
    pub fn scope(&self) -> Scope {
        Scope::new(self.action, self.command, self.inputs.iter().copied())
    }
}

/// The distinct digests of a tree so far, and the bytes they name, kept within the limits.
struct Seen {
    limits: ClosureLimits,
    digests: BTreeSet<Digest>,
    bytes: u64,
}

impl Seen {
    /// Adds `digest`; whether it was new.
    fn add(&mut self, digest: Digest) -> Result<bool, ClosureError> {
        if !self.digests.insert(digest) {
            return Ok(false);
        }
        self.bytes = self.bytes.saturating_add(digest.size_bytes);
        if self.digests.len() > self.limits.max_digests {
            return Err(ClosureError::TooManyDigests {
                limit: self.limits.max_digests,
            });
        }
        if self.bytes > self.limits.max_bytes {
            return Err(ClosureError::TooManyBytes {
                limit: self.limits.max_bytes,
            });
        }
        Ok(true)
    }
}

/// The input closure of `action`, read through `backend`.
///
/// # Errors
/// [`ClosureError`]: a blob is not held, the store cannot answer, the Action or a Directory
/// does not decode, or the tree is over `limits`.
pub async fn collect_closure(
    backend: &Backend,
    action: &Digest,
    limits: ClosureLimits,
) -> Result<InputClosure, ClosureError> {
    let blob = read(backend, "the Action", action).await?;
    let decoded = reapi::Action::decode(blob).map_err(|e| invalid("the Action", action, &e))?;
    let named = |what, digest: Option<&reapi::Digest>| {
        wire::digest(digest).map_err(|status| invalid(what, action, &status.message()))
    };
    let command = named(
        "the Action's command digest",
        decoded.command_digest.as_ref(),
    )?;
    let root = named(
        "the Action's input root",
        decoded.input_root_digest.as_ref(),
    )?;
    let mut seen = Seen {
        limits,
        digests: BTreeSet::new(),
        bytes: 0,
    };
    seen.add(root)?;
    let mut files = Vec::new();
    let mut level = vec![root];
    while !level.is_empty() {
        let directories: Vec<(Digest, reapi::Directory)> = stream::iter(level)
            .map(|digest| directory(backend, digest))
            .buffered(DIRECTORIES_IN_FLIGHT)
            .try_collect()
            .await?;
        level = Vec::new();
        for (parent, directory) in &directories {
            for file in &directory.files {
                let file = named_in(parent, "a file", file.digest.as_ref())?;
                if seen.add(file)? {
                    files.push(file);
                }
            }
            for child in &directory.directories {
                let child = named_in(parent, "a subdirectory", child.digest.as_ref())?;
                if seen.add(child)? {
                    level.push(child);
                }
            }
        }
    }
    require_held(backend, &command, files).await?;
    Ok(InputClosure {
        action: *action,
        command,
        inputs: seen.digests,
        bytes: seen.bytes,
    })
}

/// The digest a Directory `parent` names for `what`.
fn named_in(
    parent: &Digest,
    what: &'static str,
    digest: Option<&reapi::Digest>,
) -> Result<Digest, ClosureError> {
    wire::digest(digest).map_err(|status| invalid(what, parent, &status.message()))
}

fn invalid(what: &'static str, digest: &Digest, why: &impl std::fmt::Display) -> ClosureError {
    ClosureError::Invalid {
        what,
        digest: *digest,
        why: why.to_string(),
    }
}

/// The Directory blob `digest` and its decoded content; the empty blob is the empty Directory.
async fn directory(
    backend: &Backend,
    digest: Digest,
) -> Result<(Digest, reapi::Directory), ClosureError> {
    if is_empty_blob(&digest) {
        return Ok((digest, reapi::Directory::default()));
    }
    let blob = read(backend, "a Directory", &digest).await?;
    let decoded =
        reapi::Directory::decode(blob).map_err(|e| invalid("a Directory", &digest, &e))?;
    Ok((digest, decoded))
}

/// The blob `digest`, which the store holds or the closure is not one.
async fn read(
    backend: &Backend,
    what: &'static str,
    digest: &Digest,
) -> Result<Bytes, ClosureError> {
    backend
        .read_blob(digest)
        .await
        .map_err(|error| match error {
            StorageError::NotFound(_) => ClosureError::Missing {
                what,
                digest: *digest,
            },
            other => store_fault(what, digest, &other),
        })
}

/// A store error other than "not held": [`ClosureError::Unavailable`] when the same call may
/// succeed later (the store could not answer, a node served bad bytes and another owner may
/// not, a stale map, an object a write will heal), else [`ClosureError::Invalid`], since an
/// error that repeats (the store's records disagree, a malformed request, an action-cache
/// refusal) must not be retried as if it were transient.
fn store_fault(what: &'static str, digest: &Digest, error: &StorageError) -> ClosureError {
    match error {
        StorageError::Unavailable(_)
        | StorageError::Corrupt { .. }
        | StorageError::StaleMap { .. }
        | StorageError::Unreachable(_) => ClosureError::Unavailable {
            digest: *digest,
            why: error.to_string(),
        },
        _ => ClosureError::Invalid {
            what,
            digest: *digest,
            why: format!("the store refused it: {error}"),
        },
    }
}

/// Checks that the store holds the Command and every file; the empty blob is always held.
async fn require_held(
    backend: &Backend,
    command: &Digest,
    files: Vec<Digest>,
) -> Result<(), ClosureError> {
    let mut wanted: Vec<Digest> = files.into_iter().chain([*command]).collect();
    wanted.retain(|digest| !is_empty_blob(digest));
    wanted.sort();
    wanted.dedup();
    // Owned chunks: a closure over a borrowed chunk makes the future's `Send` unprovable where
    // it is boxed (`kbf_server`'s `RunPrepare`, an `async_trait`).
    let chunks: Vec<Vec<Digest>> = wanted
        .chunks(FILES_PER_ASK)
        .map(<[Digest]>::to_vec)
        .collect();
    let asks = chunks.into_iter().map(|chunk| async move {
        backend
            .find_missing(&chunk)
            .await
            .map_err(|error| store_fault("a blob of the inputs", &chunk[0], &error))
    });
    let answers: Vec<Vec<Digest>> = stream::iter(asks)
        .buffered(ASKS_IN_FLIGHT)
        .try_collect()
        .await?;
    match answers.into_iter().flatten().min() {
        None => Ok(()),
        Some(digest) => Err(ClosureError::Missing {
            what: if digest == *command {
                "the Command"
            } else {
                "a file"
            },
            digest,
        }),
    }
}
