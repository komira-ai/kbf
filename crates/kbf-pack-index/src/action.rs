//! The action cache: the result of an action, by the action's digest.
//!
//! [`ActionStore`] is the typed view. An index that holds the pack rows also holds the action
//! rows (`PostgresPackIndex` in the `action_result` table of migration 3, [`crate::schema`];
//! `MemoryPackIndex` in a map), because the one rule that matters is that they are written
//! together: a result names blobs, the blobs are found through the pack index, and a result
//! must never be readable while a blob it names has no recorded pack.
//!
//! - **One row per action digest, the encoded result inline.** The store holds the bytes of
//!   an `ActionResult` and does not read them: it is a small record (an exit code, the
//!   digests of stdout and stderr, the output files with their digests), never the outputs.
//!   A result that encodes to no bytes is a valid result and is stored as an empty row. A
//!   result longer than [`MAX_RESULT_BYTES`] is [`PackIndexError::Invalid`].
//! - **A put replaces.** A later put for the same action replaces the row, last writer wins,
//!   as `UpdateActionResult` does; [`ActionPut::replaced`] says whether there was a row.
//! - **A put records its packs in the same transaction.** [`ActionStore::put_action`] takes
//!   the packs the action's outputs were written in and records them as
//!   [`PackIndex::record_pack`] does, then writes the row, all or nothing: a refused pack
//!   ([`PackIndexError::Invalid`] or [`PackIndexError::Conflict`]) writes no row and records
//!   no pack, and an existing row for the action is left as it was. A put repeated with the
//!   same packs succeeds (each pack is `created: false`).
//! - **A get is a query.** [`ActionStore::get_action`] writes nothing.
//! - **A cold cache is a valid cache.** Nothing is imported: an action with no row has not
//!   been run, and the answer for it is `None`.

use crate::{BlobDigest, NewPack, PackIndex, PackIndexError, Recorded};

/// The most bytes of encoded result one action row holds.
pub const MAX_RESULT_BYTES: usize = 1 << 20;

/// An action's result as the store holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionEntry {
    /// The encoded `ActionResult`, as it was put.
    pub result: Vec<u8>,
    /// When the row was written (the last put), in microseconds of the index's clock.
    pub written_us: i64,
}

/// What a put came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionPut {
    /// The action had a row, which the put replaced. `false`: the action is new to the cache.
    pub replaced: bool,
    /// What recording each pack came to, in the order the packs were given.
    pub packs: Vec<Recorded>,
}

/// The action cache, held with the pack index so that a result and the packs of its outputs
/// are written together. Calls block, as the index's do.
pub trait ActionStore: PackIndex {
    /// The result put for `action`, or `None` when the action has no row.
    ///
    /// # Errors
    /// [`PackIndexError::NotMigrated`] when the schema has no action cache table;
    /// [`PackIndexError::Store`] when the store fails.
    fn get_action(&self, action: &BlobDigest) -> Result<Option<ActionEntry>, PackIndexError>;

    /// Records `packs`, as [`PackIndex::record_pack`] does, and writes `result` as the row of
    /// `action`, in one transaction: either all of it happens or none.
    ///
    /// # Errors
    /// [`PackIndexError::Invalid`] for a result longer than [`MAX_RESULT_BYTES`] or a pack
    /// that cannot exist; [`PackIndexError::Conflict`] for a pack key recorded with other
    /// contents; [`PackIndexError::NotMigrated`] when the schema has no action cache table;
    /// [`PackIndexError::Store`] when the store fails. Nothing is written for any of them.
    fn put_action(
        &self,
        action: &BlobDigest,
        result: &[u8],
        packs: &[NewPack],
    ) -> Result<ActionPut, PackIndexError>;
}

/// `Err(Invalid)` for a result the store does not hold.
pub(crate) fn check_result(result: &[u8]) -> Result<(), PackIndexError> {
    if result.len() > MAX_RESULT_BYTES {
        return Err(PackIndexError::Invalid(
            "the action result is longer than an action row holds",
        ));
    }
    Ok(())
}

/// The packs in the order a put records them: by key, so that two puts that name the same
/// packs take their rows in the same order and never wait on each other in a cycle. Each is
/// paired with its place in the call, which the answer is put back in.
pub(crate) fn in_key_order(packs: &[NewPack]) -> Vec<(usize, &NewPack)> {
    let mut order: Vec<(usize, &NewPack)> = packs.iter().enumerate().collect();
    order.sort_by(|a, b| a.1.key.cmp(&b.1.key));
    order
}
