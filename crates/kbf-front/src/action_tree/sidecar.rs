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
//!   [`tierstore_list::CellHasher`] as they arrive, records the cells, writes the sidecar, and
//!   drops the tree's unlisted row. An object the store answers 404 for is
//!   [`SidecarError::Lost`]: the tree is not present.
//!
//! Every pack is written with one PUT (`upload_part_size` 0) and no pack is marked shared (the
//! `shared` bit is set by a later change). The sidecar is written create-only; one already
//! written is kept (its `shared` bit may be stale, which costs fetch order and never a byte,
//! design 3.5). Nothing here fails the list it is the sidecar of.

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use kbf_objstore::{ObjectKey, ObjectStoreError};
use kbf_pack_index::cells::ObjectCells;
use kbf_pack_index::{MAX_BATCH, PackIndex, PackIndexError};
use tierstore_list::{CellHasher, PackCells, PackInfo};

use super::place::store_object;
use super::retry::PutRetry;
use crate::large_backfill::LargeStore;
use crate::metrics::{Counter, Families};

/// The bytes one read of [`Sidecars::backfill`] asks for: 8 cells.
pub const READ_WINDOW: u64 = 8 << 20;

/// What the L2 sidecars of tree lists are written with: the pack index's cells and unlisted
/// trees, and the store the lists are in.
pub struct Sidecars {
    index: Arc<dyn PackIndex>,
    store: Arc<dyn LargeStore>,
    retry: PutRetry,
    counters: Arc<SidecarCounters>,
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
    /// The bytes are not a version 2 list tierstore's reader opens.
    #[error("not a tree list: {0}")]
    NotAList(String),
    /// The list is not one the writer takes, or the cells are not those of an object it names.
    #[error("no L2 sidecar of the tree list: {0}")]
    List(#[from] tierstore_list::WriteError),
    /// The pack index failed.
    #[error("the pack index failed: {0}")]
    Index(#[from] PackIndexError),
    /// The store failed, or answered a read with other bytes than were asked for.
    #[error("the store failed: {0}")]
    Store(#[from] ObjectStoreError),
}

/// The counters of the sidecars: `kbf_l2_...`.
#[derive(Debug, Default)]
pub struct SidecarCounters {
    written: Counter,
    present: Counter,
    failed: Counter,
    missing: Counter,
    objects_read: Counter,
    bytes_read: Counter,
}

impl SidecarCounters {
    /// Appends the families to `out`.
    pub fn render(&self, out: &mut String) {
        use std::fmt::Write as _;
        let mut families = Families(std::mem::take(out));
        let name = "kbf_l2_sidecars_total";
        families.head(
            name,
            "counter",
            "L2 sidecars of tree lists, by how their write ended: written, already present, failed",
        );
        for (result, counter) in [
            ("written", &self.written),
            ("present", &self.present),
            ("failed", &self.failed),
        ] {
            let _ = writeln!(families.0, "{name}{{result=\"{result}\"}} {}", counter.get());
        }
        families.counter(
            "kbf_l2_sidecar_missing_total",
            "Tree lists written with no L2 sidecar, because an object they name has no recorded \
             cells: each is left to the backfill",
            self.missing.get(),
        );
        families.counter(
            "kbf_l2_cells_read_objects_total",
            "Objects read once for their cell digests, by the miss path or the backfill",
            self.objects_read.get(),
        );
        families.counter(
            "kbf_l2_cells_read_bytes_total",
            "Bytes read for those cell digests",
            self.bytes_read.get(),
        );
        *out = families.0;
    }
}

impl Sidecars {
    /// Sidecars written to `store` (the frontend's packs) from the cells of `index`.
    #[must_use]
    pub fn new(index: Arc<dyn PackIndex>, store: Arc<dyn LargeStore>) -> Self {
        Self {
            index,
            store,
            retry: PutRetry::default(),
            counters: Arc::default(),
        }
    }

    /// These sidecars, counting in `counters` (shared with whatever renders them).
    #[must_use]
    pub fn with_counters(mut self, counters: Arc<SidecarCounters>) -> Self {
        self.counters = counters;
        self
    }

    /// The counters these sidecars count in.
    #[must_use]
    pub fn counters(&self) -> &Arc<SidecarCounters> {
        &self.counters
    }

    /// Dual emission of the version 2 list `v2` (read back, as the writer stores it): writes its
    /// sidecar from recorded cells only.
    ///
    /// # Errors
    /// [`SidecarError`]; nothing that fails here fails the list.
    pub async fn emit(&self, v2: &[u8]) -> Result<Emitted, SidecarError> {
        let emitted = self.emit_counted(v2).await;
        match &emitted {
            Ok(Emitted::Unlisted { .. }) => self.counters.missing.add(1),
            Ok(Emitted::Written) => {}
            Err(_) => self.counters.failed.add(1),
        }
        emitted
    }

    async fn emit_counted(&self, v2: &[u8]) -> Result<Emitted, SidecarError> {
        let (tree, keys) = list_of(v2)?;
        let sidecar = key_of(&tree)?;
        if self.store.len(&sidecar).await?.is_some() {
            self.counters.present.add(1);
            return Ok(Emitted::Written);
        }
        let cells = self.cells(&keys).await?;
        let missing = keys.len() - cells.len();
        if missing > 0 {
            let index = Arc::clone(&self.index);
            blocking(move || index.record_unlisted(&tree)).await?;
            return Ok(Emitted::Unlisted { missing });
        }
        self.write(v2, &sidecar, &cells).await?;
        Ok(Emitted::Written)
    }

    /// The miss path and the backfill of the version 2 list `v2`: reads once each object it
    /// names that has no recorded cells, records them, writes the sidecar, and drops the tree's
    /// unlisted row.
    ///
    /// # Errors
    /// [`SidecarError::Lost`] for an object the store does not have; otherwise
    /// [`SidecarError`].
    pub async fn backfill(&self, v2: &[u8]) -> Result<Backfilled, SidecarError> {
        let filled = self.backfill_counted(v2).await;
        if filled.is_err() {
            self.counters.failed.add(1);
        }
        filled
    }

    async fn backfill_counted(&self, v2: &[u8]) -> Result<Backfilled, SidecarError> {
        let (tree, keys) = list_of(v2)?;
        let mut cells = self.cells(&keys).await?;
        let mut done = Backfilled::default();
        for key in &keys {
            if cells.contains_key(key) {
                continue;
            }
            let object = object_key(key)?;
            let read = read_cells(&*self.store, &object).await?;
            done.objects_read += 1;
            done.bytes_read += read.size;
            self.counters.objects_read.add(1);
            self.counters.bytes_read.add(read.size);
            let (index, owned, row) = (Arc::clone(&self.index), key.clone(), read.clone());
            blocking(move || index.record_cells(&owned, &row)).await?;
            cells.insert(key.clone(), read);
        }
        let sidecar = key_of(&tree)?;
        self.write(v2, &sidecar, &cells).await?;
        let index = Arc::clone(&self.index);
        blocking(move || index.drop_unlisted(&tree)).await?;
        Ok(done)
    }

    /// Records the cells of the object `key` whose bytes were just written or checked, as the
    /// writer hashed them: best effort, a failure is logged and the backfill reads it later.
    pub async fn record(&self, key: &ObjectKey, cells: ObjectCells) {
        let (index, owned) = (Arc::clone(&self.index), key.to_string());
        if let Err(error) = blocking(move || index.record_cells(&owned, &cells)).await {
            tracing::warn!(%key, %error, "the cells of an object written were not recorded: the backfill reads it");
        }
    }

    /// The recorded cells of each of `keys` that has some.
    async fn cells(&self, keys: &[String]) -> Result<BTreeMap<String, ObjectCells>, SidecarError> {
        let mut found = BTreeMap::new();
        for chunk in keys.chunks(MAX_BATCH) {
            let (index, batch) = (Arc::clone(&self.index), chunk.to_vec());
            let rows = blocking(move || index.object_cells(&batch)).await?;
            for (key, row) in chunk.iter().zip(rows) {
                if let Some(row) = row {
                    found.insert(key.clone(), row);
                }
            }
        }
        Ok(found)
    }

    /// Makes the sidecar of `v2` from `cells` (one for every key it names) and writes it to
    /// `sidecar`, create-only.
    async fn write(
        &self,
        v2: &[u8],
        sidecar: &ObjectKey,
        cells: &BTreeMap<String, ObjectCells>,
    ) -> Result<(), SidecarError> {
        let bytes = tierstore_list::sidecar_from_v2(v2, |key| {
            cells.get(key).map(|row| PackInfo {
                cells: PackCells {
                    size: row.size,
                    digests: row.digests.clone(),
                },
                upload_part_size: 0,
                shared: false,
            })
        })?;
        store_object(&*self.store, &self.retry, sidecar, Bytes::from(bytes)).await?;
        self.counters.written.add(1);
        Ok(())
    }
}

/// The tree id of `v2` (the SHA-256 of its bytes) and the keys it names, sorted and each once.
fn list_of(v2: &[u8]) -> Result<([u8; 32], Vec<String>), SidecarError> {
    let list =
        tierstore::TreeListV2::open(v2).map_err(|error| SidecarError::NotAList(error.to_string()))?;
    let keys = list.keys().iter().map(|key| key.to_string()).collect();
    Ok((kbf_segments::sha256(v2).hash, keys))
}

/// The key of the sidecar of the tree `tree`: `cas/l2/` and 64 hex digits, always a key.
fn key_of(tree: &[u8; 32]) -> Result<ObjectKey, SidecarError> {
    object_key(&tierstore_list::sidecar_key(tree))
}

/// `key` as an object key.
fn object_key(key: &str) -> Result<ObjectKey, SidecarError> {
    ObjectKey::new(key.to_owned()).map_err(|error| SidecarError::NotAList(error.to_string()))
}

/// The cells of the object `key`, read once from `store` in [`READ_WINDOW`] pieces fed to a
/// [`CellHasher`] as each arrives.
///
/// # Errors
/// [`SidecarError::Lost`] when the store has no object `key` (a HEAD that finds none, or a read
/// answered 404); [`SidecarError::Store`] when it fails or answers a read with another number of
/// bytes than were asked for.
pub async fn read_cells(
    store: &dyn LargeStore,
    key: &ObjectKey,
) -> Result<ObjectCells, SidecarError> {
    let lost = || SidecarError::Lost {
        key: key.to_string(),
    };
    let size = store.len(key).await?.ok_or_else(lost)?;
    let mut hasher = CellHasher::new();
    let mut offset = 0;
    while offset < size {
        let len = READ_WINDOW.min(size - offset);
        let piece = match store.read_range(key, offset, len).await {
            Err(ObjectStoreError::NotFound(_)) => return Err(lost()),
            other => other?,
        };
        if piece.len() as u64 != len {
            return Err(SidecarError::Store(ObjectStoreError::Body(format!(
                "{key}: {} bytes at {offset} where {len} were asked for",
                piece.len()
            ))));
        }
        hasher.update(&piece);
        offset += len;
    }
    let cells = hasher.finish();
    Ok(ObjectCells {
        size: cells.size,
        digests: cells.digests,
    })
}

/// Runs `work`, which blocks (a call of the pack index), on the runtime's blocking pool.
async fn blocking<T, F>(work: F) -> Result<T, PackIndexError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, PackIndexError> + Send + 'static,
{
    let joined = tokio::task::spawn_blocking(work).await;
    joined.unwrap_or_else(|broke| std::panic::resume_unwind(broke.into_panic()))
}
