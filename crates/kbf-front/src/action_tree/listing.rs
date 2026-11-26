//! The job's root tree as one tierstore tree list (docs/design/actions-as-jobs.md, 4.7): what
//! a job whose layer names tierstore's mount provider is made from.
//!
//! [`TreeWriter::write_listing`] lists the action tree [`TreeWriter::write`] would write as
//! manifests as one tierstore tree list, version 2 (the `TSTREE` layout, [`super::tstree`]):
//! every entry of the base tree ([`BaseTree::listed`]), `kbf-work` ([`INPUT_ROOT`]) and every entry of the input root
//! below it, and `.kbf` with `action` and `command`. A file with bytes names the object they are
//! in (a span of a pack, or the whole object `cas/l/<sha256>`) and its SHA-256, which the
//! provider checks the bytes against; a file of no bytes names no object and has the SHA-256 of
//! no bytes. Every directory is an entry, so an empty one is made too; a file's mode is 0755 if
//! REAPI marks it executable and 0644 if not, every directory's 0755. The ranges table holds the
//! files' spans, those of one pack merged across [`RANGE_MERGE_GAP`].
//!
//! The list is laid out from the root breadth first, straight from the base tree's entries and
//! the input root's `Directory` messages: a `Directory` named at several paths is listed at each,
//! and is read once. Before anything is looked up, the input root is surveyed once per distinct
//! `Directory`: what it refuses (a `Directory` the closure did not keep, `node_properties`) is
//! refused, and the entries the list would have are counted, so a list over its limit is refused
//! before it is made.
//!
//! The blobs are made locatable as for the manifests ([`super::place`]), and the list is
//! stored with one create-only write at the key tierstore names it by, `cas/l/<sha256 of its
//! bytes>` ([`tierstore::tree_list_key`]): a retry, or another run with the same inputs, finds
//! it written and writes nothing. Before it is stored the list is read back by tierstore's own
//! reader (`TreeListV2::open`, then `build`), so nothing is stored that the provider would refuse
//! to read.
//!
//! With sidecars ([`TreeWriter::with_sidecars`]), the list's L2 sidecar is written once the list
//! is stored: the sidecar's HEAD and the pack index's cells query run beside the list's write,
//! since they read and write nothing the list needs, and the sidecar's write (or the tree's
//! unlisted row) follows the list's, so no sidecar names a list the store does not have. A list
//! whose write fails fails the tree, and what its sidecar read is dropped; a sidecar that fails
//! never fails the list. The miss path's backfill ([`ListingWrite::republish`]) reads objects
//! and records cells, and runs after the list's write.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::future::join;
use kbf_objstore::ObjectKey;
use kbf_types::Digest;
use tierstore::{BlobSource, EntryKind, MAX_TREE_LIST_BYTES, TreeEntry, TreeLimits, TreeListV2};
use tokio::time::Instant;

use super::place::{self, Place};
use super::sidecar::{SidecarError, Sidecars, Staged};
use super::tstree::{
    self, Child, DIR_MODE, EMPTY_SHA256, ENTRY_BYTES, HEADER_LEN, Item, RANGE_MERGE_GAP,
};
use super::waits::tallied;
use super::{
    BaseTree, INPUT_ROOT, NoTree, TreeWriter, Trust, WriteStage, WriteTimed, reserved_input,
};
use crate::large_backfill::large_key;
use crate::prepare_run::{KeptClosure, KeptDirectory};

/// The mode of a file REAPI marks executable.
const EXECUTABLE: u16 = 0o755;

/// The mode of any other file.
const PLAIN: u16 = 0o644;

/// The most entries below the root a list can have and still fit in tierstore's
/// [`MAX_TREE_LIST_BYTES`]: each costs its record and its SHA-256, the root's too.
pub(super) const FITTING_ENTRIES: u64 =
    ((MAX_TREE_LIST_BYTES - HEADER_LEN) / ENTRY_BYTES) as u64 - 1;

/// How [`TreeWriter::write_listing_with`] writes a tree list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ListingWrite {
    /// No place is taken from the place memo and every pack the list names is asked after in
    /// the store, as [`TreeWriter::write_listing_checked`] does: a job of the action found an
    /// object of its tree lost.
    pub checked: bool,
    /// The miss path of a list a node refused as not L2 (`ListVersionUnsupported`, tierstore's
    /// design 3.5): every object the list names whose cells are not recorded is read once, its
    /// cells recorded, and the list's L2 sidecar written, so the next job of the action finds
    /// it.
    pub republish: bool,
}

/// The base tree's entries as tierstore's [`tierstore::flatten`] gave them, the root first, and
/// the children of each, so that a list is laid out from them without a path.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Listed {
    /// The entries, by index; a file with bytes and no source has its whole object
    /// `cas/l/<sha256>` as its source, at offset 0.
    entries: Vec<TreeEntry>,
    /// Where the children of entry `i` begin in `children`: they are
    /// `children[starts[i]..starts[i + 1]]`, in the order of their manifest, sorted by name.
    starts: Vec<u32>,
    children: Vec<u32>,
}

impl Listed {
    /// The base tree `flattened` (the root first, every directory before what it holds).
    pub(super) fn new(flattened: Vec<TreeEntry>) -> Self {
        let mut entries = flattened;
        let mut counts = vec![0u32; entries.len() + 1];
        for entry in &mut entries {
            if let Some(parent) = entry.parent {
                counts[parent as usize + 1] += 1;
            }
            if let EntryKind::File {
                digest,
                size,
                source,
                ..
            } = &mut entry.kind
                && *size > 0
                && source.is_none()
            {
                *source = Some(BlobSource {
                    key: Arc::from(format!("cas/l/{digest}")),
                    offset: 0,
                });
            }
        }
        let mut starts = counts;
        for at in 1..starts.len() {
            starts[at] += starts[at - 1];
        }
        let mut next = starts.clone();
        let mut children = vec![0u32; entries.len().saturating_sub(1)];
        for entry in &entries {
            if let Some(parent) = entry.parent {
                let slot = &mut next[parent as usize];
                children[*slot as usize] = entry.index;
                *slot += 1;
            }
        }
        Self {
            entries,
            starts,
            children,
        }
    }

    /// The entries below the root.
    pub(super) fn len(&self) -> u64 {
        self.entries.len().saturating_sub(1) as u64
    }

    /// The children of the entry `dir`, as a list lays them out.
    fn children_of(&self, dir: u32) -> impl Iterator<Item = Child<'_, At<'_>>> {
        let at = dir as usize;
        let range = self.starts[at] as usize..self.starts[at + 1] as usize;
        self.children[range].iter().map(|child| {
            let entry = &self.entries[*child as usize];
            let item = match &entry.kind {
                EntryKind::Dir { .. } => Item::Dir {
                    mode: DIR_MODE,
                    dir: At::Base(entry.index),
                },
                EntryKind::File {
                    digest,
                    size,
                    exec,
                    source,
                } => file_item(
                    *exec,
                    *size,
                    digest.as_bytes(),
                    source
                        .as_ref()
                        .map(|source| (Cow::Borrowed(&*source.key), source.offset)),
                ),
                EntryKind::Symlink { target } => Item::Link { target },
            };
            Child {
                name: &entry.name,
                item,
            }
        })
    }
}

/// A directory of the action tree, as the list's layout asks for its children.
#[derive(Clone, Copy)]
enum At<'a> {
    /// The root: the base root's children, `kbf-work` and `.kbf`.
    Root,
    /// A directory of the base tree, by its index.
    Base(u32),
    /// A directory of the input root.
    Input(&'a KeptDirectory),
    /// `.kbf`: `action` and `command`.
    Kbf,
}

/// A written listing: the key of the tree list in the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    key: String,
}

impl Listing {
    /// The key of the tree list: what the job's layer names for the provider to read.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }
}

/// What the survey of an input root found: every distinct `Directory` below it, the entries
/// below it (a `Directory` at several paths counted at each, at most `u64::MAX`), and the
/// blobs of its files.
struct Survey<'a> {
    directories: BTreeMap<Digest, &'a KeptDirectory>,
    entries: u64,
    blobs: BTreeSet<Digest>,
}

impl TreeWriter {
    /// Lists the action tree of `kept` over `base` as one tree list and stores it.
    ///
    /// # Errors
    /// [`NoTree`], by name: the base tree was not listed ([`NoTree::BaseNotListed`]), what the
    /// tree list cannot represent (`node_properties`, a reserved name, more entries than the
    /// writer takes, a list its reader refuses), or a store that failed. Blobs written before
    /// a failure stay: each is content-bound, and nothing names them until a list is returned.
    pub async fn write_listing(
        &self,
        kept: &KeptClosure,
        base: &BaseTree,
    ) -> Result<Listing, NoTree> {
        self.write_listing_as(kept, base, (Trust::Memos, false), |_| (), untimed)
            .await
    }

    /// As [`TreeWriter::write_listing`], and calls `named` with the listing as soon as its key
    /// is known: after the blobs are located and the list is encoded and read back, before the
    /// first blob or the list is written. A list that is refused, or whose lookups fail, is
    /// never named. A listing handed to `named` is not yet stored: the writes that follow it
    /// may still fail, and only the listing this returns is in the store. The list is written
    /// last, after every blob it names.
    ///
    /// # Errors
    /// As [`TreeWriter::write_listing`], except that a write that fails after `named` ran leaves
    /// the key handed out for a list that is not in the store; the blobs written before it stay.
    pub async fn write_listing_named(
        &self,
        kept: &KeptClosure,
        base: &BaseTree,
        named: impl FnOnce(&Listing) + Send,
    ) -> Result<Listing, NoTree> {
        self.write_listing_as(kept, base, (Trust::Memos, false), named, untimed)
            .await
    }

    /// [`TreeWriter::write_listing`] for a tree whose last job found an object lost: no place
    /// is taken from the place memo, and every pack the list names is asked after in the store
    /// ([`super::check`]).
    ///
    /// # Errors
    /// As [`TreeWriter::write_listing`]; and [`NoTree::Lost`] when the store lacks a pack the
    /// index names, which the index then forgets.
    pub async fn write_listing_checked(
        &self,
        kept: &KeptClosure,
        base: &BaseTree,
    ) -> Result<Listing, NoTree> {
        self.write_listing_as(kept, base, (Trust::Store, false), |_| (), untimed)
            .await
    }

    /// [`TreeWriter::write_listing_checked`], naming the list as
    /// [`TreeWriter::write_listing_named`] does: a pack the store lost is found before the
    /// list is named, so a list that is [`NoTree::Lost`] is never named.
    ///
    /// # Errors
    /// As [`TreeWriter::write_listing_checked`] and [`TreeWriter::write_listing_named`].
    pub async fn write_listing_checked_named(
        &self,
        kept: &KeptClosure,
        base: &BaseTree,
        named: impl FnOnce(&Listing) + Send,
    ) -> Result<Listing, NoTree> {
        self.write_listing_as(kept, base, (Trust::Store, false), named, untimed)
            .await
    }

    /// Lists the action tree of `kept` over `base` as one tree list and stores it as `how`
    /// says, naming it as [`TreeWriter::write_listing_named`] does: checked against the store
    /// as [`TreeWriter::write_listing_checked`] is, and with its L2 sidecar republished
    /// ([`ListingWrite::republish`]). `timed` is told each stage the writes make and how long it
    /// took, on tokio's clock, once it ended ([`WriteTimed::Stage`]): the blobs that were nowhere
    /// ([`WriteStage::Blobs`], a list with none has no such stage), the list's write, a failed one
    /// included ([`WriteStage::List`]), and its sidecar, once the list is stored
    /// ([`WriteStage::Sidecar`]); and, just before the list's stage, where the time of the list's
    /// one write went ([`WriteTimed::Write`]). A stage cut off part way (the write was dropped) is
    /// not timed.
    ///
    /// # Errors
    /// As [`TreeWriter::write_listing_checked_named`].
    pub async fn write_listing_with(
        &self,
        kept: &KeptClosure,
        base: &BaseTree,
        how: ListingWrite,
        named: impl FnOnce(&Listing) + Send,
        timed: impl Fn(WriteTimed) + Send + Sync,
    ) -> Result<Listing, NoTree> {
        let trust = match how.checked {
            true => Trust::Store,
            false => Trust::Memos,
        };
        let how = (trust, how.republish);
        self.write_listing_as(kept, base, how, named, timed).await
    }

    async fn write_listing_as(
        &self,
        kept: &KeptClosure,
        base: &BaseTree,
        (trust, republish): (Trust, bool),
        named: impl FnOnce(&Listing) + Send,
        timed: impl Fn(WriteTimed) + Send + Sync,
    ) -> Result<Listing, NoTree> {
        reserved_input(kept)?;
        let Some(listed) = &base.listing else {
            return Err(NoTree::BaseNotListed);
        };
        let survey = survey(kept)?;
        // `kbf-work`, `.kbf` and `.kbf`'s two files.
        let entries = (listed.len().saturating_add(4)).saturating_add(survey.entries);
        let limit = self.list_entries.min(FITTING_ENTRIES);
        if entries > limit {
            return Err(NoTree::ListTooLong { entries, limit });
        }
        let closure = kept.closure();
        let (action, command) = (*closure.action(), *closure.command());
        // The input files and `.kbf`'s: the base's are in the store since it was published.
        let blobs: Vec<Digest> = (survey.blobs.iter().copied())
            .chain([action, command])
            .filter(|digest| digest.size_bytes > 0)
            .collect::<BTreeSet<Digest>>()
            .into_iter()
            .collect();
        let now = (self.clock)();
        let (places, unlocated) = self.locate(&blobs, now, trust).await?;
        let lay = Layout {
            listed,
            directories: &survey.directories,
            places: &places,
            root: kept.root(),
            action: &action,
            command: &command,
        };
        let bytes = tstree::encode(
            At::Root,
            |at| lay.children(at),
            RANGE_MERGE_GAP,
            MAX_TREE_LIST_BYTES,
        )?;
        read_back(&bytes)?;
        // tierstore names a tree list `cas/l/<sha256 of its bytes>` (`tree_list_key`).
        let key: ObjectKey = large_key(&kbf_segments::sha256(&bytes));
        let listing = Listing {
            key: key.to_string(),
        };
        named(&listing);
        if !unlocated.is_empty() {
            let started = Instant::now();
            let written = self.write_whole(unlocated, now).await;
            timed(WriteTimed::Stage(WriteStage::Blobs, started.elapsed()));
            written?;
        }
        let bytes = Bytes::from(bytes);
        let put = async {
            let started = Instant::now();
            let write = place::write_object(&*self.store, &self.retry, &key, bytes.clone());
            // A tree list takes no manifest slot.
            let (put, waits) = tallied(Duration::ZERO, write).await;
            timed(WriteTimed::Write(WriteStage::List, waits));
            timed(WriteTimed::Stage(WriteStage::List, started.elapsed()));
            put
        };
        let Some(sidecars) = &self.sidecars else {
            put.await?;
            return Ok(listing);
        };
        let started = Instant::now();
        // Dual emission reads beside the list's write and writes after it; the miss path's
        // backfill reads and records after it.
        let (put, staged) = match republish {
            false => {
                let (put, staged) = join(put, sidecars.stage(&bytes)).await;
                (put, Some(staged))
            }
            true => (put.await, None),
        };
        put?;
        sidecar(sidecars, &key, &bytes, staged).await;
        timed(WriteTimed::Stage(WriteStage::Sidecar, started.elapsed()));
        Ok(listing)
    }
}

/// The timer of a write whose stages no one times.
fn untimed(_: WriteTimed) {}

/// Writes the L2 sidecar of the list `bytes` stored at `key`: from recorded cells only, as
/// `staged` found them beside the list's write, or, on the miss path (no `staged`), reading each
/// object that has none once. A failure is logged and never fails the list: old readers read
/// the list, and a new reader's refusal of a tree with no sidecar takes the executor's miss path.
async fn sidecar(
    sidecars: &Sidecars,
    key: &ObjectKey,
    bytes: &[u8],
    staged: Option<Result<Staged, SidecarError>>,
) {
    let republish = staged.is_none();
    let written = match staged {
        Some(staged) => sidecars
            .emit_staged(bytes, staged)
            .await
            .map(|emitted| format!("{emitted:?}")),
        None => sidecars
            .backfill(bytes)
            .await
            .map(|done| format!("{done:?}")),
    };
    match written {
        Ok(done) => tracing::debug!(list = %key, republish, done, "the tree list's L2 sidecar"),
        Err(error) => {
            tracing::warn!(list = %key, republish, %error, "the tree list's L2 sidecar was not written")
        }
    }
}

/// Every distinct `Directory` of `kept`'s input root, children before the directories that hold
/// them, and what the list of the root would have below it.
///
/// # Errors
/// [`NoTree::NotKept`] or [`NoTree::NodeProperties`], for the first such `Directory` found.
fn survey(kept: &KeptClosure) -> Result<Survey<'_>, NoTree> {
    let mut directories = BTreeMap::new();
    let mut order = Vec::new();
    let mut blobs = BTreeSet::new();
    let mut stack: Vec<(Digest, bool)> = vec![(*kept.root(), false)];
    while let Some((digest, expanded)) = stack.pop() {
        if expanded {
            // Every subdirectory of it was surveyed before this.
            order.push(digest);
            continue;
        }
        if directories.contains_key(&digest) {
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
        directories.insert(digest, directory);
        blobs.extend(directory.files.iter().map(|(_, blob, _)| *blob));
        stack.push((digest, true));
        let children = directory.directories.iter();
        stack.extend(children.map(|(_, child)| (*child, false)));
    }
    let mut below: BTreeMap<Digest, u64> = BTreeMap::new();
    for digest in &order {
        let directory = directories[digest];
        let leaves = directory.files.len() + directory.symlinks.len();
        let mut entries = leaves as u64;
        for (_, child) in &directory.directories {
            // Children first: every subdirectory is counted.
            entries = entries.saturating_add(1).saturating_add(below[child]);
        }
        below.insert(*digest, entries);
    }
    Ok(Survey {
        entries: below[kept.root()],
        directories,
        blobs,
    })
}

/// What the layout of one action tree's list reads.
struct Layout<'a> {
    listed: &'a Listed,
    directories: &'a BTreeMap<Digest, &'a KeptDirectory>,
    places: &'a BTreeMap<Digest, Place>,
    root: &'a Digest,
    action: &'a Digest,
    command: &'a Digest,
}

impl<'a> Layout<'a> {
    /// The children of the directory `at`.
    fn children(&self, at: At<'a>) -> Vec<Child<'a, At<'a>>> {
        match at {
            At::Root => {
                let mut children: Vec<Child<'a, At<'a>>> = self.listed.children_of(0).collect();
                let work = self.directories[self.root];
                children.push(dir(INPUT_ROOT, At::Input(work)));
                children.push(dir(".kbf", At::Kbf));
                children
            }
            At::Base(index) => self.listed.children_of(index).collect(),
            At::Input(directory) => {
                let files = directory.files.iter().map(|(name, blob, exec)| Child {
                    name,
                    item: self.file(blob, *exec),
                });
                let directories = directory.directories.iter().map(|(name, child)| {
                    let child = self.directories[child];
                    dir(name, At::Input(child))
                });
                let links = directory.symlinks.iter().map(|(name, target)| Child {
                    name,
                    item: Item::Link { target },
                });
                files.chain(directories).chain(links).collect()
            }
            At::Kbf => vec![
                Child {
                    name: "action",
                    item: self.file(self.action, false),
                },
                Child {
                    name: "command",
                    item: self.file(self.command, false),
                },
            ],
        }
    }

    /// The file of the blob `blob`: a span of its pack, or its whole object.
    fn file(&self, blob: &Digest, exec: bool) -> Item<'a, At<'a>> {
        // A file of no bytes names no object.
        let source = (blob.size_bytes > 0).then(|| match self.places.get(blob) {
            Some(Place::Packed { key, offset }) => (Cow::Borrowed(key.as_str()), *offset),
            _ => (Cow::Owned(large_key(blob).to_string()), 0),
        });
        file_item(exec, blob.size_bytes, &blob.hash, source)
    }
}

/// A directory entry, 0755.
fn dir<'a>(name: &'a str, at: At<'a>) -> Child<'a, At<'a>> {
    Child {
        name,
        item: Item::Dir {
            mode: DIR_MODE,
            dir: at,
        },
    }
}

/// A file of `size` bytes whose SHA-256 is `sha256` at `source`: a file of no bytes has the
/// SHA-256 of no bytes and no source, whatever its digest says.
fn file_item<'a>(
    exec: bool,
    size: u64,
    sha256: &[u8; 32],
    source: Option<(Cow<'a, str>, u64)>,
) -> Item<'a, At<'a>> {
    Item::File {
        mode: if exec { EXECUTABLE } else { PLAIN },
        size,
        sha256: if size == 0 { EMPTY_SHA256 } else { *sha256 },
        source: source.filter(|_| size > 0),
    }
}

/// Reads `bytes` back with tierstore's reader, as its mount provider reads a list.
///
/// # Errors
/// [`NoTree::ListingRefused`]: what the reader says (a name it refuses, a name twice, a key or
/// a path over its length, a tree over its limits).
fn read_back(bytes: &[u8]) -> Result<(), NoTree> {
    let refused = |error: tierstore::TreeListV2Error| NoTree::ListingRefused {
        why: error.to_string(),
    };
    let list = TreeListV2::open(bytes).map_err(refused)?;
    list.build(&TreeLimits::default(), |_| {})
        .map_err(refused)?;
    Ok(())
}
