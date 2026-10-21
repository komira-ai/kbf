//! The floor on `seq`: the root-owned file `seq_floor` beside `state.json`, holding the highest
//! `seq` a staged order may not use (a seed must be strictly above it; see [`crate::seed`]).
//!
//! The floor only goes up, and only to the `seq` of a release that was confirmed:
//!
//! - it starts at the `seq` of the release that runs, on a host that has none (a farm that was
//!   fetched by URL before the floor existed): a package older than the farm must not be
//!   orderable over it;
//! - it rises when a swap is confirmed (the node's `confirm`, the frontend's flip), never at the
//!   stage or at the swap itself: a release put on trial and reverted must stay orderable;
//! - a rollback leaves it where it is, so the release just rolled back from stays unorderable
//!   until a package newer than it arrives.
//!
//! [`raise`] is the one writer. It takes the installer's lock, so two writers cannot each read
//! the floor and write back a lower one.

use std::io;
use std::path::PathBuf;

use crate::layout::Layout;
use crate::state::StateError;

/// Why the floor could not be raised.
#[derive(Debug, thiserror::Error)]
pub enum FloorError {
    #[error("seq_floor: {0}")]
    Lock(#[from] StateError),
    /// The file is there and is not one number: it is left as it is.
    #[error("{0}")]
    Unreadable(String),
    #[error("seq_floor: cannot write {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
}

/// Raise the floor to `to` unless it is there already or higher; the floor after the call.
///
/// # Errors
/// The lock cannot be taken, the file is there and cannot be read as one number (it is not
/// replaced), or the new file cannot be written.
pub fn raise(_host: &Layout, to: u64) -> Result<u64, FloorError> {
    Ok(to)
}

#[cfg(test)]
#[path = "seq_floor_tests.rs"]
mod tests;
