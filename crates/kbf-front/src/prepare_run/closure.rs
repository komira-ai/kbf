//! The input closure of an Action: every digest a task must be able to read to run it.
//!
//! [`collect_closure`] reads the Action, then the input root `Directory` and every `Directory`
//! under it, level by level, and asks the store
//! ([`Backend::find_missing`]) whether the Command and every file the tree names are held. The
//! answer is the set of the input root's tree (its Directory blobs and its files) beside the
//! Action and the Command.
//!
//! - **Directories from their rows first.** Each level asks the Directory rows of the pack
//!   index at once ([`Backend::directories`]) for all of its Directories that the walk's
//!   [`DirectoryCache`] does not hold (all of them without a cache; none is asked when the
//!   cache holds the whole level), and reads only the
//!   ones without a row through the frontend's own read path ([`Backend::read_blob`], which
//!   checks the bytes against the digest); those are then recorded as rows
//!   ([`Backend::record_directories`]), so the next walk of the same tree, on any frontend over
//!   the same database, reads none of them from the object store. A row is the Directory's own
//!   bytes (content-addressed), so the walk is the same with rows or without. When the rows
//!   cannot be read the level is read from the blobs, and a record that fails is logged and
//!   the walk goes on: the rows make a walk faster and never decide its answer.
//! - **Complete or an error.** A blob that is not held is [`ClosureError::Missing`], never a
//!   smaller closure: a run whose inputs cannot all be read is not started.
//! - **Bounded.** The tree is cut off as soon as it holds more digests, or more bytes of
//!   blobs, than [`ClosureLimits`] allows, before the Directories under the digest that went
//!   over are read.
//! - **Each digest once.** A Directory that several parents name (a Merkle tree repeats them)
//!   is read when first reached and never again; a file named twice is one digest. A Directory
//!   whose digest a file also has (the empty file and the empty Directory are one blob) is
//!   still read: the closure counts the digest once.
//! - **Deterministic.** The closure is a sorted set, and the error of two failures is the
//!   one of the Directory read first in level order (the reads of a level are issued
//!   together and answered in order), so the same store gives the same answer.
//!
//! [`collect_closure_keeping`] makes the same walk and keeps every `Directory` it read, its
//! digests parsed, for the action tree's manifest writer ([`crate::action_tree`]): the walk the
//! frontend makes before every submit stays the only walk. What it keeps is bounded by the same
//! [`ClosureLimits`], and is dropped with the [`KeptClosure`].
//!
//! The walk may read its `Directory` messages through a [`DirectoryCache`]: a `Directory` the
//! cache holds (in memory, or as a verified blob on its disk) is not read from the store, and one
//! it does not hold is read from the store, decoded and then kept in the cache. The files and the
//! Command are asked of the store (`FindMissing`) either way, so a cache never makes a closure
//! complete that the store cannot serve.

use std::collections::{BTreeMap, BTreeSet};

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
use crate::prepare_run::DirectoryCache;
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

/// An [`InputClosure`] with every `Directory` of its input root kept, by digest, its digests
/// parsed: what the action tree's manifest writer ([`crate::action_tree::TreeWriter`]) builds the
/// manifests from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeptClosure {
    closure: InputClosure,
    root: Digest,
    directories: BTreeMap<Digest, KeptDirectory>,
}

impl KeptClosure {
    /// The closure, as [`collect_closure`] gives it.
    #[must_use]
    pub const fn closure(&self) -> &InputClosure {
        &self.closure
    }

    /// The Action's input root.
    #[must_use]
    pub const fn root(&self) -> &Digest {
        &self.root
    }

    /// How many `Directory` messages are kept: each of the tree's, once.
    #[must_use]
    pub fn directory_count(&self) -> usize {
        self.directories.len()
    }

    /// The kept `Directory` `digest`.
    pub(crate) fn directory(&self, digest: &Digest) -> Option<&KeptDirectory> {
        self.directories.get(digest)
    }
}

/// One kept `Directory`: its nodes in the order the message lists them, their digests parsed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct KeptDirectory {
    /// The `FileNode`s: name, digest and whether it is executable.
    pub(crate) files: Vec<(String, Digest, bool)>,
    /// The `DirectoryNode`s: name and digest.
    pub(crate) directories: Vec<(String, Digest)>,
    /// The `SymlinkNode`s: name and target.
    pub(crate) symlinks: Vec<(String, String)>,
    /// Where REAPI `node_properties` are set (any property, an `mtime` or a `unix_mode`), the
    /// first place found: `Some("")` on the `Directory` itself, `Some(name)` on the node
    /// `name`, `None` nowhere.
    pub(crate) properties: Option<String>,
}

/// A decoded `Directory`: its nodes up to the first whose digest is malformed, and that fault.
#[derive(Debug)]
pub(crate) struct Parsed {
    /// The nodes parsed: all of them when there is no fault.
    pub(crate) kept: KeptDirectory,
    /// The first malformed digest of a node, files before subdirectories.
    pub(crate) fault: Option<ClosureError>,
}

impl KeptDirectory {
    /// The `Directory` held in `blob`, whose digest is `digest`, its nodes' digests parsed.
    ///
    /// A malformed digest of a node is not an error here but the fault the walk names once it
    /// has counted the nodes before it ([`Parsed::fault`]): files are counted before
    /// subdirectories and each list in its order, so a tree that passes the digest limit before
    /// the malformed node is reached is over the limit, as it was before Directories were
    /// cached.
    ///
    /// # Errors
    /// [`ClosureError::Invalid`]: `blob` does not decode.
    pub(crate) fn parse(digest: &Digest, blob: Bytes) -> Result<Parsed, ClosureError> {
        let message =
            reapi::Directory::decode(blob).map_err(|e| invalid("a Directory", digest, &e))?;
        let mut files = Vec::with_capacity(message.files.len());
        let mut directories = Vec::with_capacity(message.directories.len());
        let mut fault = None;
        for node in &message.files {
            match named_in(digest, "a file", node.digest.as_ref()) {
                Ok(file) => files.push((node.name.clone(), file, node.is_executable)),
                Err(error) => {
                    fault = Some(error);
                    break;
                }
            }
        }
        // A malformed file stops the parse: the subdirectories after it are not reached.
        let subdirectories = match fault {
            None => message.directories.as_slice(),
            Some(_) => &[],
        };
        for node in subdirectories {
            match named_in(digest, "a subdirectory", node.digest.as_ref()) {
                Ok(child) => directories.push((node.name.clone(), child)),
                Err(error) => {
                    fault = Some(error);
                    break;
                }
            }
        }
        let kept = Self::of(message, files, directories);
        Ok(Parsed { kept, fault })
    }

    /// The `Directory` `message`, its file and subdirectory nodes already parsed.
    fn of(
        message: reapi::Directory,
        files: Vec<(String, Digest, bool)>,
        directories: Vec<(String, Digest)>,
    ) -> Self {
        let properties = if sets(message.node_properties.as_ref()) {
            Some(String::new())
        } else {
            let files = message.files.iter();
            let files = files.map(|node| (&node.name, node.node_properties.as_ref()));
            let symlinks = message.symlinks.iter();
            let symlinks = symlinks.map(|node| (&node.name, node.node_properties.as_ref()));
            let mut nodes = files.chain(symlinks);
            nodes
                .find(|(_, properties)| sets(*properties))
                .map(|(name, _)| name.clone())
        };
        Self {
            files,
            directories,
            symlinks: message
                .symlinks
                .into_iter()
                .map(|node| (node.name, node.target))
                .collect(),
            properties,
        }
    }
}

/// Whether `properties` sets anything: a named property, an `mtime` or a `unix_mode`. An empty
/// `NodeProperties` message sets nothing.
fn sets(properties: Option<&reapi::NodeProperties>) -> bool {
    properties
        .is_some_and(|p| !p.properties.is_empty() || p.mtime.is_some() || p.unix_mode.is_some())
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
    let (closure, _, _) = collect(backend, action, limits, false, None).await?;
    Ok(closure)
}

/// [`collect_closure`], keeping every `Directory` of the input root it reads.
///
/// # Errors
/// As [`collect_closure`].
pub async fn collect_closure_keeping(
    backend: &Backend,
    action: &Digest,
    limits: ClosureLimits,
) -> Result<KeptClosure, ClosureError> {
    keeping_through(backend, action, limits, None).await
}

/// [`collect_closure_keeping`], reading each `Directory` through `cache` when there is one.
///
/// # Errors
/// As [`collect_closure`].
pub(crate) async fn keeping_through(
    backend: &Backend,
    action: &Digest,
    limits: ClosureLimits,
    cache: Option<&DirectoryCache>,
) -> Result<KeptClosure, ClosureError> {
    let (closure, root, directories) = collect(backend, action, limits, true, cache).await?;
    Ok(KeptClosure {
        closure,
        root,
        directories,
    })
}

/// The closure of `action`, its input root, and, when `keep` is set, every `Directory` of it.
async fn collect(
    backend: &Backend,
    action: &Digest,
    limits: ClosureLimits,
    keep: bool,
    cache: Option<&DirectoryCache>,
) -> Result<(InputClosure, Digest, BTreeMap<Digest, KeptDirectory>), ClosureError> {
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
    // The Directories queued to be read, apart from `seen`: a file may have a Directory's
    // digest (the empty file is the empty Directory, `e3b0c442.../0`), and a Directory a file
    // reached first is still read and kept.
    let mut queued = BTreeSet::from([root]);
    let mut kept = BTreeMap::new();
    let mut files = Vec::new();
    let mut level = vec![root];
    while !level.is_empty() {
        let directories = read_level(backend, cache, level).await?;
        level = Vec::new();
        for (parent, parsed) in directories {
            let Parsed {
                kept: directory,
                fault,
            } = parsed;
            for (_, file, _) in &directory.files {
                if seen.add(*file)? {
                    files.push(*file);
                }
            }
            for (_, child) in &directory.directories {
                seen.add(*child)?;
                if queued.insert(*child) {
                    level.push(*child);
                }
            }
            // The nodes before a malformed digest are counted first, as they always were.
            if let Some(fault) = fault {
                return Err(fault);
            }
            // Kept in the cache in level order, so what it holds does not depend on which read of
            // a level answered first. The empty Directory is not kept.
            if let Some(cache) = cache
                && !is_empty_blob(&parent)
            {
                cache.remember(parent, &directory);
            }
            // The parsed nodes are kept only when asked: the walk before every submit holds
            // none past its level.
            if keep {
                kept.insert(parent, directory);
            }
        }
    }
    require_held(backend, &command, files).await?;
    let closure = InputClosure {
        action: *action,
        command,
        inputs: seen.digests,
        bytes: seen.bytes,
    };
    Ok((closure, root, kept))
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

/// The Directories of one level, in order, parsed: from `cache` when it holds them, else from
/// their rows where the pack index has them, else from their blobs, which are then recorded as
/// rows. Only what the cache does not hold is asked of the rows, and nothing is asked when it
/// holds the whole level.
async fn read_level(
    backend: &Backend,
    cache: Option<&DirectoryCache>,
    level: Vec<Digest>,
) -> Result<Vec<(Digest, Parsed)>, ClosureError> {
    // The empty Directory is never cached: it is left to `directory`, as without a cache.
    // Owned digests: a closure over a borrowed one makes the future's `Send` unprovable where
    // it is boxed (`kbf_server`'s `RunPrepare`, an `async_trait`).
    let held: Vec<Option<KeptDirectory>> = stream::iter(level.clone())
        .map(|digest| async move {
            match is_empty_blob(&digest) {
                true => None,
                false => remembered(cache, &digest).await,
            }
        })
        .buffered(DIRECTORIES_IN_FLIGHT)
        .collect()
        .await;
    let misses: Vec<Digest> = (level.iter().zip(&held))
        .filter(|(_, held)| held.is_none())
        .map(|(digest, _)| *digest)
        .collect();
    let rows = match misses.is_empty() {
        true => Vec::new(),
        false => match backend.directories(&misses).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "the Directory rows could not be read: reading the blobs");
                vec![None; misses.len()]
            }
        },
    };
    let read: Vec<(Digest, Parsed, Option<Bytes>)> = stream::iter(misses.into_iter().zip(rows))
        .map(|(digest, row)| directory(backend, cache, digest, row))
        .buffered(DIRECTORIES_IN_FLIGHT)
        .try_collect()
        .await?;
    let fresh: Vec<(Digest, Bytes)> = read
        .iter()
        .filter_map(|(digest, _, blob)| Some((*digest, blob.clone()?)))
        .collect();
    if !fresh.is_empty()
        && let Err(error) = backend.record_directories(fresh).await
    {
        tracing::warn!(%error, "the Directories read could not be recorded as rows");
    }
    // The digests of a level are distinct: each was queued once.
    let mut read: BTreeMap<Digest, Parsed> = read
        .into_iter()
        .map(|(digest, parsed, _)| (digest, parsed))
        .collect();
    let whole = |kept| Parsed { kept, fault: None };
    Ok((level.into_iter().zip(held))
        .filter_map(|(digest, held)| match held {
            Some(kept) => Some((digest, whole(kept))),
            None => read.remove(&digest).map(|parsed| (digest, parsed)),
        })
        .collect())
}

/// The Directory `digest` parsed, from `row` (its bytes from the Directory rows) when there is
/// one, else from its blob, which is handed back to be recorded; a Directory with no malformed
/// digest is kept on the disk half of `cache`. The empty blob is the empty Directory, with
/// nothing to read, keep or record.
async fn directory(
    backend: &Backend,
    cache: Option<&DirectoryCache>,
    digest: Digest,
    row: Option<Bytes>,
) -> Result<(Digest, Parsed, Option<Bytes>), ClosureError> {
    if is_empty_blob(&digest) {
        let kept = KeptDirectory::default();
        return Ok((digest, Parsed { kept, fault: None }, None));
    }
    let (bytes, fresh) = match row {
        Some(body) => (body, false),
        None => (read(backend, "a Directory", &digest).await?, true),
    };
    let parsed = KeptDirectory::parse(&digest, bytes.clone())?;
    // A Directory with a malformed digest is never stored: the walk fails on it.
    if let Some(cache) = cache
        && parsed.fault.is_none()
    {
        cache.store(&digest, &bytes).await;
    }
    Ok((digest, parsed, fresh.then_some(bytes)))
}

/// The Directory `digest` as `cache` holds it: in memory, or as a blob on its disk that decodes
/// with no malformed digest.
async fn remembered(cache: Option<&DirectoryCache>, digest: &Digest) -> Option<KeptDirectory> {
    let cache = cache?;
    if let Some(kept) = cache.recent(digest) {
        return Some(kept);
    }
    let blob = cache.stored(digest).await?;
    let parsed = KeptDirectory::parse(digest, blob).ok()?;
    parsed.fault.is_none().then_some(parsed.kept)
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
