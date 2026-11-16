//! The neutral file index of a one-pass object (`.kbf/index.json`, docs/design/one-pass-task-object.md
//! section 3), and what the frontend derives from it: the REAPI `Directory` and `Tree` messages of
//! each output directory, which the frontend checks against the manifest's tree digests, and the
//! `Directory` records the tierstore manifests are built from ([`crate::action_tree`]).
//!
//! The index names every output file and symlink and every directory by its full path, a file with
//! its size, executable bit, SHA-256 and offset in the object. It is in no consumer's format; this
//! module is the one place it becomes REAPI's (and, through [`crate::action_tree::TreeWriter`],
//! tierstore's). The two name a folder with different digests (the SHA-256 of the `Directory`
//! message, the SHA-256 of the manifest JSON); the file digests are shared.
//!
//! The derived messages are those `kbf-outputs` builds from the directory on disk: names sorted
//! bytewise, a `Directory`'s files, directories and symlinks each in that order, and a `Tree`'s
//! children in the order the walk opens them (pre-order, the root apart). Any other order gives
//! another `Tree` digest, which the manifest would not match, so a task whose index and manifest
//! disagree is refused.

use std::collections::{BTreeMap, BTreeSet};

use kbf_proto::reapi::{Directory, DirectoryNode, FileNode, SymlinkNode, Tree};
use kbf_types::{Digest, DigestFunction};
use prost::Message as _;
use serde_json::Value;

use crate::prepare_run::KeptDirectory;
use crate::wire::digest_to_proto;

/// The deepest directory the derivation follows: `kbf-outputs` stops a walk at 512 by default and
/// an operator may raise it; an index deeper than this is refused rather than followed.
pub(crate) const MAX_DEPTH: usize = 4096;

/// Why an index is not one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum IndexError {
    /// The bytes are not JSON of the shape of the index.
    #[error("the file index is not the index of an output: {0}")]
    Shape(String),
    /// A path that is empty, absolute, or has an empty, `.` or `..` part.
    #[error("the file index names the path {0:?}, which is not a relative path in normal form")]
    Path(String),
    /// A path named twice, or as two kinds.
    #[error("the file index names {0:?} twice")]
    Twice(String),
    /// A directory below a file or a symlink, or a file at a directory's path.
    #[error("the file index has {0:?} below something that is not a directory")]
    NotDirectory(String),
    /// The output directory is not in the index.
    #[error("the file index has no output directory {0:?}")]
    NoDirectory(String),
    /// Directories nested deeper than [`MAX_DEPTH`].
    #[error("the file index nests directories deeper than {MAX_DEPTH}")]
    TooDeep,
}

/// What a name is in a directory.
#[derive(Clone, Debug)]
enum Entry {
    File { digest: Digest, exec: bool },
    Link(String),
    Dir,
}

/// One directory being derived.
struct Frame<'a> {
    entries: std::vec::IntoIter<(&'a String, &'a Entry)>,
    message: Directory,
    kept: KeptDirectory,
    dir: String,
    name: String,
    slot: usize,
}

/// A file of the index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IndexedFile {
    /// The file's digest (its size is the digest's).
    pub(crate) digest: Digest,
    /// Whether it is executable.
    pub(crate) exec: bool,
}

/// A parsed file index.
#[derive(Debug, Default)]
pub(crate) struct OutputIndex {
    /// Every name by the directory it is in (`""` for the top), and sorted by name.
    by_parent: BTreeMap<String, BTreeMap<String, Entry>>,
    /// The index's files by full path.
    files: BTreeMap<String, IndexedFile>,
    /// The index's symlinks by full path.
    links: BTreeMap<String, String>,
}

/// The tree of one output directory, derived.
#[derive(Debug)]
pub(crate) struct DerivedTree {
    /// The output directory's path.
    pub(crate) path: String,
    /// The SHA-256 digest of the `Tree` message: what the manifest says.
    pub(crate) digest: Digest,
    /// The digest of the root `Directory`.
    pub(crate) root: Digest,
    /// Every `Directory` of the tree, by digest, for the manifests.
    pub(crate) directories: BTreeMap<Digest, KeptDirectory>,
}

fn normal(path: &str) -> bool {
    !path.is_empty() && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

fn hex_hash(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    let nibble = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    let mut hash = [0u8; 32];
    for (at, slot) in hash.iter_mut().enumerate() {
        let pair = bytes.get(2 * at..2 * at + 2)?;
        *slot = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    (bytes.len() == 64).then_some(hash)
}

fn directory_digest(directory: &Directory) -> Digest {
    kbf_segments::sha256(&directory.encode_to_vec())
}

fn shape(why: &str) -> IndexError {
    IndexError::Shape(why.to_owned())
}

impl DerivedTree {
    /// The `Tree`'s digest as hex, for comparing with a REAPI result's.
    #[cfg(test)]
    pub(crate) fn digest_hex(&self) -> String {
        self.digest.hash_hex()
    }
}

impl OutputIndex {
    /// Parses the index `bytes`.
    ///
    /// # Errors
    /// [`IndexError`] for bytes that are not an index, a path not in normal form, a path twice or
    /// below a file.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, IndexError> {
        let value: Value =
            serde_json::from_slice(bytes).map_err(|e| IndexError::Shape(e.to_string()))?;
        let list = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_array)
                .ok_or_else(|| shape(name))
        };
        let mut index = Self::default();
        let mut kinds: BTreeMap<String, &'static str> = BTreeMap::new();
        for file in list("files")? {
            let path = file
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| shape("path"))?;
            let size = file
                .get("size")
                .and_then(Value::as_u64)
                .ok_or_else(|| shape("size"))?;
            let exec = file
                .get("exec")
                .and_then(Value::as_bool)
                .ok_or_else(|| shape("exec"))?;
            let sha = file
                .get("sha256")
                .and_then(Value::as_str)
                .and_then(hex_hash);
            let digest = Digest::new(
                DigestFunction::Sha256,
                sha.ok_or_else(|| shape("sha256"))?,
                size,
            );
            index.add(&mut kinds, path, "file", Entry::File { digest, exec })?;
            index
                .files
                .insert(path.to_owned(), IndexedFile { digest, exec });
        }
        for link in list("symlinks")? {
            let path = link
                .get("path")
                .and_then(Value::as_str)
                .ok_or_else(|| shape("path"))?;
            let target = link
                .get("target")
                .and_then(Value::as_str)
                .ok_or_else(|| shape("target"))?;
            index.add(&mut kinds, path, "link", Entry::Link(target.to_owned()))?;
            index.links.insert(path.to_owned(), target.to_owned());
        }
        for dir in list("dirs")? {
            let path = dir.as_str().ok_or_else(|| shape("dirs"))?;
            index.add(&mut kinds, path, "dir", Entry::Dir)?;
        }
        // Nothing may be below a file or a symlink.
        for path in kinds.keys() {
            let above = path
                .rsplit_once('/')
                .and_then(|(parent, _)| kinds.get(parent));
            if matches!(above, Some(&"file" | &"link")) {
                return Err(IndexError::NotDirectory(path.clone()));
            }
        }
        Ok(index)
    }

    fn add(
        &mut self,
        kinds: &mut BTreeMap<String, &'static str>,
        path: &str,
        kind: &'static str,
        entry: Entry,
    ) -> Result<(), IndexError> {
        if !normal(path) {
            return Err(IndexError::Path(path.to_owned()));
        }
        if kinds.insert(path.to_owned(), kind).is_some() {
            return Err(IndexError::Twice(path.to_owned()));
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        self.by_parent
            .entry(parent.to_owned())
            .or_default()
            .insert(name.to_owned(), entry);
        Ok(())
    }

    /// The file at `path`, if the index has one.
    pub(crate) fn file(&self, path: &str) -> Option<&IndexedFile> {
        self.files.get(path)
    }

    /// The target of the symlink at `path`, if the index has one.
    pub(crate) fn link(&self, path: &str) -> Option<&str> {
        self.links.get(path).map(String::as_str)
    }

    /// Every distinct non-empty digest of a file, for the check that the object holds it.
    pub(crate) fn digests(&self) -> BTreeSet<Digest> {
        let sized = self.files.values().filter(|f| f.digest.size_bytes != 0);
        sized.map(|f| f.digest).collect()
    }

    /// The `Directory` at `dir` and its tierstore form, its subdirectories pushed to `children` in
    /// the order the walk of `kbf-outputs` opens them. The levels are a stack of frames on the
    /// heap, not of calls, so a deep index costs memory, not the thread's stack.
    fn directory(
        &self,
        dir: &str,
        children: &mut Vec<(Directory, KeptDirectory)>,
    ) -> Result<(Directory, KeptDirectory), IndexError> {
        let frame = |dir: String, name: String, slot: usize| Frame {
            entries: self
                .by_parent
                .get(&dir)
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .into_iter(),
            message: Directory::default(),
            kept: KeptDirectory::default(),
            dir,
            name,
            slot,
        };
        let mut stack = vec![frame(dir.to_owned(), String::new(), 0)];
        loop {
            let depth = stack.len();
            let top = stack
                .last_mut()
                .expect("the root frame stays until the end");
            let Some((name, entry)) = top.entries.next() else {
                let done = stack.pop().expect("a frame");
                let digest = directory_digest(&done.message);
                let Some(parent) = stack.last_mut() else {
                    return Ok((done.message, done.kept));
                };
                parent.message.directories.push(DirectoryNode {
                    name: done.name.clone(),
                    digest: Some(digest_to_proto(&digest)),
                });
                parent.kept.directories.push((done.name, digest));
                children[done.slot] = (done.message, done.kept);
                continue;
            };
            let name = name.clone();
            match entry {
                Entry::File { digest, exec } => {
                    top.message.files.push(FileNode {
                        name: name.clone(),
                        digest: Some(digest_to_proto(digest)),
                        is_executable: *exec,
                        ..FileNode::default()
                    });
                    top.kept.files.push((name, *digest, *exec));
                }
                Entry::Link(target) => {
                    top.message.symlinks.push(SymlinkNode {
                        name: name.clone(),
                        target: target.clone(),
                        ..SymlinkNode::default()
                    });
                    top.kept.symlinks.push((name, target.clone()));
                }
                Entry::Dir => {
                    if depth > MAX_DEPTH {
                        return Err(IndexError::TooDeep);
                    }
                    let slot = children.len();
                    children.push((Directory::default(), KeptDirectory::default()));
                    let below = format!("{}/{name}", top.dir);
                    stack.push(frame(below, name, slot));
                }
            }
        }
    }

    /// The tree of the output directory `path`.
    ///
    /// # Errors
    /// [`IndexError::NoDirectory`] when `path` is not a directory of the index; the depth bound.
    pub(crate) fn tree(&self, path: &str) -> Result<DerivedTree, IndexError> {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let entry = self.by_parent.get(parent).and_then(|d| d.get(name));
        if !matches!(entry, Some(Entry::Dir)) {
            return Err(IndexError::NoDirectory(path.to_owned()));
        }
        let mut below = Vec::new();
        let (root, root_kept) = self.directory(path, &mut below)?;
        let mut directories = BTreeMap::new();
        directories.insert(directory_digest(&root), root_kept);
        let mut children = Vec::with_capacity(below.len());
        for (message, kept) in below {
            directories.insert(directory_digest(&message), kept);
            children.push(message);
        }
        let tree = Tree {
            root: Some(root.clone()),
            children,
        };
        Ok(DerivedTree {
            path: path.to_owned(),
            digest: kbf_segments::sha256(&tree.encode_to_vec()),
            root: directory_digest(&root),
            directories,
        })
    }
}

#[cfg(test)]
#[path = "output_index_tests.rs"]
mod tests;
