//! The action tree's manifest writer: the tierstore tree a job's root file system is read from
//! (docs/design/actions-as-jobs.md, 4.1, 4.2 and 4.7).
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
//!   beside them `kbf-work` ([`INPUT_ROOT`], the input root's manifest) and `.kbf`. The base's subtrees are named,
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
//! root's entries. A directory is remembered as soon as its manifest and every manifest of its
//! subtree are stored, not when the whole tree is: a preparation cut off part way keeps what it
//! stored, and the next tree of the input root writes only the rest.
//!
//! **The writes in flight** ([`flight`]) are shared: a preparation that needs a manifest another
//! preparation is writing waits for that write, so concurrent trees that share a subtree store
//! each of its manifests once.
//!
//! **The place memo** ([`places`]) maps a blob's digest to where it was found (a pack span, or
//! the whole object `cas/l/<sha256>`), so that a tree naming blobs an earlier tree named asks
//! neither the pack index nor `cas/l/` for them. It holds at most [`PLACE_ENTRIES`] entries, the
//! oldest dropped first, each good for [`MEMO_TTL`] on the writer's clock. A blob found by the
//! lookups is remembered when they answer; a blob written to `cas/l/` is remembered as whole only
//! after its write succeeded. [`TreeWriter::forget`] drops the place memo whole.
//!
//! **A lost object** is one the frontend's state names and the store does not have: the store is
//! general-purpose, and its objects can vanish. A job whose tree named a manifest the store lost
//! says so, and [`TreeWriter::lost_manifest`] drops the memo's entries that name it and every
//! entry above them, and the place memo whole; a job whose tree named a span of a pack the store
//! answered absent says so, and [`TreeWriter::lost_pack`] drops the memo's entries whose
//! manifests name a span of that pack, every entry above them, and the places in it. The next tree
//! of that action is written with [`TreeWriter::write_checked`] (or
//! [`TreeWriter::write_listing_checked`]): no memo hit, no remembered place, and every pack it
//! names asked after in the store ([`check`]); a pack the store lacks is an absence, which the
//! packs' loss guard confirms only with a second look some time after the first, and once the
//! pack index forgets it both memos are dropped whole and the tree is refused naming its blobs
//! ([`NoTree::Lost`], `why` `missing`), which are then missing for FindMissingBlobs too.
//!
//! **The tree list** ([`listing`], [`TreeWriter::write_listing`]) is the same action tree as one
//! object, tierstore's tree list in its version 2, the `TSTREE` layout ([`tstree`]): every entry
//! as a fixed record, breadth first from the root, the base tree's included
//! ([`BaseTree::listed`]), a file with bytes naming its pack span or whole object and its SHA-256,
//! and a table of the pack ranges the tree reads; stored create-only at
//! `cas/l/<sha256 of the list>`. tierstore's mount provider makes a job's tree from it
//! (`--executor-input-tree provider`).
//!
//! The executor's runs that commit into a put directory call it (`--task-out-mode put-dir`),
//! each run's action tree written over the published base tree, as manifests or as a tree list.
//! A tree's name is a digest of what it holds, known before anything is written:
//! [`TreeWriter::write_named`] and [`TreeWriter::write_listing_named`] hand it out then, so the
//! executor submits the job while the tree's writes go on.

mod base;
mod check;
mod encode;
mod flight;
mod listing;
mod memo;
mod place;
mod places;
mod retry;
mod tstree;
#[cfg(test)]
mod tstree_tests;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use futures::stream::{self, StreamExt as _};
use kbf_types::Digest;
use tierstore::{MAX_DEPTH, MAX_ENTRIES, TreeDigest, TreeError};

pub use self::base::{BaseFacts, BaseTree, BaseTreeError, INPUT_ROOT, RESERVED_NAMES};
pub use self::listing::Listing;
pub use self::place::{HEADS_IN_FLIGHT, LOCATE_CHUNK, LOOKUP_DEADLINE};
pub use self::places::PLACE_ENTRIES;
pub use self::retry::{DEFAULT_TOTAL, FIRST_WAIT, LONGEST_WAIT, PutRetry, Sleeper};
pub(crate) use self::retry::{is_slow_down, tokio_sleeper};

use self::encode::{Child, Manifest};
use self::flight::Flights;
use self::memo::{Measure, Memo, Remembered};
use self::place::Place;
use self::places::Places;
use crate::job_read::BlobSource;
use crate::large_backfill::LargeStore;
use crate::prepare_run::{KeptClosure, KeptDirectory};
use crate::proxy::Clock;

/// The most `Directory` manifests the memo remembers.
pub const MEMO_ENTRIES: usize = 100_000;

/// The most manifest writes the writer has at the store at once, across every preparation
/// ([`flight`]); [`TreeWriter::with_write_slots`] lowers it in a test.
pub const MANIFEST_WRITES: usize = 32;

/// How long a memo entry is good for, at most.
pub const MEMO_TTL: Duration = Duration::from_secs(600);

/// The names an Action's input root may not hold at its top: `.kbf-out`, where a task that
/// commits into a put directory finds the directory on a host whose job mounts go below the
/// input root (docs/design/jobs-on-jobscheduler-and-tierstore.md, section 6).
pub const RESERVED_INPUT_NAMES: [&str; 1] = [".kbf-out"];

/// The entries an action tree adds to the base's and the input root's: `kbf-work`, `.kbf`, and
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
    /// (`..`, empty, with a `/`, ...), a name twice, or a manifest over its size limit.
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
    /// the four the action tree adds (`kbf-work`, `.kbf` and its two files), has more entries than tierstore takes.
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
    /// tierstore's reader refuses the tree list of the tree (a name it does not take, a
    /// list over its size limit), or a record cannot hold it (a name over 255 bytes):
    /// [`TreeWriter::write_listing`] only.
    #[error("the tree list of the tree is one tierstore refuses: {why}")]
    ListingRefused {
        /// What the reader said.
        why: String,
    },
    /// The tree list would hold more entries than the writer takes
    /// ([`TreeWriter::with_list_entries`], by default [`MAX_ENTRIES`]), or more than fit in the
    /// largest list tierstore reads (256 MiB, 64 bytes an entry): [`TreeWriter::write_listing`]
    /// only.
    #[error("the tree list has {entries} entries or more, over the {limit} it may have")]
    ListTooLong {
        /// The entries the list would have (at most `u64::MAX`).
        entries: u64,
        /// The most it may have.
        limit: u64,
    },
    /// The base tree was published without its entries listed, so no tree list can name
    /// them: [`TreeWriter::write_listing`] only.
    #[error("the kbf base tree is not listed: no tree list can be written")]
    BaseNotListed,
    /// The store does not have the packs of these blobs, which the pack index named: the index
    /// forgot those packs, so the blobs are missing until a client uploads them again.
    /// [`TreeWriter::write_checked`] and [`TreeWriter::write_listing_checked`] only.
    #[error("the store lost the packs of {} input blobs, which are missing now", blobs.len())]
    Lost {
        /// The blobs, in digest order; at least one.
        blobs: Vec<Digest>,
    },
}

impl NoTree {
    /// The fallback reason of the design's 4.6: `unrepresentable` or `store`; or `missing`, for
    /// inputs the store lost ([`NoTree::Lost`]), which the client is to upload again.
    #[must_use]
    pub const fn why(&self) -> &'static str {
        match self {
            Self::NodeProperties { .. }
            | Self::Refused { .. }
            | Self::RootRefused { .. }
            | Self::TooManyEntries { .. }
            | Self::TooDeep { .. }
            | Self::ReservedInput { .. }
            | Self::ListingRefused { .. }
            | Self::ListTooLong { .. } => "unrepresentable",
            Self::Lookup { .. }
            | Self::Deadline
            | Self::WrongLength { .. }
            | Self::Read { .. }
            | Self::Mismatch { .. }
            | Self::Put { .. }
            | Self::NotKept { .. }
            | Self::BaseNotListed => "store",
            Self::Lost { .. } => "missing",
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

/// The manifest writer. Shared by every preparation: the manifest memo, the place memo and the
/// manifest writes in flight are the only state it holds.
pub struct TreeWriter {
    source: Arc<dyn BlobSource>,
    store: Arc<dyn LargeStore>,
    clock: Clock,
    retry: PutRetry,
    memo: Mutex<Memo>,
    /// Where blobs were found ([`places`]).
    places: Mutex<Places>,
    /// The manifest writes in flight, which every preparation that needs one waits for
    /// ([`flight`]).
    flights: Flights,
    /// The most entries a tree list holds ([`MAX_ENTRIES`] unless a test lowers it).
    list_entries: u64,
}

/// What a write takes on trust: the memos, or nothing but the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trust {
    /// The memo's manifests and the place memo's places, while they are good.
    Memos,
    /// Neither: every manifest is built, every blob looked up, and every pack asked after in
    /// the store ([`check`]).
    Store,
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
            retry: PutRetry::default(),
            memo: Mutex::new(Memo::new(entries)),
            places: Mutex::new(Places::new(PLACE_ENTRIES)),
            flights: Flights::new(MANIFEST_WRITES),
            list_entries: MAX_ENTRIES,
        }
    }

    /// This writer, its place memo holding at most `entries` (a test scales it down);
    /// [`PLACE_ENTRIES`] by default.
    #[must_use]
    pub fn with_place_entries(mut self, entries: usize) -> Self {
        self.places = Mutex::new(Places::new(entries));
        self
    }

    /// This writer, refusing a tree list of more than `entries` entries
    /// ([`NoTree::ListTooLong`]); [`MAX_ENTRIES`] by default (a test scales it down).
    #[must_use]
    pub fn with_list_entries(mut self, entries: u64) -> Self {
        self.list_entries = entries;
        self
    }

    /// This writer, with at most `slots` manifest writes at the store at once
    /// ([`MANIFEST_WRITES`] by default; a test scales it down).
    #[must_use]
    pub fn with_write_slots(mut self, slots: usize) -> Self {
        self.flights = Flights::new(slots);
        self
    }

    /// This writer, repeating a write the store answered 503 or a dead connection by `retry`.
    #[must_use]
    pub fn with_put_retry(mut self, retry: PutRetry) -> Self {
        self.retry = retry;
        self
    }

    /// Writes the action tree of `kept` over `base`.
    ///
    /// # Errors
    /// [`NoTree`], by name: what tierstore cannot represent, or a store that failed. Blobs and
    /// manifests written before a failure stay: each is content-bound, and nothing names them
    /// until a tree is returned.
    pub async fn write(&self, kept: &KeptClosure, base: &BaseTree) -> Result<ActionTree, NoTree> {
        self.write_as(kept, base, Trust::Memos, |_| ()).await
    }

    /// As [`TreeWriter::write`], and calls `named` with the tree's name as soon as it is known:
    /// after the blobs are located and every manifest is built and checked, before the first
    /// blob or manifest is written. A tree that is refused, or whose lookups fail, is never
    /// named. A name handed to `named` is not yet a stored tree: the writes that follow it may
    /// still fail, and only the tree this returns is in the store. The writes keep their order,
    /// blobs first and the root last, so a reader that finds the root finds every manifest and
    /// blob below it.
    ///
    /// # Errors
    /// As [`TreeWriter::write`], except that a write that fails after `named` ran leaves the
    /// name handed out for a tree that is not in the store; the blobs and manifests written
    /// before it stay.
    pub async fn write_named(
        &self,
        kept: &KeptClosure,
        base: &BaseTree,
        named: impl FnOnce(&ActionTree) + Send,
    ) -> Result<ActionTree, NoTree> {
        self.write_as(kept, base, Trust::Memos, named).await
    }

    /// [`TreeWriter::write`] for a tree whose last job found a manifest lost: nothing is taken
    /// from the memos, every manifest is built and stored again, and every pack the tree names
    /// is asked after in the store ([`check`]).
    ///
    /// # Errors
    /// As [`TreeWriter::write`]; and [`NoTree::Lost`] when the store lacks a pack the index
    /// names, which the index then forgets.
    pub async fn write_checked(
        &self,
        kept: &KeptClosure,
        base: &BaseTree,
    ) -> Result<ActionTree, NoTree> {
        self.write_as(kept, base, Trust::Store, |_| ()).await
    }

    /// [`TreeWriter::write_checked`], naming the tree as [`TreeWriter::write_named`] does: a
    /// pack the store lost is found before the tree is named, so a tree that is
    /// [`NoTree::Lost`] is never named.
    ///
    /// # Errors
    /// As [`TreeWriter::write_checked`] and [`TreeWriter::write_named`].
    pub async fn write_checked_named(
        &self,
        kept: &KeptClosure,
        base: &BaseTree,
        named: impl FnOnce(&ActionTree) + Send,
    ) -> Result<ActionTree, NoTree> {
        self.write_as(kept, base, Trust::Store, named).await
    }

    /// Drops what the memos say of the pack `key`, which the store answered a job absent for:
    /// every memo entry whose manifest names a span of it and every entry above one, and every
    /// place in it the place memo holds; the next tree that holds those blobs looks them up in
    /// the pack index again. Returns whether either memo named the pack.
    pub fn lost_pack(&self, key: &str) -> bool {
        let entries = self.lock().forget_pack(key);
        let places = self.remembered().forget_pack(key);
        entries + places > 0
    }

    /// Drops what the memos say of a manifest the store lost (`manifest`, its SHA-256): every
    /// memo entry that names it and every entry above one, and the place memo whole, since
    /// the store that lost one object may have lost the packs the memo places blobs in. Returns
    /// whether the memo named the manifest.
    pub fn lost_manifest(&self, manifest: &[u8; 32]) -> bool {
        let dropped = self.lock().forget_manifest(manifest);
        self.remembered().clear();
        dropped > 0
    }

    async fn write_as(
        &self,
        kept: &KeptClosure,
        base: &BaseTree,
        trust: Trust,
        named: impl FnOnce(&ActionTree) + Send,
    ) -> Result<ActionTree, NoTree> {
        reserved_input(kept)?;
        let now = (self.clock)();
        let plan = self.plan(kept, now, trust)?;
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
        let (places, unlocated) = self.locate(&blobs, now, trust).await?;
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
        children.push(encode::dir(INPUT_ROOT, &manifests[kept.root()]));
        children.push(encode::dir(".kbf", &kbf.digest));
        let root = top(children)?;
        let tree = ActionTree { digest: root.tree };
        named(&tree);
        self.write_whole(unlocated, now).await?;
        self.store_built(&plan, &built, &measures, &manifests, &places)
            .await?;
        let flights = &self.flights;
        flights
            .write(&self.store, &self.retry, kbf.digest, kbf.bytes)
            .await?;
        flights
            .write(&self.store, &self.retry, root.digest, root.bytes)
            .await?;
        Ok(tree)
    }

    /// Publishes the tierstore manifests of an output directory a task wrote, from the facts of
    /// its one-pass object: `directories` are the tree's `Directory` messages as the frontend
    /// derived them (`root` the output directory's), and `spans` says where each record's bytes start in the
    /// object `key` (every non-empty file of the directories is one). Each manifest is built as [`TreeWriter::write`] builds it,
    /// with `source` the span of that object, stored with a create-only write and remembered, so
    /// that a later input root holding an equal subtree finds it in the memo and writes only what
    /// is new. Returns the manifest of `root`.
    ///
    /// # Errors
    /// [`NoTree`]: what tierstore refuses or cannot represent, or a store that failed.
    pub(crate) async fn publish_output(
        &self,
        root: &Digest,
        directories: &BTreeMap<Digest, KeptDirectory>,
        key: &str,
        spans: &BTreeMap<Digest, u64>,
    ) -> Result<Digest, NoTree> {
        let now = (self.clock)();
        let lookup = |digest: &Digest| directories.get(digest);
        let plan = self.plan_of(root, &lookup, now, Trust::Memos)?;
        let measures = measure(&plan, now);
        // The limits of a whole action tree are applied when one is written ([`TreeWriter::write`]),
        // which counts this directory again with the base's.
        // Every file of the directories is a record of the object: the ingest checks the index's
        // digests against the object's before it publishes.
        let places: BTreeMap<Digest, Place> = (spans.iter())
            .map(|(digest, offset)| {
                let place = Place::Packed {
                    key: key.to_owned(),
                    offset: *offset,
                };
                (*digest, place)
            })
            .collect();
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
        self.store_built(&plan, &built, &measures, &manifests, &places)
            .await?;
        Ok(manifests[root])
    }

    /// Drops the memo's entries of the input root `root`: its own, and every entry the memo
    /// reaches from it through its subdirectories (expired ones included), so that its next tree
    /// is built from the pack index as it is then. An entry the memo already dropped at its
    /// bound is not walked through; its subdirectories were remembered before it and went first.
    ///
    /// The place memo does not say which tree a blob was found for, so it is dropped whole: the
    /// next tree asks the pack index and `cas/l/` for every blob again.
    pub fn forget(&self, root: &Digest) {
        self.lock().forget(root);
        self.remembered().clear();
    }

    /// The places of `blobs` (none of them empty), those remembered at `now` answered from the
    /// place memo and the rest from [`place::locate`], and those of them that are nowhere yet,
    /// which are placed [`Place::Whole`] and must be written before a manifest names them. What
    /// the lookups found is remembered, except the blobs that are nowhere: they are remembered
    /// once [`TreeWriter::write_whole`] has written them.
    ///
    /// With [`Trust::Store`] the place memo answers nothing, and every pack the lookups name is
    /// asked after in the store before anything is remembered ([`check`]).
    async fn locate(
        &self,
        blobs: &[Digest],
        now: SystemTime,
        trust: Trust,
    ) -> Result<(BTreeMap<Digest, Place>, Vec<Digest>), NoTree> {
        let (mut places, unknown) = match trust {
            Trust::Memos => self.remembered().split(blobs, now),
            Trust::Store => (BTreeMap::new(), blobs.to_vec()),
        };
        let (found, unlocated) = place::locate(&*self.source, &unknown).await?;
        if trust == Trust::Store {
            self.check_objects(&found).await?;
        }
        let nowhere: BTreeSet<&Digest> = unlocated.iter().collect();
        let located = found
            .iter()
            .filter(|(digest, _)| !nowhere.contains(digest))
            .map(|(digest, place)| (*digest, place.clone()));
        self.remembered().remember(located, now);
        places.extend(found);
        Ok((places, unlocated))
    }

    /// Writes the `unlocated` blobs to `cas/l/` ([`place::write_whole`]), and remembers them
    /// whole once every write has succeeded: a write that failed is asked about again.
    async fn write_whole(&self, unlocated: Vec<Digest>, now: SystemTime) -> Result<(), NoTree> {
        let written = unlocated.clone();
        place::write_whole(&*self.source, &*self.store, &self.retry, unlocated).await?;
        let whole = written.into_iter().map(|digest| (digest, Place::Whole));
        self.remembered().remember(whole, now);
        Ok(())
    }

    fn remembered(&self) -> MutexGuard<'_, Places> {
        self.places.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock(&self) -> MutexGuard<'_, Memo> {
        self.memo.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The directories of `kept`'s tree to build, children first, each once, and the memo's
    /// hits, whose subtrees are not walked.
    fn plan<'a>(
        &self,
        kept: &'a KeptClosure,
        now: SystemTime,
        trust: Trust,
    ) -> Result<Plan<'a>, NoTree> {
        self.plan_of(kept.root(), &|digest| kept.directory(digest), now, trust)
    }

    /// [`TreeWriter::plan`] for the directory `root` of any set of directories `lookup` finds.
    /// With [`Trust::Store`] the memo has no hits: every directory is built.
    fn plan_of<'a>(
        &self,
        root: &Digest,
        lookup: &dyn Fn(&Digest) -> Option<&'a KeptDirectory>,
        now: SystemTime,
        trust: Trust,
    ) -> Result<Plan<'a>, NoTree> {
        let mut memo = self.lock();
        let mut order = Vec::new();
        let mut hits = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut stack: Vec<(Digest, Option<&KeptDirectory>)> = vec![(*root, None)];
        while let Some((digest, expanded)) = stack.pop() {
            if let Some(directory) = expanded {
                // Every subdirectory of it was popped, and built or found, before this.
                order.push((digest, directory));
                continue;
            }
            if !seen.insert(digest) {
                continue;
            }
            let hit = (trust == Trust::Memos).then(|| memo.get(&digest, now));
            if let Some(hit) = hit.flatten() {
                hits.insert(digest, hit);
                continue;
            }
            let directory = lookup(&digest).ok_or(NoTree::NotKept { directory: digest })?;
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

    /// Stores `built`, the manifests of `plan`'s directories in its order, at most
    /// [`place::WRITES_IN_FLIGHT`] at once, each write shared with every other preparation that
    /// needs it ([`flight`]); and remembers each directory as soon as its manifest and every
    /// manifest before it in the order are stored. The order is children first, so a directory
    /// is remembered only when its whole subtree is stored, and a preparation cut off part way
    /// (its time ran out, a write failed) keeps the subtrees it finished: the next tree of the
    /// same input root writes only the rest.
    ///
    /// # Errors
    /// [`NoTree::Put`]: the store refused a write; the directories before it stay remembered.
    async fn store_built(
        &self,
        plan: &Plan<'_>,
        built: &[Manifest],
        measures: &BTreeMap<Digest, Measure>,
        manifests: &BTreeMap<Digest, Digest>,
        places: &BTreeMap<Digest, Place>,
    ) -> Result<(), NoTree> {
        // Collected, as `place`'s asks are, so the future can be proved `Send`.
        let writes: Vec<_> = built
            .iter()
            .map(|manifest| {
                let bytes = manifest.bytes.clone();
                self.flights
                    .write(&self.store, &self.retry, manifest.digest, bytes)
            })
            .collect();
        let mut stored = stream::iter(writes)
            .buffered(place::WRITES_IN_FLIGHT)
            .zip(stream::iter(&plan.order));
        while let Some((answer, (digest, directory))) = stored.next().await {
            answer?;
            let children = directory.directories.iter().map(|(_, child)| *child);
            let packs: BTreeSet<&String> = (directory.files.iter())
                .filter_map(|(_, file, _)| match places.get(file) {
                    Some(Place::Packed { key, .. }) => Some(key),
                    _ => None,
                })
                .collect();
            self.lock().put(
                *digest,
                Remembered {
                    manifest: manifests[digest],
                    measure: measures[digest],
                    children: children.collect(),
                    packs: packs.into_iter().cloned().collect(),
                },
            );
        }
        Ok(())
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
