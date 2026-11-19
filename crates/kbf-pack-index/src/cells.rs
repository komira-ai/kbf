//! Cell rows and unlisted trees: what the L2 sidecar of a tree list is made from
//! (tierstore's `docs/design/fetch-cache-serve.md`, 3.2 and 3.5).
//!
//! The L2 sidecar of a tree list (`cas/l2/<id>`) holds, for every object the list names, its
//! size and the SHA-256 of each 1 MiB cell of its bytes. The list's writer never reads an
//! object, so the cells are recorded where the bytes pass anyway (the packer, the write of a
//! whole object) and, for an object written before, by the one read of the miss path or of
//! `kbf-l2-backfill`. A tree list whose sidecar could not be written because an object it names
//! has no cells yet is recorded as **unlisted**, so the backfill finds it.
//!
//! `PostgresPackIndex` keeps the rows in the `object_cells` and `unlisted_tree` tables of
//! migration 5 ([`crate::schema`]); `MemoryPackIndex` in maps. The rules, which
//! `cells_conformance` states for both:
//!
//! - **A cell row never changes.** An object key names one sequence of bytes (the keys are
//!   content-derived or written once), so its cells are recorded once: the same cells recorded
//!   again change nothing ([`PackIndex::record_cells`] answers `false`), and other cells under
//!   a recorded key are [`PackIndexError::Conflict`] and change nothing, so the index never
//!   holds two answers for one key.
//! - **A cell row is whole.** Its size is at least one byte and at most what a `bigint` holds,
//!   and it has exactly `ceil(size / 1 MiB)` digests ([`ObjectCells::checked`]); anything else
//!   is [`PackIndexError::Invalid`] and nothing is written.
//! - **An unlisted tree is a work item.** [`PackIndex::record_unlisted`] inserts the tree with
//!   the index's clock as its first and last time, or moves the last time of a tree already
//!   recorded; [`PackIndex::unlisted`] lists those last recorded within a window, by tree id,
//!   after a cursor; [`PackIndex::drop_unlisted`] removes one once its sidecar is written.
//! - **Reads are queries.** [`PackIndex::object_cells`], [`PackIndex::unlisted`] and
//!   [`PackIndex::live_packs`] write nothing. Batches of keys are bounded by
//!   [`crate::MAX_BATCH`], pages by [`MAX_PAGE`].
//!
//! [`PackIndex::record_cells`]: crate::PackIndex::record_cells
//! [`PackIndex::object_cells`]: crate::PackIndex::object_cells
//! [`PackIndex::record_unlisted`]: crate::PackIndex::record_unlisted
//! [`PackIndex::unlisted`]: crate::PackIndex::unlisted
//! [`PackIndex::drop_unlisted`]: crate::PackIndex::drop_unlisted
//! [`PackIndex::live_packs`]: crate::PackIndex::live_packs
//! [`PackIndexError::Conflict`]: crate::PackIndexError::Conflict
//! [`PackIndexError::Invalid`]: crate::PackIndexError::Invalid

/// The bytes of one cell: 1 MiB, the cell of every pack the L2 list names (`cell_shift = 20`).
pub const CELL_BYTES: u64 = 1 << 20;

/// The most rows one page of [`crate::PackIndex::unlisted`] or [`crate::PackIndex::live_packs`]
/// returns.
pub const MAX_PAGE: usize = 1000;

/// The cells of an object: its size and the SHA-256 of each [`CELL_BYTES`] of its bytes, the
/// last one short.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObjectCells {
    /// The object's size in bytes.
    pub size: u64,
    /// `digests[i]` is the SHA-256 of the bytes `[i * CELL_BYTES, min((i + 1) * CELL_BYTES, size))`.
    pub digests: Vec<[u8; 32]>,
}

impl ObjectCells {
    /// The number of cells of an object of `size` bytes.
    #[must_use]
    pub const fn cells_of(size: u64) -> u64 {
        size.div_ceil(CELL_BYTES)
    }

    /// `Ok` when the row can exist: a size of at least one byte that a `bigint` holds, and one
    /// digest per cell.
    ///
    /// # Errors
    /// [`crate::PackIndexError::Invalid`], saying which.
    pub fn checked(&self) -> Result<(), crate::PackIndexError> {
        if self.size == 0 {
            return Err(crate::PackIndexError::Invalid(
                "an object of no bytes has no cells",
            ));
        }
        if i64::try_from(self.size).is_err() {
            return Err(crate::PackIndexError::Invalid(
                "the object is longer than the index can hold",
            ));
        }
        if self.digests.len() as u64 != Self::cells_of(self.size) {
            return Err(crate::PackIndexError::Invalid(
                "the cells are not those of the object's size",
            ));
        }
        Ok(())
    }
}

/// A tree list whose L2 sidecar is not written yet, by the tree's id (the SHA-256 of its
/// version 2 list, the digest of the key `cas/l/<id>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unlisted {
    /// The tree's id.
    pub tree: [u8; 32],
    /// When it was first recorded, by the index's clock, in microseconds.
    pub first_us: i64,
    /// When it was last recorded, by the index's clock, in microseconds.
    pub last_us: i64,
}

/// A pack some blob row points at, as a sweep of the index sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LivePack {
    /// The pack's key.
    pub key: String,
    /// Its size in bytes.
    pub size: u64,
    /// Whether its cells are recorded.
    pub has_cells: bool,
}
