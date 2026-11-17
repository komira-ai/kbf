//! The writer of tierstore's tree list, version 2: the `TSTREE` layout of tierstore's
//! `crates/tierstore/docs/tree-manifest.md` ("The tree list, version 2"), which tierstore's
//! mount provider reads with `tierstore::TreeListV2`.
//!
//! [`encode`] lays a tree out from its root, breadth first, asking `children` for what each
//! directory holds as it reaches it, so a tree is walked once and no path is ever made. The bytes
//! are what the reference's "Writing it the same way" says, so they equal what
//! `tierstore::TreeList::encode_v2` writes for the same entries when `gap` is
//! [`RANGE_MERGE_GAP`]:
//!
//! - a 64-byte header (the magic, major version 1, minor version 0, no flags, and the offset and
//!   count of each section), then the six sections in order, each at the next multiple of 8 with
//!   zero bytes between them;
//! - **entries**: one 32-byte record per entry, the root first, then breadth first, each
//!   directory's children together and sorted by name, bytewise. A directory names its first
//!   child and its number of children; a file its key, offset and size; a link its target's
//!   offset in the names and its length;
//! - **sha256**: the SHA-256 of each entry's bytes, zero for a directory or a link;
//! - **key offsets** and **keys**: the keys of the objects the files are in, sorted, each once,
//!   back to back;
//! - **names**: each name once, at the first entry that has it, and each link's target right
//!   after its link's name;
//! - **ranges**: the files' spans sorted by key and offset, two spans of one key merged when the
//!   second begins at most `gap` bytes after the end of the range before it. The ranges are a
//!   prefetch hint: the provider reads each file's own span and checks it against the file's
//!   digest.
//!
//! Every field a reader ignores is zero. Nothing is judged here that the reader judges (a name
//! twice, a name a tree may not have, a key over 1024 bytes): [`super::listing`] reads every list
//! back with tierstore's reader before it is stored. What a record cannot hold is refused here,
//! by name: a name over 255 bytes, and bytes over `max_bytes`.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, VecDeque};

use tierstore::TREE_LIST_V2_MAGIC;

use super::NoTree;

/// The gap across which the writer merges the spans of one pack into one range: two files of a
/// pack whose bytes are at most this far apart are one range.
pub(super) const RANGE_MERGE_GAP: u64 = 64 * 1024;

/// The major version this writer writes.
const MAJOR: u16 = 1;

/// The length of the header.
pub(super) const HEADER_LEN: usize = 64;

/// The length of an entry record.
const RECORD_LEN: usize = 32;

/// The length of a range record.
const RANGE_LEN: usize = 24;

/// What an entry costs in the object at the least: its record and its SHA-256.
pub(super) const ENTRY_BYTES: usize = RECORD_LEN + 32;

/// The kinds of the top four bits of a record's kind and mode.
const KIND_FILE: u16 = 0;
const KIND_DIR: u16 = 1;
const KIND_SYMLINK: u16 = 2;

/// The mode of the root, and of every directory kbf lists.
pub(super) const DIR_MODE: u16 = 0o755;

/// The longest name a record holds: its length is one byte.
const MAX_NAME: usize = 255;

/// The SHA-256 of no bytes: the digest of a file of size 0.
pub(super) const EMPTY_SHA256: [u8; 32] = [
    0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f, 0xb9, 0x24,
    0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b, 0x78, 0x52, 0xb8, 0x55,
];

/// One entry of a directory: its name and what it is. `D` is how the caller names a directory,
/// so that its children are asked for only when the walk reaches it.
pub(super) struct Child<'a, D> {
    pub(super) name: &'a str,
    pub(super) item: Item<'a, D>,
}

/// What an entry is.
pub(super) enum Item<'a, D> {
    /// A directory of `mode` (the permission bits with setuid, setgid and sticky).
    Dir { mode: u16, dir: D },
    /// A file of `size` bytes whose SHA-256 is `sha256`, and the object and offset its bytes
    /// are at (none for a file of size 0).
    File {
        mode: u16,
        size: u64,
        sha256: [u8; 32],
        source: Option<(Cow<'a, str>, u64)>,
    },
    /// A symbolic link to `target`.
    Link { target: &'a str },
}

/// A file with a source, kept until the keys are known: its record, key, offset and size.
struct Sourced<'a> {
    record: usize,
    key: Cow<'a, str>,
    offset: u64,
    size: u64,
}

/// The bytes of the tree list of the tree whose root is `root`, the children of each directory
/// those `children` gives for it (in any order: they are sorted here), spans of one key merged
/// across `gap` bytes.
///
/// # Errors
/// [`NoTree::ListingRefused`]: a name over 255 bytes, or more than `max_bytes` bytes.
pub(super) fn encode<'a, D: Copy>(
    root: D,
    mut children: impl FnMut(D) -> Vec<Child<'a, D>>,
    gap: u64,
    max_bytes: usize,
) -> Result<Vec<u8>, NoTree> {
    let mut entries: Vec<u8> = Vec::new();
    let mut digests: Vec<u8> = Vec::new();
    let mut names: Vec<u8> = Vec::new();
    let mut interned: HashMap<&'a str, u32> = HashMap::new();
    let mut sourced: Vec<Sourced<'a>> = Vec::new();
    // The directories not yet walked, in the order of their records: breadth first.
    let mut queue: VecDeque<(usize, D)> = VecDeque::new();
    entries.extend_from_slice(&record(0, 0, 0, kind_mode(KIND_DIR, DIR_MODE), 0, 0));
    digests.extend_from_slice(&[0; 32]);
    queue.push_back((0, root));
    let mut count: usize = 1;
    while let Some((at, dir)) = queue.pop_front() {
        let mut kids = children(dir);
        kids.sort_by(|a, b| a.name.cmp(b.name));
        // The directory's first child and number of children.
        let slot = at * RECORD_LEN;
        entries[slot + 8..slot + 12].copy_from_slice(&(count as u32).to_le_bytes());
        entries[slot + 16..slot + 24].copy_from_slice(&(kids.len() as u64).to_le_bytes());
        for kid in kids {
            if kid.name.len() > MAX_NAME {
                return Err(NoTree::ListingRefused {
                    why: format!(
                        "the name {:?} is {} bytes, over the {MAX_NAME} a record holds",
                        kid.name,
                        kid.name.len()
                    ),
                });
            }
            let name_at = *interned.entry(kid.name).or_insert_with(|| {
                let start = names.len() as u32;
                names.extend_from_slice(kid.name.as_bytes());
                start
            });
            let name_len = kid.name.len() as u8;
            let parent = at as u32;
            let mut digest = [0; 32];
            let bytes = match kid.item {
                Item::Dir { mode, dir } => {
                    queue.push_back((count, dir));
                    record(parent, name_at, name_len, kind_mode(KIND_DIR, mode), 0, 0)
                }
                Item::File {
                    mode,
                    size,
                    sha256,
                    source,
                } => {
                    digest = sha256;
                    let mut offset = 0;
                    if let Some((key, start)) = source {
                        // A file of size 0 has no key: its key and offset stay 0. Its key is
                        // still in the keys, as tierstore's writer keeps it.
                        if size > 0 {
                            offset = start;
                        }
                        sourced.push(Sourced {
                            record: count,
                            key,
                            offset: start,
                            size,
                        });
                    }
                    record(
                        parent,
                        name_at,
                        name_len,
                        kind_mode(KIND_FILE, mode),
                        offset,
                        size,
                    )
                }
                Item::Link { target } => {
                    let target_at = names.len() as u64;
                    names.extend_from_slice(target.as_bytes());
                    let length = target.len() as u64;
                    record(
                        parent,
                        name_at,
                        name_len,
                        kind_mode(KIND_SYMLINK, 0),
                        target_at,
                        length,
                    )
                }
            };
            entries.extend_from_slice(&bytes);
            digests.extend_from_slice(&digest);
            count += 1;
        }
        // Checked as the object grows, so a tree far over the limit is not laid out whole.
        if entries.len() + digests.len() + names.len() > max_bytes {
            return Err(too_large(
                entries.len() + digests.len() + names.len(),
                max_bytes,
            ));
        }
    }
    let keys: Vec<&str> = (sourced.iter().map(|file| &*file.key))
        .collect::<BTreeSet<&str>>()
        .into_iter()
        .collect();
    let mut spans = Vec::with_capacity(sourced.len());
    for file in &sourced {
        // Every key of a file is among the keys.
        let key = keys.binary_search(&&*file.key).unwrap_or_default() as u32;
        if file.size > 0 {
            let slot = file.record * RECORD_LEN;
            entries[slot + 8..slot + 12].copy_from_slice(&key.to_le_bytes());
            spans.push((key, file.offset, file.size));
        }
    }
    let ranges = merged(spans, gap);
    layout(&entries, &digests, &keys, &names, &ranges, count, max_bytes)
}

/// The kind and mode of a record: the kind in the top four bits, the mode in the low twelve.
const fn kind_mode(kind: u16, mode: u16) -> u16 {
    (kind << 12) | mode
}

/// One entry record: `a` is a file's offset or a link target's offset, `b` a file's size or a
/// link target's length. Bytes 8..12 (a directory's first child, a file's key) and a
/// directory's number of children are set once they are known.
fn record(parent: u32, name_at: u32, name_len: u8, kind: u16, a: u64, b: u64) -> [u8; RECORD_LEN] {
    let mut record = [0; RECORD_LEN];
    record[0..4].copy_from_slice(&parent.to_le_bytes());
    record[4..8].copy_from_slice(&name_at.to_le_bytes());
    record[12..14].copy_from_slice(&kind.to_le_bytes());
    record[14] = name_len;
    // Byte 15, the entry's flags, is zero.
    record[16..24].copy_from_slice(&a.to_le_bytes());
    record[24..32].copy_from_slice(&b.to_le_bytes());
    record
}

/// The ranges of `spans` ({key, offset, size} of each file with bytes): sorted, and merged where
/// a span of a key begins at most `gap` bytes after the end of the range before it. Each range is
/// {key, start, end}.
pub(super) fn merged(mut spans: Vec<(u32, u64, u64)>, gap: u64) -> Vec<(u32, u64, u64)> {
    spans.sort_unstable();
    let mut ranges: Vec<(u32, u64, u64)> = Vec::new();
    for (key, offset, size) in spans {
        let end = offset.saturating_add(size);
        match ranges.last_mut() {
            Some(last) if last.0 == key && offset <= last.2.saturating_add(gap) => {
                last.2 = last.2.max(end);
            }
            _ => ranges.push((key, offset, end)),
        }
    }
    ranges
}

/// The refusal of a list of `size` bytes.
fn too_large(size: usize, max: usize) -> NoTree {
    NoTree::ListingRefused {
        why: format!("the tree list is {size} bytes or more, over the {max} tierstore reads"),
    }
}

/// `at` rounded up to a multiple of 8.
const fn align8(at: usize) -> usize {
    at.div_ceil(8) * 8
}

/// The object: the header, then each section at the next multiple of 8, zero between them.
fn layout(
    entries: &[u8],
    digests: &[u8],
    keys: &[&str],
    names: &[u8],
    ranges: &[(u32, u64, u64)],
    count: usize,
    max_bytes: usize,
) -> Result<Vec<u8>, NoTree> {
    let mut offsets = Vec::with_capacity((keys.len() + 1) * 4);
    let mut end = 0usize;
    offsets.extend_from_slice(&0u32.to_le_bytes());
    for key in keys {
        end += key.len();
        offsets.extend_from_slice(&(end as u32).to_le_bytes());
    }
    let mut blob = Vec::with_capacity(end);
    for key in keys {
        blob.extend_from_slice(key.as_bytes());
    }
    let mut range_bytes = Vec::with_capacity(ranges.len() * RANGE_LEN);
    for (key, start, end) in ranges {
        range_bytes.extend_from_slice(&key.to_le_bytes());
        range_bytes.extend_from_slice(&0u32.to_le_bytes());
        range_bytes.extend_from_slice(&start.to_le_bytes());
        range_bytes.extend_from_slice(&(end - start).to_le_bytes());
    }
    let sections: [(&[u8], usize); 6] = [
        (entries, count),
        (digests, count),
        (&offsets, keys.len()),
        (&blob, blob.len()),
        (names, names.len()),
        (&range_bytes, ranges.len()),
    ];
    let mut at = HEADER_LEN;
    let mut places = [0usize; 6];
    for (place, (bytes, _)) in places.iter_mut().zip(&sections) {
        *place = align8(at);
        at = *place + bytes.len();
    }
    if at > max_bytes {
        return Err(too_large(at, max_bytes));
    }
    // Every offset and count below fits in 32 bits: the whole is at most `max_bytes`, which is
    // at most tierstore's 256 MiB.
    let mut out = Vec::with_capacity(at);
    out.extend_from_slice(&TREE_LIST_V2_MAGIC);
    out.extend_from_slice(&MAJOR.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for (place, (_, count)) in places.iter().zip(&sections) {
        out.extend_from_slice(&(*place as u32).to_le_bytes());
        out.extend_from_slice(&(*count as u32).to_le_bytes());
    }
    for (place, (bytes, _)) in places.iter().zip(&sections) {
        out.resize(*place, 0);
        out.extend_from_slice(bytes);
    }
    Ok(out)
}
