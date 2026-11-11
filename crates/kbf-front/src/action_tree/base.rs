//! [`BaseTree`]: the kbf base tree an action tree's root is written over, given by its root
//! manifest and its facts.

use tierstore::{TreeDigest, TreeError, parse_manifest};

use super::encode::{self, Child};

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
}

/// The kbf base tree: the children of its root manifest, each copied as it is into every action
/// tree's root, and its facts. Its subtrees are never read: the root names them by digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseTree {
    pub(super) children: Vec<Child>,
    pub(super) facts: BaseFacts,
}

impl BaseTree {
    /// The base tree whose root manifest is `root` (its bytes as stored), with `facts`.
    ///
    /// # Errors
    /// [`BaseTreeError`]: the manifest is not one tierstore reads, or holds a reserved name.
    pub fn new(root: &[u8], facts: BaseFacts) -> Result<Self, BaseTreeError> {
        let manifest =
            parse_manifest(root, &TreeDigest::of(root)).map_err(BaseTreeError::Manifest)?;
        let mut children = Vec::with_capacity(manifest.children.len());
        for child in &manifest.children {
            if RESERVED_NAMES.contains(&&*child.name) {
                return Err(BaseTreeError::Reserved(child.name.to_string()));
            }
            children.push(encode::copied(child));
        }
        Ok(Self { children, facts })
    }
}
