//! The pack index: where each blob lies inside a pack.
//!
//! A pack is one object that holds many blobs. The index maps a blob's digest (its hash and
//! its size) to the pack that holds it and the offset in that pack, so a reader asks for one
//! range of one object. The caller records a pack only once it is durable in the object
//! store, never before, so that a row only ever points at a pack that exists; the index
//! cannot check it.
//!
//! The same index holds the action cache, one row per action digest with the encoded result
//! inline, written in the transaction that records the packs of the action's outputs: see
//! [`action`]. It holds the bytes of each REAPI Directory a walk has read, too, by the
//! Directory's digest, so a walk of an input tree reads a level with one query: see
//! [`directory`].
//!
//! [`PackIndex`] is the typed view; `postgres::PostgresPackIndex` (feature `postgres`) keeps
//! it in tables of the Postgres state store (migration 2, [`schema`]) that are outside the key/value and feed tables
//! and outside the commit head, so recording blobs never takes the lock that state
//! transactions queue on. `memory::MemoryPackIndex` (feature `test-support`) is the
//! in-memory index, and `conformance` states the rules once for both:
//!
//! - **Recording is one transaction and idempotent on the pack's key.** The same pack
//!   recorded twice changes nothing. A second record of a key with other contents is
//!   [`PackIndexError::Conflict`] and changes nothing. The contents compared are the writer,
//!   the size, the number of blobs and the blobs the first record made rows for, each at its
//!   offset; a blob that already had a row in another pack then is not compared. A retry is
//!   recognised while the rows the first record made still point at the pack. A row removed
//!   by [`PackIndex::drop_location`] also takes one off the pack's `live_blobs`, so the
//!   rows left are the ones the pack counts and a later record of the identical pack under
//!   the same key is still `created: false`, not [`PackIndexError::Conflict`]; the dropped
//!   blob is not given a row again by it (the key is recorded), only by a record of another
//!   pack that holds it. A pack all of whose rows are dropped has no row pointing at it, so
//!   [`PackIndex::dead_packs`] lists it once its grace has passed.
//! - **The first location of a blob is kept.** A blob already indexed in another pack keeps
//!   that location; the later pack holds a copy that no row points at.
//! - **Digest and key batches are bounded.** A call that takes digests or keys names at most
//!   [`MAX_BATCH`] of them; the blob list of a pack is bounded by the pack, not by the index.
//! - **A location can be dropped.** A reader that fetched bytes that are not a blob's digest
//!   asks the index to drop that blob's row, but only while it still points into the pack
//!   the bytes came from; a row that points at another pack is kept. A dropped blob has no
//!   row until a pack that holds it is recorded: the first location wins again.
//! - **A pack can be forgotten.** A reader the object store told that a pack is gone (or
//!   shorter than its rows say), twice and some time apart, asks the index to forget it: every
//!   row that points into it and the pack's own row go, in one transaction, so its blobs are
//!   missing until a pack that holds them is recorded, and the same key recorded again is a new
//!   pack. A pack recorded less than the age the caller names before is kept whole.
//! - **Reads are queries.** [`PackIndex::locate`], [`PackIndex::find_missing`],
//!   [`PackIndex::dead_packs`] and [`PackIndex::unreferenced_keys`] write nothing, and
//!   [`PackIndex::touch`] writes `last_touch_us` and nothing else. Blob rows are inserted by
//!   a record and removed only by [`PackIndex::drop_location`] and [`PackIndex::forget_pack`].

use std::time::Duration;

use kbf_state::StoreError;

use crate::directory::NewDirectory;
#[cfg(any(test, feature = "test-support", feature = "postgres"))]
use kbf_state::StoreTime;

pub mod action;
#[cfg(any(test, feature = "test-support"))]
pub mod action_conformance;
#[cfg(any(test, feature = "test-support"))]
pub mod conformance;
pub mod directory;
#[cfg(any(test, feature = "test-support"))]
pub mod directory_conformance;
#[cfg(any(test, feature = "test-support"))]
pub mod memory;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "postgres")]
pub mod schema;

/// The most digests (or keys) one call of the index takes.
pub const MAX_BATCH: usize = 1000;

/// A blob's identity: the SHA-256 of its bytes and its length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlobDigest {
    /// The SHA-256 of the blob's bytes.
    pub hash: [u8; 32],
    /// The blob's length in bytes.
    pub size: u64,
}

/// A blob of a pack, as its footer lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PackBlob {
    /// The blob.
    pub digest: BlobDigest,
    /// Where the blob's bytes start in the pack.
    pub offset: u64,
}

/// A pack to record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewPack {
    /// The pack's object key. Recording is idempotent on it.
    pub key: String,
    /// Who wrote the pack (a node or a frontend).
    pub writer: String,
    /// The pack's length in bytes.
    pub size: u64,
    /// The blobs the pack holds, in any order.
    pub blobs: Vec<PackBlob>,
}

/// What recording a pack came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recorded {
    /// The pack was new. `false`: the key was recorded before, with these contents (see the
    /// module's rule on what is compared).
    pub created: bool,
    /// How many blobs the index points at this pack: the ones it had no row for.
    pub indexed: u32,
}

/// Where a blob is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// The key of the pack that holds the blob.
    pub pack_key: String,
    /// Where the blob's bytes start in the pack.
    pub offset: u64,
    /// The blob's length in bytes.
    pub length: u64,
    /// When the blob was last touched (recorded, or [`PackIndex::touch`]ed), in microseconds
    /// of the index's clock.
    pub last_touch_us: i64,
}

/// A recorded pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackInfo {
    /// The pack's object key.
    pub key: String,
    /// Who wrote the pack.
    pub writer: String,
    /// The pack's length in bytes.
    pub size: u64,
    /// When the pack was recorded, in microseconds of the index's clock.
    pub created_us: i64,
    /// How many blobs the pack holds.
    pub blob_count: u32,
    /// How many of them the index pointed at this pack when it was recorded.
    pub live_blobs: u32,
}

/// What [`PackIndex::forget_pack`] came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Forgot {
    /// The pack and every blob row that pointed into it are gone; `rows` is how many blob rows.
    Forgotten {
        /// The blob rows removed.
        rows: usize,
    },
    /// The pack was recorded less than the age asked ago: nothing was removed.
    Young,
    /// No pack is recorded under the key: nothing was removed.
    NotRecorded,
}

/// Why a call of the index failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PackIndexError {
    /// The pack to record is not one that can exist.
    #[error("pack index: {0}")]
    Invalid(&'static str),
    /// The key was recorded before with other contents, as the module's rule compares them.
    /// Nothing was changed.
    #[error("pack index: pack {key:?} is already recorded with other contents")]
    Conflict {
        /// The pack's key.
        key: String,
    },
    /// The index behind this call does not take the call, and will not until it is set up to:
    /// a client's commit of a pack to a frontend that has no pack store.
    #[error("pack index: the index takes no such call")]
    Unsupported,
    /// The index refused the call for a reason that a repeat does not change (the caller is not
    /// allowed to make it, or the pack it names is not acceptable). `0` says why.
    #[error("pack index: refused: {0}")]
    Refused(String),
    /// A call named more digests or keys than [`MAX_BATCH`].
    #[error("pack index: {given} entries in one call, at most {max}")]
    TooMany {
        /// How many the call named.
        given: usize,
        /// The most a call takes.
        max: usize,
    },
    /// The state schema has no pack tables: the installer has not applied the migration.
    #[error("pack index: the state schema has no pack tables: run `kbf-installer setup`")]
    NotMigrated,
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl PackIndexError {
    /// Whether a repeat of the call that failed with this error gets the same answer: the
    /// call or the pack is refused, so trying again only waits for the same refusal. A store
    /// failure, or a schema that is migrated meanwhile, may pass.
    #[must_use]
    pub fn is_final(&self) -> bool {
        match self {
            Self::Invalid(_)
            | Self::Conflict { .. }
            | Self::TooMany { .. }
            | Self::Unsupported
            | Self::Refused(_) => true,
            Self::NotMigrated | Self::Store(_) => false,
        }
    }
}

/// The index of blobs in packs. Calls block, as the state store's do.
pub trait PackIndex: Send + Sync {
    /// Records `pack` and the blobs it holds, in one transaction.
    ///
    /// A key recorded before with the same contents changes nothing and answers
    /// `created: false`; the blob list is compared on the blobs the first record made rows
    /// for, and a blob that was indexed in another pack then is not compared. A blob that has
    /// a row already keeps it.
    ///
    /// # Errors
    /// [`PackIndexError::Invalid`] for a pack that cannot exist (no key, a size or a blob
    /// the index cannot hold, a digest twice, a blob past the end of the pack);
    /// [`PackIndexError::Conflict`] for a key recorded with other contents;
    /// [`PackIndexError::Unsupported`] or [`PackIndexError::Refused`] from an index that is
    /// reached through a service that does not take or does not allow the record;
    /// [`PackIndexError::Store`] when the store fails.
    fn record_pack(&self, pack: &NewPack) -> Result<Recorded, PackIndexError>;

    /// The pack recorded under `key`.
    ///
    /// # Errors
    /// [`PackIndexError::Store`] when the store fails.
    fn pack(&self, key: &str) -> Result<Option<PackInfo>, PackIndexError>;

    /// For each of `digests`, in order, where its blob is, or `None` when it has no row.
    ///
    /// # Errors
    /// [`PackIndexError::TooMany`] for more than [`MAX_BATCH`] digests;
    /// [`PackIndexError::Store`] when the store fails.
    fn locate(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError>;

    /// The digests among `digests` that have no row, in the order given.
    ///
    /// # Errors
    /// As [`PackIndex::locate`].
    fn find_missing(&self, digests: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError>;

    /// Sets `last_touch_us` of the blobs among `digests` that have a row to the index's
    /// clock, and changes nothing else. Returns how many blobs it touched.
    ///
    /// # Errors
    /// As [`PackIndex::locate`].
    fn touch(&self, digests: &[BlobDigest]) -> Result<usize, PackIndexError>;

    /// At most `limit` packs recorded more than `grace` ago that no blob points at, oldest
    /// first. Read-only: what to do with them is the caller's.
    ///
    /// # Errors
    /// [`PackIndexError::Store`] when the store fails.
    fn dead_packs(&self, grace: Duration, limit: usize) -> Result<Vec<PackInfo>, PackIndexError>;

    /// The keys among `keys` that no pack is recorded under, in the order given. Read-only:
    /// a listing of the object store is checked against the index with it.
    ///
    /// # Errors
    /// [`PackIndexError::TooMany`] for more than [`MAX_BATCH`] keys;
    /// [`PackIndexError::Store`] when the store fails.
    fn unreferenced_keys(&self, keys: &[String]) -> Result<Vec<String>, PackIndexError>;

    /// Removes the row of `digest` only if it points into the pack recorded under
    /// `pack_key`, and takes one off that pack's `live_blobs`. Returns whether a row was
    /// removed: `false` when the digest has no row or its row points at another pack.
    ///
    /// A reader calls it when the bytes it fetched for the digest from that pack are not the
    /// digest. Afterwards [`PackIndex::locate`] answers `None` for the digest until a pack
    /// that holds it is recorded.
    ///
    /// The default answers [`PackIndexError::Unsupported`], for an index reached through a
    /// service that takes no such call.
    ///
    /// # Errors
    /// [`PackIndexError::Unsupported`] or [`PackIndexError::Refused`] from an index that does
    /// not take or allow the call; [`PackIndexError::Store`] when the store fails.
    fn drop_location(&self, digest: &BlobDigest, pack_key: &str) -> Result<bool, PackIndexError> {
        let _ = (digest, pack_key);
        Err(PackIndexError::Unsupported)
    }

    /// Forgets the pack recorded under `pack_key` if it was recorded at least `min_age` ago by
    /// the index's clock: removes every blob row that points into it and the pack's own row, in
    /// one transaction. The age is read in that transaction, so a pack recorded again under the
    /// key a moment before is young and kept ([`Forgot::Young`]); a key nobody recorded removes
    /// nothing ([`Forgot::NotRecorded`]).
    ///
    /// A caller calls it once the object store has answered, twice and some time apart, that
    /// the object under `pack_key` is not there. Afterwards [`PackIndex::find_missing`] lists
    /// the pack's blobs, so a client uploads them again, and a pack recorded under the same key
    /// again is a new pack whose blobs get rows ([`PackIndex::record_pack`] answers
    /// `created: true`), where a pack row kept with no rows would answer `created: false` and
    /// make none. A blob whose row is in the pack and whose bytes are in another pack too (the
    /// first location wins, so that pack has no row for it) is missing as well, and is packed
    /// again when a client uploads it.
    ///
    /// The default answers [`PackIndexError::Unsupported`], as [`PackIndex::drop_location`]
    /// does.
    ///
    /// # Errors
    /// [`PackIndexError::Unsupported`] or [`PackIndexError::Refused`] from an index that does
    /// not take or allow the call; [`PackIndexError::Store`] when the store fails.
    fn forget_pack(&self, pack_key: &str, min_age: Duration) -> Result<Forgot, PackIndexError> {
        let _ = (pack_key, min_age);
        Err(PackIndexError::Unsupported)
    }

    /// For each of `digests`, in order, the bytes of the Directory recorded under it, or `None`
    /// when it has no row. Writes nothing. See [`directory`] for the rules.
    ///
    /// The default answers [`PackIndexError::Unsupported`], for an index that keeps no
    /// Directory rows: the caller reads each Directory from its blob.
    ///
    /// # Errors
    /// [`PackIndexError::TooMany`] for more than [`MAX_BATCH`] digests;
    /// [`PackIndexError::NotMigrated`] when the schema has no directory table;
    /// [`PackIndexError::Unsupported`] from an index that keeps no Directory rows;
    /// [`PackIndexError::Store`] when the store fails.
    fn directories(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Vec<u8>>>, PackIndexError> {
        let _ = digests;
        Err(PackIndexError::Unsupported)
    }

    /// Records each of `directories` that has no row yet, in one transaction, and returns how
    /// many rows it inserted. A digest that has a row keeps it (it is the same bytes).
    ///
    /// The default answers [`PackIndexError::Unsupported`], as [`PackIndex::directories`] does.
    ///
    /// # Errors
    /// [`PackIndexError::Invalid`] when a body is not its digest or is longer than
    /// [`directory::MAX_DIRECTORY_BYTES`], and nothing is written;
    /// [`PackIndexError::TooMany`] for more than [`MAX_BATCH`] Directories;
    /// [`PackIndexError::NotMigrated`] when the schema has no directory table;
    /// [`PackIndexError::Unsupported`] from an index that keeps no Directory rows;
    /// [`PackIndexError::Store`] when the store fails.
    fn record_directories(&self, directories: &[NewDirectory]) -> Result<usize, PackIndexError> {
        let _ = directories;
        Err(PackIndexError::Unsupported)
    }
}

#[cfg(any(test, feature = "test-support", feature = "postgres"))]
impl NewPack {
    /// The blobs sorted by digest, once the pack is known to be one that can exist. Both
    /// indexes insert in this order, so two recordings never wait on each other in a cycle.
    /// A pack longer than a `bigint` holds is refused, which bounds every offset and size.
    pub(crate) fn checked(&self) -> Result<Vec<PackBlob>, PackIndexError> {
        if self.key.is_empty() {
            return Err(PackIndexError::Invalid("the pack key is empty"));
        }
        if i64::try_from(self.size).is_err() {
            return Err(PackIndexError::Invalid(
                "the pack is longer than the index can hold",
            ));
        }
        let mut blobs = self.blobs.clone();
        blobs.sort_by_key(|blob| blob.digest);
        let twice = blobs.windows(2).any(|p| p[0].digest == p[1].digest);
        if twice {
            return Err(PackIndexError::Invalid(
                "a digest appears twice in the pack",
            ));
        }
        let past = |blob: &PackBlob| {
            let end = blob.offset.checked_add(blob.digest.size);
            end.is_none_or(|end| end > self.size)
        };
        if blobs.iter().any(past) {
            return Err(PackIndexError::Invalid(
                "a blob reaches past the end of the pack",
            ));
        }
        // A blob of no bytes shares none (the frontend's own packs hold the empty blob).
        let mut placed: Vec<(u64, u64)> = blobs
            .iter()
            .filter(|blob| blob.digest.size > 0)
            .map(|blob| (blob.offset, blob.offset + blob.digest.size))
            .collect();
        placed.sort_unstable();
        if placed.windows(2).any(|pair| pair[1].0 < pair[0].1) {
            return Err(PackIndexError::Invalid("two blobs of the pack overlap"));
        }
        Ok(blobs)
    }
}

/// `Err(TooMany)` when a call names more than [`MAX_BATCH`] entries.
#[cfg(any(test, feature = "test-support", feature = "postgres"))]
pub(crate) fn check_batch(given: usize) -> Result<(), PackIndexError> {
    if given > MAX_BATCH {
        return Err(PackIndexError::TooMany {
            given,
            max: MAX_BATCH,
        });
    }
    Ok(())
}

/// A duration in whole microseconds, saturating.
#[cfg(any(test, feature = "test-support", feature = "postgres"))]
pub(crate) fn micros(d: Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

/// A reading of a clock in whole microseconds.
#[cfg(any(test, feature = "test-support", feature = "postgres"))]
pub(crate) fn micros_of(time: StoreTime) -> i64 {
    micros(time.as_duration())
}

/// A count as the `u32` the index reports, saturating.
#[cfg(any(test, feature = "test-support", feature = "postgres"))]
pub(crate) fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
