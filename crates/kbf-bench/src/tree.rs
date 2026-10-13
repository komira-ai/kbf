//! The blobs a run sends or reads: an output tree, generated or read from a directory,
//! as the daemon would store it (each file a blob, each directory's REAPI `Directory`
//! message a blob, and one REAPI `Tree` message of them all), each distinct digest once.
//!
//! A generated tree is a function of its [`Shape`] and seed alone, so `kbf-bench read`
//! rebuilds the bytes `kbf-bench upload` sent and checks every blob against them. Every
//! non-empty file starts with the seed and its index (16 bytes), so two seeds share no
//! blob but the empty one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use kbf_proto::reapi::{Digest, Directory, DirectoryNode, FileNode, Tree as TreeMessage};
use prost::Message as _;
use sha2::{Digest as _, Sha256};

/// The bytes at the start of every non-empty file: the seed and the file's index,
/// little-endian.
pub const HEADER_BYTES: u64 = 16;

/// How many child directories a generated directory has at most.
const FAN_OUT: u64 = 8;

/// The output tree to generate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    /// Files, the empty ones included.
    pub files: u64,
    /// Empty files among them.
    pub empty: u64,
    /// Directories below the output directory itself.
    pub dirs: u64,
    /// The bytes of all files together.
    pub total_bytes: u64,
}

/// One blob to send or read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blob {
    pub digest: Digest,
    pub bytes: Vec<u8>,
}

/// An output tree, as blobs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
    /// What the contents are drawn from (generated) or salted with (read).
    pub seed: u64,
    pub files: u64,
    pub empty: u64,
    pub dirs: u64,
    /// The bytes of all files together (not the `Tree` message's).
    pub file_bytes: u64,
    /// Entries of a read directory that are neither files nor directories (symlinks,
    /// sockets): left out.
    pub skipped: u64,
    /// Every distinct blob, by digest: the files, each directory's `Directory`
    /// message (the output directory's too) and the `Tree` message of them all, what
    /// the daemon stores for an output directory.
    pub blobs: Vec<Blob>,
    /// The `Tree` message's digest (one of `blobs`).
    pub tree_digest: Digest,
}

/// Why a tree could not be made.
#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    #[error("{0}")]
    Shape(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The SHA-256 digest of `bytes`.
#[must_use]
pub fn digest_of(bytes: &[u8]) -> Digest {
    Digest {
        hash: hex::encode(Sha256::digest(bytes)),
        size_bytes: bytes.len() as i64,
    }
}

/// splitmix64: a fixed, portable stream of numbers from a seed.
struct Numbers(u64);

impl Numbers {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// The tree `shape` describes, its contents drawn from `seed`.
///
/// File sizes spread over about three decades (a size is `HEADER_BYTES` plus a share
/// of the rest proportional to a weight between 1 024 and 2 047 times a power of two
/// up to 2^9), and add up to `total_bytes` exactly. Empty files are spread evenly over
/// the indices. File `i` is `f<i>` in directory `i mod dirs` (or the output directory
/// when `dirs` is 0); directory `j` is `d<j>`, in directory `(j - 1) / 8` (directory 0
/// in the output directory).
///
/// # Errors
/// More empty files than files, or a total too small for each non-empty file's
/// header, or bytes but no non-empty file to hold them.
pub fn generate(shape: &Shape, seed: u64) -> Result<Tree, TreeError> {
    let fits = shape.files.checked_sub(shape.empty).filter(|&nonempty| {
        shape.total_bytes >= nonempty.saturating_mul(HEADER_BYTES)
            && (nonempty == 0) == (shape.total_bytes == 0)
    });
    let Some(nonempty) = fits else {
        return Err(TreeError::Shape(format!(
            "no tree has {} files, {} of them empty, holding {} bytes: each non-empty file \
             holds at least {HEADER_BYTES}",
            shape.files, shape.empty, shape.total_bytes
        )));
    };
    let header = nonempty * HEADER_BYTES;
    let mut numbers = Numbers(seed);
    let weights: Vec<u64> = (0..nonempty)
        .map(|_| {
            let n = numbers.next();
            (1024 + (n & 1023)) << ((n >> 10) % 10)
        })
        .collect();
    let all: u128 = weights.iter().map(|&w| u128::from(w)).sum();
    let extra = u128::from(shape.total_bytes - header);
    let mut sizes: Vec<u64> = weights
        .iter()
        // A share of `extra`, so it fits in u64.
        .map(|&w| HEADER_BYTES + (extra * u128::from(w) / all.max(1)) as u64)
        .collect();
    let short = shape.total_bytes - sizes.iter().sum::<u64>();
    // Each share was rounded down by less than one byte, so `short < nonempty`.
    for size in sizes.iter_mut().take(short as usize) {
        *size += 1;
    }
    let mut sizes = sizes.into_iter();
    let mut layout = Layout::default();
    for j in 0..shape.dirs {
        // Directory j is at index j + 1; its parent was placed before it.
        let parent = if j == 0 { 0 } else { 1 + (j - 1) / FAN_OUT };
        layout.dir(parent as usize, format!("d{j}"));
    }
    for i in 0..shape.files {
        // Empty files where `i * empty / files` steps: `empty` of them, evenly spread.
        let is_empty = (i * shape.empty) / shape.files != ((i + 1) * shape.empty) / shape.files;
        let bytes = if is_empty {
            Vec::new()
        } else {
            contents(seed, i, sizes.next().unwrap_or_default())
        };
        let dir = if shape.dirs == 0 {
            0
        } else {
            1 + i % shape.dirs
        };
        layout.file(dir as usize, format!("f{i}"), bytes);
    }
    Ok(layout.finish(seed, 0))
}

/// File `index`'s bytes: the header, then numbers from `seed` and `index`.
fn contents(seed: u64, index: u64, size: u64) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(size as usize + 8);
    bytes.extend_from_slice(&seed.to_le_bytes());
    bytes.extend_from_slice(&index.to_le_bytes());
    let mut numbers = Numbers(seed ^ index.rotate_left(32));
    while (bytes.len() as u64) < size {
        bytes.extend_from_slice(&numbers.next().to_le_bytes());
    }
    bytes.truncate(size as usize);
    bytes
}

/// The tree under `root`, read from the disk. Each non-empty file gets the seed and
/// its index in the walk appended (16 bytes), so a second run with another seed stores
/// every file again; symlinks and other entries are skipped and counted.
///
/// # Errors
/// A directory cannot be listed or a file cannot be read.
pub fn read_dir(root: &Path, seed: u64) -> Result<Tree, TreeError> {
    let mut layout = Layout::default();
    let mut pending = vec![(root.to_owned(), 0)];
    let mut skipped = 0;
    let mut index = 0_u64;
    while let Some((path, dir)) = pending.pop() {
        let mut entries = Vec::new();
        for entry in io(&path, std::fs::read_dir(&path))? {
            let entry = io(&path, entry)?;
            entries.push((
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            ));
        }
        // One order on every run, so the indices (and the bytes) repeat.
        entries.sort();
        for (name, child) in entries {
            let kind = io(&child, std::fs::symlink_metadata(&child))?.file_type();
            if kind.is_dir() {
                pending.push((child, layout.dir(dir, name)));
            } else if kind.is_file() {
                let mut bytes = io(&child, std::fs::read(&child))?;
                if !bytes.is_empty() {
                    bytes.extend_from_slice(&seed.to_le_bytes());
                    bytes.extend_from_slice(&index.to_le_bytes());
                }
                index += 1;
                layout.file(dir, name, bytes);
            } else {
                skipped += 1;
            }
        }
    }
    Ok(layout.finish(seed, skipped))
}

fn io<T>(path: &Path, result: std::io::Result<T>) -> Result<T, TreeError> {
    result.map_err(|source| TreeError::Io {
        path: path.to_owned(),
        source,
    })
}

/// Directories and their entries, the output directory first and every directory
/// after its parent, as [`Layout::finish`] needs; and the blobs, each digest once.
struct Layout {
    dirs: Vec<Entries>,
    files: u64,
    empty: u64,
    file_bytes: u64,
    blobs: BTreeMap<String, Blob>,
}

#[derive(Default)]
struct Entries {
    files: Vec<FileNode>,
    dirs: Vec<(String, usize)>,
}

impl Default for Layout {
    /// The output directory alone.
    fn default() -> Self {
        Self {
            dirs: vec![Entries::default()],
            files: 0,
            empty: 0,
            file_bytes: 0,
            blobs: BTreeMap::new(),
        }
    }
}

impl Layout {
    fn file(&mut self, dir: usize, name: String, bytes: Vec<u8>) {
        let digest = digest_of(&bytes);
        self.files += 1;
        self.empty += u64::from(bytes.is_empty());
        self.file_bytes += bytes.len() as u64;
        self.dirs[dir].files.push(FileNode {
            name,
            digest: Some(digest.clone()),
            ..FileNode::default()
        });
        self.add(Blob { digest, bytes });
    }

    /// A new directory `name` in directory `parent`; returns its index.
    fn dir(&mut self, parent: usize, name: String) -> usize {
        let child = self.dirs.len();
        self.dirs.push(Entries::default());
        self.dirs[parent].dirs.push((name, child));
        child
    }

    fn add(&mut self, blob: Blob) {
        self.blobs.entry(blob.digest.hash.clone()).or_insert(blob);
    }

    /// The tree's blobs: each directory's message, children before parents, each a
    /// blob, gathered in one `Tree` message, which is a blob too.
    fn finish(mut self, seed: u64, skipped: u64) -> Tree {
        let mut messages: Vec<Option<Directory>> = (0..self.dirs.len()).map(|_| None).collect();
        for at in (0..self.dirs.len()).rev() {
            let entries = std::mem::take(&mut self.dirs[at]);
            let mut files = entries.files;
            files.sort_by(|a, b| a.name.cmp(&b.name));
            let mut directories: Vec<DirectoryNode> = entries
                .dirs
                .into_iter()
                .map(|(name, child)| DirectoryNode {
                    name,
                    digest: messages[child]
                        .as_ref()
                        .map(|m| digest_of(&m.encode_to_vec())),
                })
                .collect();
            directories.sort_by(|a, b| a.name.cmp(&b.name));
            let message = Directory {
                files,
                directories,
                ..Directory::default()
            };
            // As the daemon's `store_directories`: every Directory the Tree names is
            // stored, so each digest in it can be fetched.
            let bytes = message.encode_to_vec();
            self.add(Blob {
                digest: digest_of(&bytes),
                bytes,
            });
            messages[at] = Some(message);
        }
        let mut messages = messages.into_iter().flatten();
        let tree = TreeMessage {
            root: messages.next(),
            children: messages.collect(),
        };
        let dirs = self.dirs.len() as u64 - 1;
        let tree = tree.encode_to_vec();
        let tree_digest = digest_of(&tree);
        self.add(Blob {
            digest: tree_digest.clone(),
            bytes: tree,
        });
        Tree {
            seed,
            files: self.files,
            empty: self.empty,
            dirs,
            file_bytes: self.file_bytes,
            skipped,
            blobs: self.blobs.into_values().collect(),
            tree_digest,
        }
    }
}
