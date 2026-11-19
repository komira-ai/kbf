//! The cell rows and unlisted trees of the in-memory index: maps beside the pack maps, changed
//! under the same lock.

use std::collections::btree_map::Entry;
use std::time::Duration;

use super::State;
use crate::cells::{LivePack, ObjectCells, Unlisted};
use crate::{PackIndexError, check_batch, check_page, micros};

/// [`crate::PackIndex::object_cells`] on the index's state.
pub(super) fn read(
    state: &State,
    keys: &[String],
) -> Result<Vec<Option<ObjectCells>>, PackIndexError> {
    check_batch(keys.len())?;
    Ok(keys
        .iter()
        .map(|key| state.cells.get(key).cloned())
        .collect())
}

/// [`crate::PackIndex::record_cells`] on the index's state.
pub(super) fn record(
    state: &mut State,
    key: &str,
    cells: &ObjectCells,
) -> Result<bool, PackIndexError> {
    if key.is_empty() {
        return Err(PackIndexError::Invalid("the object key is empty"));
    }
    cells.checked()?;
    match state.cells.entry(key.to_owned()) {
        Entry::Occupied(held) if held.get() == cells => Ok(false),
        Entry::Occupied(_) => Err(PackIndexError::Conflict {
            key: key.to_owned(),
        }),
        Entry::Vacant(slot) => {
            slot.insert(cells.clone());
            Ok(true)
        }
    }
}

/// [`crate::PackIndex::record_unlisted`] on the index's state, at `now`.
pub(super) fn record_unlisted(state: &mut State, tree: &[u8; 32], now: i64) {
    state
        .unlisted
        .entry(*tree)
        .and_modify(|(_, last)| *last = now)
        .or_insert((now, now));
}

/// [`crate::PackIndex::unlisted`] on the index's state, at `now`.
pub(super) fn unlisted(
    state: &State,
    (within, now): (Duration, i64),
    after: Option<&[u8; 32]>,
    limit: usize,
) -> Result<Vec<Unlisted>, PackIndexError> {
    check_page(limit)?;
    let since = now.saturating_sub(micros(within));
    Ok(state
        .unlisted
        .iter()
        .filter(|(tree, _)| after.is_none_or(|after| *tree > after))
        .filter(|(_, (_, last))| *last >= since)
        .take(limit)
        .map(|(tree, (first, last))| Unlisted {
            tree: *tree,
            first_us: *first,
            last_us: *last,
        })
        .collect())
}

/// [`crate::PackIndex::live_packs`] on the index's state.
pub(super) fn live_packs(
    state: &State,
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<LivePack>, PackIndexError> {
    check_page(limit)?;
    Ok(state
        .packs
        .iter()
        .filter(|(key, _)| after.is_none_or(|after| key.as_str() > after))
        .filter(|(_, row)| row.info.live_blobs > 0)
        .take(limit)
        .map(|(key, row)| LivePack {
            key: key.clone(),
            size: row.info.size,
            has_cells: state.cells.contains_key(key),
        })
        .collect())
}
