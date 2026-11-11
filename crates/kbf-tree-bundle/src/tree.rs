//! A tree of files, directories and symlinks, written as tierstore tree manifests.
//!
//! A [`Dir`] holds its children by name; [`Dir::build`] writes one manifest per directory,
//! bottom-up, each `{"v":1,"children":[...]}` with its children sorted bytewise by name and
//! each child's members in one order, with no space (the bytes the frontend's own writer makes
//! for a file without a `source`, `crates/kbf-front/src/action_tree/encode.rs`):
//!
//! - a file: `{"name","kind":"file","sha256","size","exec"}`, its bytes the whole object
//!   `cas/l/<sha256>`, so it has no `source`;
//! - a directory: `{"name","kind":"dir","sha256","size"}`, its manifest's digest and length;
//! - a symlink: `{"name","kind":"symlink","target"}`.
//!
//! What tierstore's reader refuses is refused here first, by name and path: a name that is
//! empty, `.`, `..`, holds a `/` or a NUL, or begins with `.wh.` ([`TreeError::Name`]); a
//! manifest over [`MAX_MANIFEST_BYTES`]; more than [`MAX_ENTRIES`] entries below the root; an
//! entry deeper than [`MAX_DEPTH`].

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use sha2::{Digest as _, Sha256};

/// The most entries below a tree's root that tierstore takes (its `MAX_ENTRIES`).
pub const MAX_ENTRIES: u64 = (1 << 24) - 1;

/// The largest manifest tierstore takes, in bytes (its `MAX_MANIFEST_BYTES`).
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;

/// The deepest entry tierstore takes: the number of names on its path from the root (its
/// `MAX_DEPTH`).
pub const MAX_DEPTH: usize = 4096;

/// The prefix of the names tierstore keeps for whiteouts, which no entry of a tree may have.
pub const WHITEOUT_PREFIX: &str = ".wh.";

/// Why a tree cannot be written as tierstore manifests.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    /// A name tierstore refuses: empty, `.`, `..`, with a `/` or a NUL, or beginning `.wh.`.
    #[error("{path}: the name {name:?} is one a tierstore tree cannot hold")]
    Name {
        /// The directory that holds it (`/` for the root).
        path: String,
        /// The name.
        name: String,
    },
    /// A directory whose manifest is over [`MAX_MANIFEST_BYTES`].
    #[error("{path}: its manifest is {size} bytes, over tierstore's limit")]
    ManifestTooLarge {
        /// The directory (`/` for the root).
        path: String,
        /// The manifest's length.
        size: usize,
    },
    /// More entries below the root than [`MAX_ENTRIES`].
    #[error("the tree has {entries} entries, over tierstore's limit")]
    TooManyEntries {
        /// The entries it has.
        entries: u64,
    },
    /// An entry deeper than [`MAX_DEPTH`].
    #[error("{path}: deeper than tierstore's limit of levels")]
    TooDeep {
        /// The first entry found past the limit.
        path: String,
    },
}

/// An entry of a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    /// A regular file, named by its contents.
    File {
        /// The SHA-256 of its bytes.
        sha256: [u8; 32],
        /// Their length.
        size: u64,
        /// Whether it is executable (mode 0755; else 0644).
        exec: bool,
    },
    /// A directory.
    Dir(Dir),
    /// A symbolic link.
    Symlink {
        /// What it says.
        target: String,
    },
}

impl Node {
    /// The file whose bytes are `bytes`.
    #[must_use]
    pub fn file(bytes: &[u8], exec: bool) -> Self {
        Self::File {
            sha256: Sha256::digest(bytes).into(),
            size: bytes.len() as u64,
            exec,
        }
    }
}

/// A directory: its children by name, in bytewise order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dir {
    children: BTreeMap<String, Node>,
}

/// A tree written as manifests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Built {
    /// The root manifest's SHA-256, as 64 lowercase hex digits: the tree's name.
    pub root: String,
    /// Every manifest once, by its SHA-256 in hex, each after the manifests it names: the
    /// root's is last.
    pub manifests: Vec<(String, Vec<u8>)>,
    /// The entries below the root, across all its directories, as tierstore counts them.
    pub entries: u64,
    /// The deepest entry: the number of names on its path from the root (0 for an empty
    /// root).
    pub depth: usize,
}

impl Dir {
    /// An empty directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The child `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Node> {
        self.children.get(name)
    }

    /// The child `name`, to change.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Node> {
        self.children.get_mut(name)
    }

    /// Puts `node` at `name`, and gives back what was there.
    pub fn insert(&mut self, name: impl Into<String>, node: Node) -> Option<Node> {
        self.children.insert(name.into(), node)
    }

    /// Takes the child `name` out.
    pub fn remove(&mut self, name: &str) -> Option<Node> {
        self.children.remove(name)
    }

    /// Takes every child out.
    pub fn clear(&mut self) {
        self.children.clear();
    }

    /// The children, in bytewise order of their names.
    pub fn children(&self) -> impl Iterator<Item = (&str, &Node)> {
        self.children
            .iter()
            .map(|(name, node)| (name.as_str(), node))
    }

    /// The tree below this directory, as its root, written as manifests.
    ///
    /// # Errors
    /// [`TreeError`]: a name, a manifest, the entries or the depth that tierstore refuses.
    pub fn build(&self) -> Result<Built, TreeError> {
        self.build_within(Limits::TIERSTORE)
    }

    /// [`Dir::build`] within `limits`.
    fn build_within(&self, limits: Limits) -> Result<Built, TreeError> {
        let mut writer = ManifestWriter {
            limits,
            manifests: Vec::new(),
            seen: BTreeSet::new(),
            entries: 0,
            depth: 0,
        };
        let root = writer.dir(self, "", 0)?;
        if writer.entries > limits.entries {
            return Err(TreeError::TooManyEntries {
                entries: writer.entries,
            });
        }
        Ok(Built {
            root: root.0,
            manifests: writer.manifests,
            entries: writer.entries,
            depth: writer.depth,
        })
    }
}

/// tierstore's limits, which [`Dir::build`] holds a tree to. The tests hold a tree to smaller
/// ones, so that each limit is tested at its boundary without a tree of its size.
#[derive(Clone, Copy, Debug)]
struct Limits {
    entries: u64,
    depth: usize,
    manifest_bytes: usize,
}

impl Limits {
    const TIERSTORE: Self = Self {
        entries: MAX_ENTRIES,
        depth: MAX_DEPTH,
        manifest_bytes: MAX_MANIFEST_BYTES,
    };
}

/// The manifests of a tree as they are written, and what is counted on the way.
struct ManifestWriter {
    limits: Limits,
    manifests: Vec<(String, Vec<u8>)>,
    seen: BTreeSet<String>,
    entries: u64,
    depth: usize,
}

/// A JSON string of `text`.
fn quoted(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

/// Whether tierstore takes `name` as the name of an entry.
fn takes(name: &str) -> bool {
    !(name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\0'])
        || name.starts_with(WHITEOUT_PREFIX))
}

impl ManifestWriter {
    /// Writes the manifest of `dir` at `path` (`""` for the root), `depth` names below the
    /// root, after those of its subdirectories: its digest in hex and its length.
    fn dir(&mut self, dir: &Dir, path: &str, depth: usize) -> Result<(String, usize), TreeError> {
        let shown = if path.is_empty() { "/" } else { path };
        let mut listed = Vec::with_capacity(dir.children.len());
        for (name, node) in &dir.children {
            if !takes(name) {
                return Err(TreeError::Name {
                    path: shown.to_owned(),
                    name: name.clone(),
                });
            }
            let at = format!("{path}/{name}");
            if depth + 1 > self.limits.depth {
                return Err(TreeError::TooDeep { path: at });
            }
            self.depth = self.depth.max(depth + 1);
            self.entries = self.entries.saturating_add(1);
            let name = quoted(name);
            listed.push(match node {
                Node::File { sha256, size, exec } => format!(
                    r#"{{"name":{name},"kind":"file","sha256":"{}","size":{size},"exec":{exec}}}"#,
                    hex::encode(sha256)
                ),
                Node::Dir(child) => {
                    let (sha256, size) = self.dir(child, &at, depth + 1)?;
                    format!(r#"{{"name":{name},"kind":"dir","sha256":"{sha256}","size":{size}}}"#)
                }
                Node::Symlink { target } => format!(
                    r#"{{"name":{name},"kind":"symlink","target":{}}}"#,
                    quoted(target)
                ),
            });
        }
        let bytes = format!(r#"{{"v":1,"children":[{}]}}"#, listed.join(",")).into_bytes();
        if bytes.len() > self.limits.manifest_bytes {
            return Err(TreeError::ManifestTooLarge {
                path: shown.to_owned(),
                size: bytes.len(),
            });
        }
        let sha256 = hex::encode(Sha256::digest(&bytes));
        let size = bytes.len();
        if self.seen.insert(sha256.clone()) {
            self.manifests.push((sha256.clone(), bytes));
        }
        Ok((sha256, size))
    }
}

#[cfg(test)]
mod tests;
