//! The L2 sidecar of a tree list (tierstore's `docs/design/fetch-cache-serve.md`, 3.5): dual
//! emission, the miss path and the backfill.
//!
//! The tree's id does not change: the version 2 list stays at `cas/l/<id>`, `<id>` the SHA-256 of
//! its bytes, and the L2 list is an additional object, the sidecar, at `cas/l2/<id>`
//! ([`tierstore_list::sidecar_key`]), made by [`tierstore_list::sidecar_from_v2`] from the
//! version 2 list and the cells of every object it names. Old readers read the version 2 list and
//! never the sidecar; new readers read only the sidecar.
//!
//! - [`Sidecars::emit`] is dual emission: it writes the sidecar from the cells the pack index
//!   holds, and reads no object. A list naming an object with no recorded cells gets no sidecar:
//!   its tree is recorded unlisted ([`kbf_pack_index::PackIndex::record_unlisted`]) and counted
//!   (`kbf_l2_sidecar_missing_total`), for the backfill.
//! - [`Sidecars::backfill`] is the miss path and the backfill: it reads each object the list names
//!   whose cells are not recorded, once, in [`READ_WINDOW`] pieces fed to a
//!   [`tierstore_list::CellHasher`] as they arrive, records the cells, and then writes the
//!   sidecar. An object the store answers 404 for is [`SidecarError::Lost`]: the tree is not
//!   present.
//!
//! The sidecar is written create-only; one already written is kept (its `shared` bit may be
//! stale, which costs fetch order and never a byte, design 3.5).

use std::sync::Arc;

use kbf_objstore::{ObjectKey, ObjectStoreError};
use kbf_pack_index::PackIndex;
use kbf_pack_index::cells::ObjectCells;

use crate::large_backfill::LargeStore;

/// The bytes one read of [`Sidecars::backfill`] asks for: 8 cells.
pub const READ_WINDOW: u64 = 8 << 20;

/// What the L2 sidecars of tree lists are written with: the pack index's cells and unlisted
/// trees, and the store the lists are in.
pub struct Sidecars {
    index: Arc<dyn PackIndex>,
    store: Arc<dyn LargeStore>,
}

/// What [`Sidecars::emit`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emitted {
    /// The sidecar is in the store: written now, or found written.
    Written,
    /// `missing` objects the list names have no recorded cells: no sidecar was written and the
    /// tree is recorded unlisted.
    Unlisted {
        /// How many.
        missing: usize,
    },
}

/// What [`Sidecars::backfill`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Backfilled {
    /// The objects read for their cells.
    pub objects_read: usize,
    /// The bytes read for them: each object's size, once.
    pub bytes_read: u64,
}

/// Why no sidecar was written.
#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    /// The store answered 404 for an object the list names: the tree is not present.
    #[error("the store has no object {key} the tree list names")]
    Lost {
        /// The object's key.
        key: String,
    },
    /// The bytes are not a version 2 list the writer takes, or the cells are not those of an
    /// object it names.
    #[error("no L2 sidecar of the tree list: {0}")]
    List(#[from] tierstore_list::WriteError),
    /// The pack index failed.
    #[error("the pack index failed: {0}")]
    Index(#[from] kbf_pack_index::PackIndexError),
    /// The store failed.
    #[error("the store failed: {0}")]
    Store(#[from] ObjectStoreError),
}

impl Sidecars {
    /// Sidecars written to `store` (the frontend's packs) from the cells of `index`.
    #[must_use]
    pub fn new(index: Arc<dyn PackIndex>, store: Arc<dyn LargeStore>) -> Self {
        Self { index, store }
    }

    /// Dual emission of the version 2 list `v2` (read back, as the writer stores it): writes its
    /// sidecar from recorded cells only.
    ///
    /// # Errors
    /// [`SidecarError`]; nothing that fails here fails the list.
    pub async fn emit(&self, v2: &[u8]) -> Result<Emitted, SidecarError> {
        let _ = (v2, &self.index, &self.store);
        Err(SidecarError::Store(ObjectStoreError::Unsupported(
            "not yet",
        )))
    }

    /// The miss path and the backfill of the version 2 list `v2`: reads once each object it
    /// names that has no recorded cells, records them, writes the sidecar, and drops the tree's
    /// unlisted row.
    ///
    /// # Errors
    /// [`SidecarError::Lost`] for an object the store does not have; otherwise
    /// [`SidecarError`].
    pub async fn backfill(&self, v2: &[u8]) -> Result<Backfilled, SidecarError> {
        let _ = v2;
        Err(SidecarError::Store(ObjectStoreError::Unsupported(
            "not yet",
        )))
    }

    /// Records the cells of the object `key` whose bytes were just written or checked, as the
    /// writer hashed them: best effort, a failure is logged and the backfill reads it later.
    pub async fn record(&self, key: &ObjectKey, cells: ObjectCells) {
        let _ = (key, cells);
    }
}
