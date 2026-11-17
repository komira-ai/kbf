//! The Directory rows of the in-memory index: a map beside the pack maps, changed under the
//! same lock, so a record is atomic as the Postgres transaction is.

use super::State;
use crate::directory::NewDirectory;
use crate::{BlobDigest, PackIndexError, check_batch};

/// [`crate::PackIndex::directories`] on the index's state.
pub(super) fn read(
    state: &State,
    digests: &[BlobDigest],
) -> Result<Vec<Option<Vec<u8>>>, PackIndexError> {
    check_batch(digests.len())?;
    Ok(digests
        .iter()
        .map(|digest| state.directories.get(digest).cloned())
        .collect())
}

/// [`crate::PackIndex::record_directories`] on the index's state, for Directories that
/// [`crate::directory::checked`] passed: inserts the ones that have no row and counts them.
pub(super) fn record(state: &mut State, checked: &[&NewDirectory]) -> usize {
    let mut inserted = 0;
    for directory in checked {
        if let std::collections::btree_map::Entry::Vacant(slot) =
            state.directories.entry(directory.digest)
        {
            slot.insert(directory.body.clone());
            inserted += 1;
        }
    }
    inserted
}
