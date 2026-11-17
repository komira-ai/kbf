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

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use bytes::Bytes;
use kbf_objstore::ObjectKey;
use kbf_types::Digest;
use tierstore::{BlobSource, EntryKind, MAX_TREE_LIST_BYTES, TreeEntry, TreeLimits, TreeListV2};

use super::place::{self, Place};
use super::tstree::{
    self, Child, DIR_MODE, EMPTY_SHA256, ENTRY_BYTES, HEADER_LEN, Item, RANGE_MERGE_GAP,
};
use super::{BaseTree, INPUT_ROOT, NoTree, TreeWriter, reserved_input};
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
        let (places, unlocated) = self.locate(&blobs, now).await?;
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
        self.write_whole(unlocated, now).await?;
        place::write_object(&*self.store, &self.retry, &key, Bytes::from(bytes)).await?;
        Ok(Listing {
            key: key.to_string(),
        })
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
