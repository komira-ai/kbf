//! The values a node answers that the caller keeps: the node's digest of a tree, the id of a
//! placement, and the answer of a create that carries both.

use tierstore_trees_api::v1::CreateTreeResponse;

use crate::error::Undecodable;

/// The digest a node computed of a tree (32 bytes). kbf computes none of its own: a node of
/// any revision of tierstore's digest builder names its trees by its own, and a child entry
/// names a tree by the digest that node returned.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TreeDigest([u8; 32]);

impl TreeDigest {
    /// The digest of these 32 bytes; `None` for any other length.
    #[must_use]
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        bytes.try_into().ok().map(Self)
    }

    /// The digest of these bytes.
    #[must_use]
    pub fn from_array(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The bytes, as a child tree entry carries them.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Prints the digest in hex.
impl std::fmt::Display for TreeDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

/// Prints `TreeDigest(<hex>)`.
impl std::fmt::Debug for TreeDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TreeDigest({self})")
    }
}

/// The id of a placement, as the node answered it: the name a read or a delete of it takes.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PlacementId(String);

impl PlacementId {
    /// The id `text`, as a node answered it; `None` for an empty one, which names nothing.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Option<Self> {
        let text = text.into();
        (!text.is_empty()).then_some(Self(text))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PlacementId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The answer of a create: the node's digest of the tree, its placement, and the whole
/// answer (counts, state, lease) as the node gave it.
#[derive(Clone, Debug, PartialEq)]
pub struct Created {
    tree: TreeDigest,
    placement: PlacementId,
    answer: CreateTreeResponse,
}

impl Created {
    /// The answer, checked: its digest is 32 bytes and it names a placement.
    pub(crate) fn of(answer: CreateTreeResponse) -> Result<Self, Undecodable> {
        let tree =
            TreeDigest::from_slice(&answer.tree).ok_or(Undecodable::Digest(answer.tree.len()))?;
        let placement =
            PlacementId::new(answer.placement.clone()).ok_or(Undecodable::NoPlacement)?;
        Ok(Self {
            tree,
            placement,
            answer,
        })
    }

    /// The node's digest of the tree.
    #[must_use]
    pub fn tree(&self) -> TreeDigest {
        self.tree
    }

    /// The placement the create made, or found already made.
    #[must_use]
    pub fn placement(&self) -> &PlacementId {
        &self.placement
    }

    /// Whether this call made the placement; `false` when the same tree was already placed
    /// at the same locations, which a repeated create answers.
    #[must_use]
    pub fn is_new(&self) -> bool {
        self.answer.created
    }

    /// The whole answer.
    #[must_use]
    pub fn answer(&self) -> &CreateTreeResponse {
        &self.answer
    }
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
