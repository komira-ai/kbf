//! Repair: the debt a chain write leaves when the replica could not take its copy
//! (`docs/design/architecture-v2.md`, section 8: with the replica down, the primary
//! acknowledges after its own durable write and records `repair/<digest>`).
//!
//! A [`RepairRecord`] says "this node holds a blob that a second owner does not". The
//! record is the whole difference between a one-copy write that is acknowledged and one
//! that is a silent loss of redundancy, so it is kept in the [`Journal`]: a file on the
//! node's own disk, made durable before the write is acknowledged and kept until the
//! second copy exists.

mod journal;
#[cfg(test)]
mod tests;

pub use journal::{JOURNAL, JOURNAL_TMP, Journal, JournalError, Options};
use kbf_map::MemberId;
use kbf_types::Digest;

/// A blob this node holds that another owner does not yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepairRecord {
    /// The blob.
    pub digest: Digest,
    /// The owner that missed it. Advisory: whoever the owners are when the repair runs
    /// is who gets the copy, because the map may have turned since.
    pub replica: MemberId,
    /// The map generation the write was acknowledged under.
    pub generation: u64,
}
