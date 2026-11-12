//! The action tree's manifest writer: the tierstore tree a job's root file system is read from
//! (docs/design/actions-as-jobs.md, 4.1 and 4.2).
//!
//! [`TreeWriter::write`] takes an Action's input closure with its `Directory` messages kept
//! ([`crate::prepare_run::collect_closure_keeping`]) and the kbf base tree ([`BaseTree`]), and
//! writes:
//!
//! - **one manifest per `Directory`** of the input root, bottom-up: a `FileNode` is a `file`
//!   child naming its blob, with a `source` (the pack span its bytes are) unless the blob is the
//!   whole object `cas/l/<sha256>` or is empty; a `DirectoryNode` is a `dir` child naming its
//!   manifest by digest and length; a `SymlinkNode` is a `symlink` child. Children are sorted
//!   bytewise by name across REAPI's three node lists;
//! - **`.kbf`**, a manifest of two files, `action` and `command`, the Action's and the
//!   Command's blobs;
//! - **the action tree's root**: every child of the base tree's root manifest as it is, and
//!   beside them `work` (the input root's manifest) and `.kbf`. The base's subtrees are named,
//!   never read or written.
//!
//! Each manifest is stored with a create-only write to `cas/l/<sha256>`; an existing key is
//! success, since the key is content-bound. The root's digest names the tree ([`ActionTree`]).
//!
//! **Every blob is made locatable** ([`place`]): the pack index is asked in chunks, `cas/l/`
//! is checked for the rest, and a blob that is in neither is read from the frontend's CAS and
//! written to `cas/l/<sha256>` through a SHA-256 check before any manifest names it.
//!
//! **No tree** ([`NoTree`]) is the answer, by name, for what tierstore cannot represent
//! (`why` `unrepresentable`: `node_properties`, a name its reader refuses, a tree over its
//! entry or depth limit counted with the base's facts, a manifest over its size limit) and for
//! a store that failed or did not answer in time (`why` `store`). Every manifest is checked by
//! tierstore's own reader before it is stored, so the writer never stores one tierstore would
//! refuse.
//!
//! **The memo** ([`memo`]) maps a `Directory` digest to its manifest, so a subtree two input
//! roots share is built and stored once; a hit skips both, and the files below it are not looked
//! up. It holds at most [`MEMO_ENTRIES`] entries, each good for [`MEMO_TTL`] on the writer's
//! clock and no longer than its subdirectories' entries; [`TreeWriter::forget`] drops an input
//! root's entries.
//!
//! The executor's runs that commit into a put directory call it (`--task-out-mode put-dir`),
//! each run's action tree written over the published base tree before the job is submitted.

mod base;
mod encode;
mod memo;
mod place;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use futures::stream::{self, StreamExt as _, TryStreamExt as _};
use kbf_types::Digest;
use tierstore::{MAX_DEPTH, MAX_ENTRIES, TreeDigest, TreeError};

pub use self::base::{BaseFacts, BaseTree, BaseTreeError, RESERVED_NAMES};
pub use self::place::{HEADS_IN_FLIGHT, LOCATE_CHUNK, LOOKUP_DEADLINE};

use self::encode::{Child, Manifest};
use self::memo::{Measure, Memo, Remembered};
use self::place::Place;
use crate::job_read::BlobSource;
use crate::large_backfill::LargeStore;
use crate::prepare_run::{KeptClosure, KeptDirectory};
use crate::proxy::Clock;

/// The most `Directory` manifests the memo remembers.
pub const MEMO_ENTRIES: usize = 100_000;

/// How long a memo entry is good for, at most.
pub const MEMO_TTL: Duration = Duration::from_secs(600);

/// The names an Action's input root may not hold at its top: `.kbf-out`, where a task that
/// commits into a put directory finds the directory on a host whose job mounts go below the
/// input root (docs/design/jobs-on-jobscheduler-and-tierstore.md, section 6).
pub const RESERVED_INPUT_NAMES: [&str; 1] = [".kbf-out"];

/// The entries an action tree adds to the base's and the input root's: `work`, `.kbf`, and
/// `.kbf`'s `action` and `command`.
const ADDED_ENTRIES: u64 = 4;

/// Why an Action gets no tree. [`NoTree::why`] is the fallback reason it is counted under.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum NoTree {
    /// A `Directory` sets REAPI `node_properties`, on itself or a node it lists, which a
    /// manifest cannot carry.
    #[error("the Directory {directory} sets node_properties (on {node:?}; empty: on itself)")]
    NodeProperties {
        /// The `Directory`.
        directory: Digest,
        /// The node's name; empty when the `Directory` itself sets them.
        node: String,
    },
    /// tierstore's reader refuses the manifest of a `Directory`: a name it does not take
    /// (`.wh.`, `..`, empty, ...), a name twice, or a manifest over its size limit.
    #[error("the manifest of the Directory {directory} is one tierstore refuses: {error}")]
    Refused {
        /// The `Directory`.
        directory: Digest,
        /// What the reader said.
        error: TreeError,
    },
    /// tierstore's reader refuses the action tree's root or its `.kbf` (over its size limit).
    #[error("the action tree's root is one tierstore refuses: {error}")]
    RootRefused {
        /// What the reader said.
        error: TreeError,
    },
    /// The action tree, the base tree's entries counted with the input root's and
    /// the four the action tree adds (`work`, `.kbf` and its two files), has more entries than tierstore takes.
    #[error("the action tree has {entries} entries, over tierstore's {MAX_ENTRIES}")]
    TooManyEntries {
        /// The entries it has (at most `u64::MAX`).
        entries: u64,
    },
    /// The action tree is deeper than tierstore takes.
    #[error("the action tree is {depth} levels deep, over tierstore's {MAX_DEPTH}")]
    TooDeep {
        /// Its depth.
        depth: usize,
    },
    /// The pack index or a presence check of `cas/l/` failed.
    #[error("where the blobs are could not be found out: {why}")]
    Lookup {
        /// What the store said.
        why: String,
    },
    /// The pack index and the presence checks did not answer within [`LOOKUP_DEADLINE`].
    #[error("where the blobs are was not found out within {LOOKUP_DEADLINE:?}")]
    Deadline,
    /// `cas/l/` holds an object of another length than the blob's at the blob's key.
    #[error("cas/l/ holds an object of {length} bytes for {digest}")]
    WrongLength {
        /// The blob.
        digest: Digest,
        /// The object's length.
        length: u64,
    },
    /// A blob to be written to `cas/l/` could not be read from the CAS.
    #[error("{digest} could not be read to be written to cas/l/: {why}")]
    Read {
        /// The blob.
        digest: Digest,
        /// What the store said.
        why: String,
    },
    /// The bytes read for a blob are not its digest's: nothing was written.
    #[error("the bytes read for {digest} are not its")]
    Mismatch {
        /// The blob.
        digest: Digest,
    },
    /// A create-only write to `cas/l/` failed.
    #[error("the write of {key} failed: {why}")]
    Put {
        /// The key.
        key: String,
        /// What the store said.
        why: String,
    },
    /// The input root holds a name of [`RESERVED_INPUT_NAMES`] at its top.
    #[error("the input root holds {name:?}, a name the task keeps for its outputs")]
    ReservedInput {
        /// The name.
        name: String,
    },
    /// The closure did not keep a `Directory` of the tree.
    #[error("the closure kept no Directory {directory}")]
    NotKept {
        /// The `Directory`.
        directory: Digest,
    },
}

impl NoTree {
    /// The fallback reason of the design's 4.6: `unrepresentable` or `store`.
    #[must_use]
    pub const fn why(&self) -> &'static str {
        match self {
            Self::NodeProperties { .. }
            | Self::Refused { .. }
            | Self::RootRefused { .. }
            | Self::TooManyEntries { .. }
            | Self::TooDeep { .. }
            | Self::ReservedInput { .. } => "unrepresentable",
            Self::Lookup { .. }
            | Self::Deadline
            | Self::WrongLength { .. }
            | Self::Read { .. }
            | Self::Mismatch { .. }
            | Self::Put { .. }
            | Self::NotKept { .. } => "store",
        }
    }
}

/// A written action tree: every manifest and blob it names is in the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionTree {
    digest: TreeDigest,
}

impl ActionTree {
    /// The digest of the tree's root manifest, which names the tree.
    #[must_use]
    pub const fn digest(&self) -> &TreeDigest {
        &self.digest
    }
}

/// The manifest writer. Shared by every preparation: the memo is the only state it holds.
pub struct TreeWriter {
    source: Arc<dyn BlobSource>,
    store: Arc<dyn LargeStore>,
    clock: Clock,
    memo: Mutex<Memo>,
}

/// The directories of a tree a write builds, children first, each once, and those the memo
/// had.
struct Plan<'a> {
    order: Vec<(Digest, &'a KeptDirectory)>,
    hits: BTreeMap<Digest, Remembered>,
}

impl TreeWriter {
    /// A writer that finds blobs through `source` (the pack index, `cas/l/` presence and the
    /// CAS: the frontend's [`crate::Backend`]), writes to `cas/l/` through `store` (the
    /// frontend's [`crate::Packs`]), and expires memo entries by `clock`.
    #[must_use]
    pub fn new(source: Arc<dyn BlobSource>, store: Arc<dyn LargeStore>, clock: Clock) -> Self {
        Self::with_memo_entries(source, store, clock, MEMO_ENTRIES)
    }

    /// [`TreeWriter::new`], the memo holding at most `entries` (a test scales it down).
    #[must_use]
    pub fn with_memo_entries(
        source: Arc<dyn BlobSource>,
        store: Arc<dyn LargeStore>,
        clock: Clock,
        entries: usize,
    ) -> Self {
        Self {
            source,
            store,
            clock,
            memo: Mutex::new(Memo::new(entries)),
        }
    }

    /// Writes the action tree of `kept` over `base`.
    ///
    /// # Errors
    /// [`NoTree`], by name: what tierstore cannot represent, or a store that failed. Blobs and
    /// manifests written before a failure stay: each is content-bound, and nothing names them
    /// until a tree is returned.
    pub async fn write(&self, kept: &KeptClosure, base: &BaseTree) -> Result<ActionTree, NoTree> {
        reserved_input(kept)?;
        let now = (self.clock)();
        let plan = self.plan(kept, now)?;
        let measures = measure(&plan, now);
        let input = measures[kept.root()];
        let entries = base
            .facts
            .entries
            .saturating_add(input.entries)
            .saturating_add(ADDED_ENTRIES);
        if entries > MAX_ENTRIES {
            return Err(NoTree::TooManyEntries { entries });
        }
        // `.kbf/action` is two levels down.
        let depth = base.facts.depth.max(input.height + 1).max(2);
        if depth > MAX_DEPTH {
            return Err(NoTree::TooDeep { depth });
        }
        let closure = kept.closure();
        let (action, command) = (*closure.action(), *closure.command());
        let files = plan
            .order
            .iter()
            .flat_map(|(_, directory)| directory.files.iter().map(|(_, digest, _)| *digest));
        let blobs: BTreeSet<Digest> = files
            .chain([action, command])
            .filter(|digest| digest.size_bytes > 0)
            .collect();
        let blobs: Vec<Digest> = blobs.into_iter().collect();
        let (places, unlocated) = place::locate(&*self.source, &blobs).await?;
        let mut manifests: BTreeMap<Digest, Digest> = plan
            .hits
            .iter()
            .map(|(d, hit)| (*d, hit.manifest))
            .collect();
        let mut built = Vec::with_capacity(plan.order.len());
        for (digest, directory) in &plan.order {
            let manifest = directory_manifest(directory, &places, &manifests).map_err(|error| {
                NoTree::Refused {
                    directory: *digest,
                    error,
                }
            })?;
            manifests.insert(*digest, manifest.digest);
            built.push(manifest);
        }
        let kbf = top(vec![
            encode::file("action", &action, false, source(&places, &action)),
            encode::file("command", &command, false, source(&places, &command)),
        ])?;
        let mut children = base.children.clone();
        children.push(encode::dir("work", &manifests[kept.root()]));
        children.push(encode::dir(".kbf", &kbf.digest));
        let root = top(children)?;
        place::write_whole(&*self.source, &*self.store, unlocated).await?;
        let store = &*self.store;
        // Collected, as `place`'s asks are, so the future can be proved `Send`.
        let writes: Vec<_> = built
            .iter()
            .map(|manifest| place::write_manifest(store, &manifest.digest, manifest.bytes.clone()))
            .collect();
        stream::iter(writes)
            .buffered(place::WRITES_IN_FLIGHT)
            .try_collect::<()>()
            .await?;
        self.remember(&plan, &measures, &manifests);
        place::write_manifest(store, &kbf.digest, kbf.bytes).await?;
        place::write_manifest(store, &root.digest, root.bytes).await?;
        Ok(ActionTree { digest: root.tree })
    }

    /// Drops the memo's entries of the input root `root`: its own, and every entry the memo
    /// reaches from it through its subdirectories (expired ones included), so that its next tree
    /// is built from the pack index as it is then. An entry the memo already dropped at its
    /// bound is not walked through; its subdirectories were remembered before it and went first.
    pub fn forget(&self, root: &Digest) {
        self.lock().forget(root);
    }

    fn lock(&self) -> MutexGuard<'_, Memo> {
        self.memo.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The directories of `kept`'s tree to build, children first, each once, and the memo's
    /// hits, whose subtrees are not walked.
    fn plan<'a>(&self, kept: &'a KeptClosure, now: SystemTime) -> Result<Plan<'a>, NoTree> {
        let mut memo = self.lock();
        let mut order = Vec::new();
        let mut hits = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut stack: Vec<(Digest, Option<&KeptDirectory>)> = vec![(*kept.root(), None)];
        while let Some((digest, expanded)) = stack.pop() {
            if let Some(directory) = expanded {
                // Every subdirectory of it was popped, and built or found, before this.
                order.push((digest, directory));
                continue;
            }
            if !seen.insert(digest) {
                continue;
            }
            if let Some(hit) = memo.get(&digest, now) {
                hits.insert(digest, hit);
                continue;
            }
            let directory = kept
                .directory(&digest)
                .ok_or(NoTree::NotKept { directory: digest })?;
            if let Some(node) = &directory.properties {
                return Err(NoTree::NodeProperties {
                    directory: digest,
                    node: node.clone(),
                });
            }
            stack.push((digest, Some(directory)));
            let children = directory.directories.iter();
            stack.extend(children.map(|(_, child)| (*child, None)));
        }
        Ok(Plan { order, hits })
    }

    /// Remembers every directory `plan` built, once its manifest is stored.
    fn remember(
        &self,
        plan: &Plan<'_>,
        measures: &BTreeMap<Digest, Measure>,
        manifests: &BTreeMap<Digest, Digest>,
    ) {
        let mut memo = self.lock();
        for (digest, directory) in &plan.order {
            let children = directory.directories.iter().map(|(_, child)| *child);
            memo.put(
                *digest,
                Remembered {
                    manifest: manifests[digest],
                    measure: measures[digest],
                    children: children.collect(),
                },
            );
        }
    }
}

/// The subtree of every directory of `plan`, the hits' as remembered. A built directory is
/// good until [`MEMO_TTL`] from `now`, and no longer than any of its subdirectories.
fn measure(plan: &Plan<'_>, now: SystemTime) -> BTreeMap<Digest, Measure> {
    let fresh = now.checked_add(MEMO_TTL).unwrap_or(now);
    let mut measures: BTreeMap<Digest, Measure> =
        plan.hits.iter().map(|(d, hit)| (*d, hit.measure)).collect();
    for (digest, directory) in &plan.order {
        let children =
            directory.files.len() + directory.directories.len() + directory.symlinks.len();
        let mut measure = Measure {
            entries: children as u64,
            height: usize::from(children > 0),
            expires: fresh,
        };
        for (_, child) in &directory.directories {
            // Children first: every subdirectory is measured.
            let child = measures[child];
            measure.entries = measure.entries.saturating_add(child.entries);
            measure.height = measure.height.max(child.height + 1);
            measure.expires = measure.expires.min(child.expires);
        }
        measures.insert(*digest, measure);
    }
    measures
}

/// The pack span a blob's bytes are, for a manifest's `source`; none for a whole object or an
/// empty blob.
fn source<'a>(places: &'a BTreeMap<Digest, Place>, digest: &Digest) -> Option<(&'a str, u64)> {
    match places.get(digest) {
        Some(Place::Packed { key, offset }) => Some((key, *offset)),
        _ => None,
    }
}

/// The manifest of one `Directory`, its subdirectories' manifests in `manifests`.
fn directory_manifest(
    directory: &KeptDirectory,
    places: &BTreeMap<Digest, Place>,
    manifests: &BTreeMap<Digest, Digest>,
) -> Result<Manifest, TreeError> {
    let files = directory
        .files
        .iter()
        .map(|(name, digest, exec)| encode::file(name, digest, *exec, source(places, digest)));
    // Children first: every subdirectory's manifest is made.
    let directories = directory
        .directories
        .iter()
        .map(|(name, child)| encode::dir(name, &manifests[child]));
    let symlinks = directory
        .symlinks
        .iter()
        .map(|(name, target)| encode::symlink(name, target));
    let children: Vec<Child> = files.chain(directories).chain(symlinks).collect();
    encode::manifest(children)
}

/// The manifest of `.kbf` or of the action tree's root.
fn top(children: Vec<Child>) -> Result<Manifest, NoTree> {
    encode::manifest(children).map_err(|error| NoTree::RootRefused { error })
}

/// Refuses an input root that holds a name of [`RESERVED_INPUT_NAMES`] at its top. A root the
/// closure did not keep is left to the plan, which refuses it by name.
fn reserved_input(kept: &KeptClosure) -> Result<(), NoTree> {
    let root = kept.directory(kept.root()).into_iter();
    let names = root.flat_map(|root| {
        (root.files.iter().map(|(name, _, _)| name))
            .chain(root.directories.iter().map(|(name, _)| name))
            .chain(root.symlinks.iter().map(|(name, _)| name))
    });
    match names
        .into_iter()
        .find(|name| RESERVED_INPUT_NAMES.contains(&name.as_str()))
    {
        Some(name) => Err(NoTree::ReservedInput { name: name.clone() }),
        None => Ok(()),
    }
}
