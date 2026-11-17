//! The job's root tree as one tierstore tree list (docs/design/actions-as-jobs.md, 4.7): what
//! a job whose layer names tierstore's mount provider is made from.
//!
//! [`TreeWriter::write_listing`] lists the action tree [`TreeWriter::write`] would write as
//! manifests, as one tierstore tree list ([`TreeList`]): every entry of the
//! base tree ([`BaseTree::listed`]) at its path, `kbf-work` and every entry of the input root below
//! it, and `.kbf` with `action` and `command`. A file with bytes names the object they are in (a
//! span of a pack, or the whole object `cas/l/<sha256>`) and its SHA-256, which the provider
//! checks the bytes against; a file of no bytes names neither. Every directory is listed, so an
//! empty one is made too; a file's mode is 0755 if REAPI marks it executable and 0644 if not,
//! every directory's 0755.
//!
//! The blobs are made locatable as for the manifests ([`super::place`]), and the list is
//! stored with one create-only write at the key tierstore names it by, `cas/l/<sha256 of its
//! bytes>` ([`tierstore::tree_list_key`]): a retry, or another run with the same inputs, finds
//! it written and writes nothing. Before it
//! is stored the list is read back by tierstore's own reader, so nothing is stored that the
//! provider would refuse to read.

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;
use kbf_objstore::ObjectKey;
use kbf_types::{Digest, DigestFunction};
use tierstore::{
    BlobSource, EntryKind, ListEntry, ListKind, Mode, TreeDigest, TreeEntry, TreeList,
    parse_tree_list,
};

use super::place::{self, Place};
use super::{BaseTree, INPUT_ROOT, NoTree, TreeWriter, reserved_input};
use crate::large_backfill::large_key;
use crate::prepare_run::KeptClosure;

/// The mode of a file REAPI marks executable, and of every directory: 0755.
const EXECUTABLE: Mode = Mode::DIR;

/// The mode of any other file: 0644.
const PLAIN: Mode = Mode::FILE;

/// What an entry of a listed tree is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// A directory.
    Dir,
    /// A file: its contents' SHA-256 and length, whether it is executable, and the pack span its
    /// bytes are (none: the whole object `cas/l/<sha256>`, or no object for an empty file).
    File {
        sha256: [u8; 32],
        size: u64,
        exec: bool,
        span: Option<(Arc<str>, u64)>,
    },
    /// A symbolic link to `target`.
    Symlink { target: String },
}

/// One entry of a listed tree: its path below the root (names joined by `/`), and what it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) path: String,
    pub(super) kind: Kind,
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

/// The entries of the tree `flattened` (tierstore's [`tierstore::flatten`] of a base tree) at
/// their paths, the root left out.
pub(super) fn entries_of(flattened: &[TreeEntry]) -> Vec<Entry> {
    let mut paths: Vec<String> = Vec::with_capacity(flattened.len());
    let mut entries = Vec::with_capacity(flattened.len().saturating_sub(1));
    for entry in flattened {
        // `flatten` lists every directory before what it holds, from index 0, the root.
        let path = match entry.parent.and_then(|parent| paths.get(parent as usize)) {
            Some(parent) if parent.is_empty() => entry.name.to_string(),
            Some(parent) => format!("{parent}/{}", entry.name),
            None => String::new(),
        };
        paths.push(path.clone());
        if entry.parent.is_none() {
            continue;
        }
        let kind = match &entry.kind {
            EntryKind::Dir { .. } => Kind::Dir,
            EntryKind::File {
                digest,
                size,
                exec,
                source,
            } => Kind::File {
                sha256: *digest.as_bytes(),
                size: *size,
                exec: *exec,
                span: source.as_ref().map(|s| (Arc::clone(&s.key), s.offset)),
            },
            EntryKind::Symlink { target } => Kind::Symlink {
                target: target.to_string(),
            },
        };
        entries.push(Entry { path, kind });
    }
    entries
}

impl TreeWriter {
    /// Lists the action tree of `kept` over `base` as one tree list and stores it.
    ///
    /// # Errors
    /// [`NoTree`], by name: the base tree was not listed ([`NoTree::BaseNotListed`]), what the
    /// tree list cannot represent (`node_properties`, a reserved name, more entries than
    /// tierstore takes, a list its reader refuses), or a store that failed. Blobs written before
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
        let closure = kept.closure();
        let (action, command) = (*closure.action(), *closure.command());
        let mut entries: Vec<Entry> = listed.to_vec();
        entries.push(dir(INPUT_ROOT));
        entries.push(dir(".kbf"));
        entries.push(file(".kbf/action", &action, false));
        entries.push(file(".kbf/command", &command, false));
        walk(kept, &mut entries, self.list_entries)?;
        // The input files and `.kbf`'s: the base's are in the store since it was published.
        let blobs: BTreeSet<Digest> = entries[listed.len()..]
            .iter()
            .filter_map(|entry| match &entry.kind {
                Kind::File { sha256, size, .. } if *size > 0 => Some(blob(sha256, *size)),
                _ => None,
            })
            .collect();
        let blobs: Vec<Digest> = blobs.into_iter().collect();
        let now = (self.clock)();
        let (places, unlocated) = self.locate(&blobs, now).await?;
        for entry in &mut entries[listed.len()..] {
            if let Kind::File {
                sha256, size, span, ..
            } = &mut entry.kind
                && let Some(Place::Packed { key, offset }) = places.get(&blob(sha256, *size))
            {
                *span = Some((Arc::from(key.as_str()), *offset));
            }
        }
        let (key, bytes) = encode(&entries)?;
        self.write_whole(unlocated, now).await?;
        place::write_object(&*self.store, &self.retry, &key, Bytes::from(bytes)).await?;
        Ok(Listing {
            key: key.to_string(),
        })
    }
}

/// A directory entry at `path`.
fn dir(path: &str) -> Entry {
    Entry {
        path: path.to_owned(),
        kind: Kind::Dir,
    }
}

/// A file entry at `path` of the blob `digest`, its place not yet known.
fn file(path: &str, digest: &Digest, exec: bool) -> Entry {
    Entry {
        path: path.to_owned(),
        kind: Kind::File {
            sha256: digest.hash,
            size: digest.size_bytes,
            exec,
            span: None,
        },
    }
}

/// Lists every entry below `kept`'s input root under `kbf-work/`: each directory at each path it
/// is found at, before what it holds.
///
/// # Errors
/// [`NoTree::NodeProperties`], [`NoTree::NotKept`], or [`NoTree::ListTooLong`] as soon as
/// `entries` holds more than `limit` (a tree that names one directory at many paths lists it
/// at each): checked after each directory, the input root's too, so a list is never longer
/// than one directory over the limit.
fn walk(kept: &KeptClosure, entries: &mut Vec<Entry>, limit: u64) -> Result<(), NoTree> {
    let mut stack = vec![(INPUT_ROOT.to_owned(), *kept.root())];
    while let Some((path, digest)) = stack.pop() {
        let directory = kept
            .directory(&digest)
            .ok_or(NoTree::NotKept { directory: digest })?;
        if let Some(node) = &directory.properties {
            return Err(NoTree::NodeProperties {
                directory: digest,
                node: node.clone(),
            });
        }
        for (name, blob, exec) in &directory.files {
            entries.push(file(&format!("{path}/{name}"), blob, *exec));
        }
        for (name, target) in &directory.symlinks {
            entries.push(Entry {
                path: format!("{path}/{name}"),
                kind: Kind::Symlink {
                    target: target.clone(),
                },
            });
        }
        for (name, child) in &directory.directories {
            let below = format!("{path}/{name}");
            entries.push(dir(&below));
            stack.push((below, *child));
        }
        let count = entries.len() as u64;
        if count > limit {
            return Err(NoTree::ListTooLong {
                entries: count,
                limit,
            });
        }
    }
    Ok(())
}

/// The tree list of `entries`, read back by tierstore's reader, and its key.
///
/// # Errors
/// [`NoTree::ListingRefused`]: what the reader says (a name it refuses, a path twice, a list
/// over its size limit).
fn encode(entries: &[Entry]) -> Result<(ObjectKey, Vec<u8>), NoTree> {
    let entries = entries.iter().map(|entry| {
        let kind = match &entry.kind {
            Kind::Dir => ListKind::Dir { mode: EXECUTABLE },
            Kind::File {
                sha256,
                size,
                exec,
                span,
            } => {
                // Every file with bytes names its object, the whole `cas/l/<sha256>` too, and
                // its hash; a file of no bytes names neither.
                let source = match span {
                    _ if *size == 0 => None,
                    Some((key, offset)) => Some((Arc::clone(key), *offset)),
                    None => Some((Arc::from(large_key(&blob(sha256, *size)).as_str()), 0)),
                };
                ListKind::File {
                    mode: if *exec { EXECUTABLE } else { PLAIN },
                    size: *size,
                    source: source.map(|(key, offset)| BlobSource { key, offset }),
                    sha256: (*size > 0).then(|| TreeDigest::from_bytes(*sha256)),
                }
            }
            Kind::Symlink { target } => ListKind::Symlink {
                target: target.clone(),
            },
        };
        ListEntry {
            path: entry.path.clone(),
            kind,
        }
    });
    let bytes = TreeList {
        entries: entries.collect(),
    }
    .encode();
    parse_tree_list(&bytes, &TreeDigest::of(&bytes)).map_err(|error| NoTree::ListingRefused {
        why: error.to_string(),
    })?;
    // tierstore names a tree list `cas/l/<sha256 of its bytes>` (`tree_list_key`).
    let key = large_key(&kbf_segments::sha256(&bytes));
    Ok((key, bytes))
}

/// The blob of SHA-256 `sha256` and length `size`.
fn blob(sha256: &[u8; 32], size: u64) -> Digest {
    Digest::new(DigestFunction::Sha256, *sha256, size)
}
