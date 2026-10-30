//! The keys of one run: [`IssuedKeySet`].

use super::IssuedPackKey;

/// The keys the frontend issued for one run of one lease and one action: a key for each pack the
/// task may write (slots `0..`, pack 0 first) and a key for each large object it may write (slots
/// `L0..`). What [`super::ingest_set`] reads is exactly what these keys name; a key the set does not
/// hold is never read.
///
/// The set is the frontend's own, made from what [`super::PackKeyMint::mint_set`] returned for the
/// lease row: never from the task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedKeySet {
    packs: Vec<IssuedPackKey>,
    large: Vec<IssuedPackKey>,
}

impl IssuedKeySet {
    /// The set of `packs`, in the order of their slots, and `large`, in the order of theirs. What
    /// the set holds is checked by [`super::ingest_set`], which refuses a set that is not one a mint
    /// made (a key out of place, of another lease, or more keys than a run has slots).
    #[must_use]
    pub const fn new(packs: Vec<IssuedPackKey>, large: Vec<IssuedPackKey>) -> Self {
        Self { packs, large }
    }

    /// The set of a run of one pack and no large object: what a task that was given one key
    /// writes.
    #[must_use]
    pub fn single(key: IssuedPackKey) -> Self {
        Self::new(vec![key], Vec::new())
    }

    /// The set of the keys [`super::PackKeyMint::mint_set`] returned, of which the first `packs`
    /// are the pack keys and the rest the large ones. A `packs` over the number of keys takes
    /// them all as pack keys.
    #[must_use]
    pub fn from_minted(mut keys: Vec<IssuedPackKey>, packs: usize) -> Self {
        let large = keys.split_off(packs.min(keys.len()));
        Self::new(keys, large)
    }

    /// The pack keys, by slot.
    #[must_use]
    pub fn packs(&self) -> &[IssuedPackKey] {
        &self.packs
    }

    /// The large-object keys, by slot.
    #[must_use]
    pub fn large(&self) -> &[IssuedPackKey] {
        &self.large
    }
}
