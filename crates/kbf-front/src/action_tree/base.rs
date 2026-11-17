//! [`BaseTree`]: the kbf base tree an action tree's root is written over, given by its root
//! manifest and its facts, and listed entry by entry for a tree list ([`BaseTree::listed`]).

use std::fmt;
use std::sync::Arc;

use tierstore::{TreeDigest, TreeError, flatten, parse_manifest};

use super::encode::{self, Child};
use super::listing::{Entry, entries_of};

/// The top-level names every action tree adds or `kbf-task` makes, which a base tree must not
/// hold: the input root, the Action and Command, and `kbf-task`'s run directory.
pub const RESERVED_NAMES: [&str; 3] = ["work", ".kbf", ".kbf-run"];

/// What the kbf base tree's bundle says of the whole base tree, which its root manifest alone
/// does not: the action tree's limits are counted with them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BaseFacts {
    /// The entries below the base tree's root, across all its directories, as tierstore counts
    /// them (a directory named twice counted twice).
    pub entries: u64,
    /// The deepest entry of the base tree: the number of names on its path from the root.
    pub depth: usize,
}

/// Why a [`BaseTree`] cannot be made.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum BaseTreeError {
    /// The root manifest is not one tierstore's reader takes.
    #[error("the base tree's root manifest: {0}")]
    Manifest(#[source] TreeError),
    /// The root manifest holds a name of [`RESERVED_NAMES`].
    #[error("the base tree's root holds {0:?}, a name the action tree keeps for itself")]
    Reserved(String),
    /// The base tree could not be listed: a manifest below the root is missing or is not one
    /// tierstore's reader takes, or the tree is over its limits.
    #[error("the base tree could not be listed: {0}")]
    Listing(#[source] TreeError),
}

/// The kbf base tree: the children of its root manifest, each copied as it is into every action
/// tree's root, and its facts. Written as manifests, its subtrees are never read: the root names
/// them by digest. Listed ([`BaseTree::listed`]), it holds every entry below its root too, which
/// a tree list names one by one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseTree {
    pub(super) children: Vec<Child>,
    pub(super) facts: BaseFacts,
    root: TreeDigest,
    pub(super) listing: Option<Arc<[Entry]>>,
}

impl BaseTree {
    /// The base tree whose root manifest is `root` (its bytes as stored), with `facts`.
    ///
    /// # Errors
    /// [`BaseTreeError`]: the manifest is not one tierstore reads, or holds a reserved name.
    pub fn new(root: &[u8], facts: BaseFacts) -> Result<Self, BaseTreeError> {
        let digest = TreeDigest::of(root);
        let manifest = parse_manifest(root, &digest).map_err(BaseTreeError::Manifest)?;
        let mut children = Vec::with_capacity(manifest.children.len());
        for child in &manifest.children {
            if RESERVED_NAMES.contains(&&*child.name) {
                return Err(BaseTreeError::Reserved(child.name.to_string()));
            }
            children.push(encode::copied(child));
        }
        Ok(Self {
            children,
            facts,
            root: digest,
            listing: None,
        })
    }

    /// This base tree with every entry below its root listed, the manifests below the root read
    /// by `fetch` (each by its digest).
    ///
    /// # Errors
    /// [`BaseTreeError::Listing`]: `fetch` failed for a manifest, a manifest is not one
    /// tierstore's reader takes, or the tree is over tierstore's limits.
    pub fn listed<E: fmt::Display>(
        self,
        fetch: impl FnMut(&TreeDigest) -> Result<Vec<u8>, E>,
    ) -> Result<Self, BaseTreeError> {
        let flattened = flatten(&self.root, fetch).map_err(BaseTreeError::Listing)?;
        Ok(Self {
            listing: Some(entries_of(&flattened).into()),
            ..self
        })
    }

    /// Whether every entry of the tree is listed: what a tree list needs.
    #[must_use]
    pub fn is_listed(&self) -> bool {
        self.listing.is_some()
    }
}
