//! The write half of [`super::PackedCas`]: a daemon's outputs into packs.
//!
//! A daemon built to write packs ([`super::PackedCas::with_writes`]) puts the blobs of its
//! outputs ([`kbf_transfer::cas::Cas::put`], `put_chunks`, `put_records` and `put_spool`) into
//! packs of the farm's object store instead of sending them to the storage nodes. The write
//! rule is `kbf_pack::Packer`'s: a pack is written with a create-only put under the daemon's own
//! prefix (`cas/d/<node>/<boot>/<seq>`), and only once the store holds it is it committed
//! through the frontend ([`crate::RemoteIndex`]: `PackIndex/CommitPack`), which checks a sample
//! of its blobs and records it. **A put returns `Ok` only when the pack that holds its blob is
//! committed**, that is, stored and indexed; a blob the index has already is not packed again.
//!
//! The daemon writes packs only while [`PackCommit::enabled`]: it was built to, the frontend's
//! `Welcome` said it takes `CommitPack` (`Welcome.pack_commit`), and no commit has been answered
//! UNIMPLEMENTED since the process started. That answer turns packing off for the process (it is
//! logged once), and a later `Welcome` does not turn it on again.
//!
//! **Nothing a daemon writes is lost to a pack that did not work.** Every blob that is not
//! committed in a pack is written to the plane underneath, `C`, as it was before packs, and the
//! put returns `C`'s result:
//!
//! - the capability is off ([`FallbackStats::off`]);
//! - the blob is over `MAX_PACKED_READ_BYTES` (a read of it would not use packs), is not a
//!   SHA-256 digest, or does not fit the in-flight budget ([`FallbackStats::unfit`]);
//! - the frontend answered UNIMPLEMENTED ([`FallbackStats::unsupported`]) or refused the commit
//!   for a reason a repeat does not change ([`FallbackStats::refused`]: PERMISSION_DENIED,
//!   INVALID_ARGUMENT, a key recorded with other contents);
//! - the object store failed the put of the pack after the packer's attempts
//!   ([`FallbackStats::store`]);
//! - the commit failed after the packer's attempts ([`FallbackStats::record`]), or the index
//!   could not say which blobs it lacks ([`FallbackStats::index`]).
//!
//! A pack object that was written and whose commit then failed is **unindexed garbage under the
//! daemon's own prefix**: no row points at it, and this module deletes nothing (the farm's
//! garbage collection finds objects no row points at). The blobs of a put that fell back are
//! also in `C`, so they are not lost.
//!
//! A put of bytes whose digest is not the one named for them (the caller's bytes are wrong, not
//! the pack) is [`CasError::Corrupt`], as it is from `C`; it is not a fallback.
//!
//! **Budget.** The bytes of the blobs a put holds in memory while their pack is written count
//! against one budget for the process ([`WriteSettings::inflight_bytes`]), taken before the
//! bytes are read from where they are, and given back when the put is done. `put_records` reads
//! its records in groups of about one pack ([`WriteSettings::pack_bytes`]), so an output tree
//! is never held whole. A put waits for the puts ahead of it to give bytes back.
//!
//! One `put` or `put_chunks` is one pack of one blob; the blobs of one `put_records` are packed
//! together. Batching puts across calls is a follow-up.
//!
//! **Intake.** [`kbf_transfer::cas::Cas::intake`] is `C`'s: bytes a walk hands to `C`'s intake
//! as it reads them are stored by `C`, not packed.
//!
//! **Counts** are [`WriteStats`] (part of [`super::PackStats`]), logged with the read counts when
//! the plane is dropped. A daemon that writes packs
//! ([`crate::Daemon::with_pack_writes`]) also sends them in every heartbeat
//! (`Heartbeat.pack_writes`), read from the [`super::PackCounter`] its plane counts into.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_objstore::ObjectStore;
use kbf_pack::{Blob, FRONTEND_PACK_BYTES, PackError, PackKeys, Packed, Packer, PackerConfig};
use kbf_pack_index::{BlobDigest, MAX_BATCH, PackIndex, PackIndexError};
use kbf_proto::reapi::Digest;
use kbf_transfer::cas::{CasError, Chunks, label};
use kbf_transfer::{Record, proto_digest};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::{MAX_PACKED_READ_BYTES, add};
use crate::remote_index::wire;

/// The bytes of blobs that a plane holds in flight while their packs are written, by default:
/// 256 MiB.
pub const DEFAULT_WRITE_INFLIGHT_BYTES: u32 = 256 << 20;

/// How a plane cuts its packs and how much it holds in memory to write them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteSettings {
    /// The length at which a pack is closed, and about the bytes `put_records` reads at once.
    pub pack_bytes: u64,
    /// The bytes of blobs held in memory by the puts in flight together. A blob larger than
    /// this is written by the plane underneath.
    pub inflight_bytes: u32,
}

impl Default for WriteSettings {
    fn default() -> Self {
        Self {
            pack_bytes: FRONTEND_PACK_BYTES,
            inflight_bytes: DEFAULT_WRITE_INFLIGHT_BYTES,
        }
    }
}

/// Not told whether the frontend takes `CommitPack`: packs are not written.
const NOT_TOLD: u8 = 0;
/// The frontend's `Welcome` says it takes `CommitPack`.
const TAKEN: u8 = 1;
/// A commit was answered UNIMPLEMENTED: no pack is written for the rest of the process.
const UNSUPPORTED: u8 = 2;

/// What the plane logs, once, when a commit is answered UNIMPLEMENTED. A constant, so that the
/// call that logs it is one line, counted whether or not a subscriber takes it.
const UNSUPPORTED_WARNING: &str = "pack writes: the frontend does not take pack commits \
    (UNIMPLEMENTED); outputs go to the storage nodes for the rest of this process";

/// Whether the daemon may write packs: a handle the daemon sets from each `Welcome`
/// ([`crate::Daemon::with_pack_writes`]) and the plane reads before every put. A clone is the
/// same handle. A new one is off until a `Welcome` says the frontend takes `CommitPack`.
#[derive(Clone, Debug, Default)]
pub struct PackCommit(Arc<AtomicU8>);

impl PackCommit {
    /// A handle that is off until [`PackCommit::welcomed`] says the frontend takes commits.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records what a `Welcome` said (`Welcome.pack_commit`): a frontend that takes commits
    /// turns the handle on, one that does not (another frontend, after a reconnect) turns it
    /// off. A handle turned off for the process by [`PackCommit::unsupported`] stays off.
    pub fn welcomed(&self, takes_commits: bool) {
        let (from, to) = if takes_commits {
            (NOT_TOLD, TAKEN)
        } else {
            (TAKEN, NOT_TOLD)
        };
        let _ = self
            .0
            .compare_exchange(from, to, Ordering::Relaxed, Ordering::Relaxed);
    }

    /// Whether packs are written now.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.0.load(Ordering::Relaxed) == TAKEN
    }

    /// Turns packing off for the process: a commit was answered UNIMPLEMENTED. Logged the first
    /// time.
    pub(super) fn unsupported(&self) {
        if self.0.swap(UNSUPPORTED, Ordering::Relaxed) != UNSUPPORTED {
            tracing::warn!("{UNSUPPORTED_WARNING}");
        }
    }
}

/// What the packs of a plane wrote, and what went to the plane underneath instead: blobs
/// counted, not calls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteStats {
    /// Packs committed: stored, and recorded by the frontend.
    pub packs: u64,
    /// Blobs in those packs.
    pub packed: u64,
    /// The bytes of those packs, footers included.
    pub packed_bytes: u64,
    /// Where blobs went to the plane underneath, by reason.
    pub fallbacks: FallbackStats,
    /// Digests of `missing` that the index answered for.
    pub missing_indexed: u64,
    /// Digests of `missing` that went to the plane underneath whole because the index could not
    /// be asked.
    pub missing_unasked: u64,
}

/// Blobs written by the plane underneath instead of in a pack, by why.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FallbackStats {
    /// The capability was off: the frontend did not say it takes commits, or a commit was
    /// answered UNIMPLEMENTED earlier.
    pub off: u64,
    /// Over the largest blob a pack holds, not a SHA-256 digest, or over the in-flight budget.
    pub unfit: u64,
    /// The commit was answered UNIMPLEMENTED (and packing is off from then on).
    pub unsupported: u64,
    /// The commit was refused, and a repeat is refused again.
    pub refused: u64,
    /// The object store failed the put of the pack.
    pub store: u64,
    /// The commit failed for a reason a repeat may pass.
    pub record: u64,
    /// The index failed before the pack was written.
    pub index: u64,
}

impl fmt::Display for WriteStats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let by = &self.fallbacks;
        write!(
            f,
            "packs={} packed={} packed_bytes={} fallback_off={} fallback_unfit={} \
             fallback_unsupported={} fallback_refused={} fallback_store={} fallback_record={} \
             fallback_index={} missing_indexed={} missing_unasked={}",
            self.packs,
            self.packed,
            self.packed_bytes,
            by.off,
            by.unfit,
            by.unsupported,
            by.refused,
            by.store,
            by.record,
            by.index,
            self.missing_indexed,
            self.missing_unasked
        )
    }
}

/// [`WriteStats`] as they are counted.
#[derive(Debug, Default)]
pub(super) struct WriteCounts {
    packs: AtomicU64,
    packed: AtomicU64,
    packed_bytes: AtomicU64,
    off: AtomicU64,
    unfit: AtomicU64,
    unsupported: AtomicU64,
    refused: AtomicU64,
    store: AtomicU64,
    record: AtomicU64,
    index: AtomicU64,
    missing_indexed: AtomicU64,
    missing_unasked: AtomicU64,
    /// Whether the last write or question that failed was logged as a warning, and nothing
    /// has worked since.
    failing: AtomicBool,
}

impl WriteCounts {
    pub(super) fn stats(&self) -> WriteStats {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        WriteStats {
            packs: get(&self.packs),
            packed: get(&self.packed),
            packed_bytes: get(&self.packed_bytes),
            fallbacks: FallbackStats {
                off: get(&self.off),
                unfit: get(&self.unfit),
                unsupported: get(&self.unsupported),
                refused: get(&self.refused),
                store: get(&self.store),
                record: get(&self.record),
                index: get(&self.index),
            },
            missing_indexed: get(&self.missing_indexed),
            missing_unasked: get(&self.missing_unasked),
        }
    }

    /// Counts `blobs` that went to the plane underneath with the capability off.
    pub(super) fn off(&self, blobs: usize) {
        add(&self.off, blobs as u64);
    }

    /// Counts `blobs` that went to the plane underneath for not fitting a pack.
    pub(super) fn unfit(&self, blobs: usize) {
        add(&self.unfit, blobs as u64);
    }

    /// Logs that `what` failed with `error`: a warning for the first failure after something
    /// worked, a debug line for each after it, as a store or index that is down fails them all.
    fn say_failed(&self, what: &str, error: &dyn fmt::Display) {
        let line = format!("pack writes: {what}: {error}");
        if self.failing.swap(true, Ordering::Relaxed) {
            tracing::debug!("{line}");
        } else {
            tracing::warn!("{line}");
        }
    }

    /// Counts and logs the answer of the index to a `missing` that named `asked` digests, of which
    /// `indexed` could be asked of the index.
    pub(super) fn answered(
        &self,
        indexed: usize,
        asked: usize,
        answer: &Result<BTreeSet<BlobDigest>, PackIndexError>,
    ) {
        match answer {
            Ok(_) => {
                self.worked();
                add(&self.missing_indexed, indexed as u64);
            }
            Err(error) => {
                add(&self.missing_unasked, asked as u64);
                self.say_failed("the pack index could not say which blobs it lacks", error);
            }
        }
    }

    /// Notes that a write or a question worked.
    fn worked(&self) {
        self.failing.store(false, Ordering::Relaxed);
    }
}

/// Why blobs were not committed in a pack.
#[derive(Clone, Copy)]
enum Reason {
    Unsupported,
    Refused,
    Store,
    Record,
    Index,
}

impl Reason {
    /// The reason a failed `pack` call is.
    fn of(error: &PackError) -> Self {
        match error {
            PackError::Put { .. } => Self::Store,
            PackError::Record {
                source: PackIndexError::Unsupported,
                ..
            } => Self::Unsupported,
            PackError::Record { source, .. } if source.is_final() => Self::Refused,
            PackError::Record { .. } => Self::Record,
            _ => Self::Index,
        }
    }
}

/// What [`Writes::store`] did.
pub(super) enum Stored {
    /// Every blob is in a committed pack.
    Packed,
    /// The pack did not work (counted and logged): the plane underneath is to write the blobs.
    Elsewhere,
    /// The caller's bytes are not the digest named for them.
    Refused(CasError),
}

/// A packer, type-erased over the object store.
trait PackBlobs: Send + Sync {
    fn pack(&self, blobs: Vec<Blob>) -> BoxFuture<'_, Result<Packed, PackError>>;
}

impl<S: ObjectStore> PackBlobs for Packer<S> {
    fn pack(&self, blobs: Vec<Blob>) -> BoxFuture<'_, Result<Packed, PackError>> {
        Box::pin(Packer::pack(self, blobs))
    }
}

/// The packer of a plane and the budget its puts share: not generic, so that the plane's puts
/// compile once whatever the object store.
pub struct PackWriter {
    packer: Box<dyn PackBlobs>,
    index: Arc<dyn PackIndex>,
    room: Arc<Semaphore>,
    settings: WriteSettings,
}

impl PackWriter {
    /// A writer of packs to `store`, committed through `index` (a [`crate::RemoteIndex`] in a
    /// daemon), named by `keys` (`kbf_pack::WriterKind::Daemon`, the node id and this boot).
    ///
    /// # Errors
    /// [`PackError::NotCreateOnly`] when the store does not claim create-only writes. Run
    /// `kbf_pack::check_create_only` at startup to check the claim.
    pub fn new<S: ObjectStore>(
        store: Arc<S>,
        index: Arc<dyn PackIndex>,
        keys: PackKeys,
        settings: WriteSettings,
    ) -> Result<Self, PackError> {
        let config = PackerConfig {
            pack_bytes: settings.pack_bytes,
            ..PackerConfig::default()
        };
        let packer = Packer::new(store, Arc::clone(&index), keys, config)?;
        Ok(Self {
            packer: Box::new(packer),
            index,
            room: Arc::new(Semaphore::new(settings.inflight_bytes as usize)),
            settings,
        })
    }
}

/// The writer of a plane and the handle that says whether it may write.
#[derive(Clone)]
pub(super) struct Writes {
    writer: Arc<PackWriter>,
    commit: PackCommit,
}

/// Bytes of the in-flight budget, given back when dropped.
pub(super) type Room = OwnedSemaphorePermit;

impl Writes {
    pub(super) fn new(writer: Arc<PackWriter>, commit: PackCommit) -> Self {
        Self { writer, commit }
    }

    /// Whether packs are written now.
    pub(super) fn enabled(&self) -> bool {
        self.commit.enabled()
    }

    /// `bytes` of the in-flight budget, once the puts ahead have given enough back; `None`
    /// when `bytes` is over all of it.
    pub(super) async fn room(&self, bytes: u64) -> Option<Room> {
        let wanted = u32::try_from(bytes)
            .ok()
            .filter(|wanted| *wanted <= self.writer.settings.inflight_bytes)?;
        let room = Arc::clone(&self.writer.room);
        let taken = room.acquire_many_owned(wanted).await;
        Some(taken.expect("the semaphore of the budget is never closed"))
    }

    /// The bytes at which `put_records` closes a group.
    pub(super) fn group_bytes(&self) -> u64 {
        self.writer.settings.pack_bytes
    }

    /// Packs `blobs`, whose bytes `room` counts, and commits the packs (the module docs). The
    /// room is given back when this returns.
    pub(super) async fn store(&self, blobs: Vec<Blob>, room: Room, counts: &WriteCounts) -> Stored {
        let count = blobs.len();
        let packed = self.writer.packer.pack(blobs.clone()).await;
        drop(room);
        let error = match packed {
            Ok(packed) => {
                counts.worked();
                for pack in &packed.packs {
                    add(&counts.packs, 1);
                    add(&counts.packed, pack.blobs as u64);
                    add(&counts.packed_bytes, pack.size);
                }
                return Stored::Packed;
            }
            Err(PackError::DigestMismatch { claimed }) => {
                let blob = label(&wire(&claimed));
                return Stored::Refused(CasError::Corrupt(blob, "other bytes".to_owned()));
            }
            Err(error) => error,
        };
        let (counter, what) = match Reason::of(&error) {
            Reason::Unsupported => {
                self.commit.unsupported();
                add(&counts.unsupported, count as u64);
                return Stored::Elsewhere;
            }
            Reason::Refused => (&counts.refused, "the frontend refused a pack"),
            Reason::Store => (&counts.store, "the object store failed a pack"),
            Reason::Record => (&counts.record, "a pack was stored and not committed"),
            Reason::Index => (&counts.index, "the pack index could not be asked"),
        };
        add(counter, count as u64);
        counts.say_failed(what, &error);
        Stored::Elsewhere
    }

    /// The digests among `asked` that the index has no row for.
    ///
    /// # Errors
    /// The index failed.
    pub(super) async fn lacking(
        &self,
        asked: Vec<BlobDigest>,
    ) -> Result<BTreeSet<BlobDigest>, PackIndexError> {
        let index = Arc::clone(&self.writer.index);
        // The index's calls block (a daemon's waits for the frontend).
        let answered = tokio::task::spawn_blocking(move || {
            let mut lacking = BTreeSet::new();
            for chunk in asked.chunks(MAX_BATCH) {
                lacking.extend(index.find_missing(chunk)?);
            }
            Ok(lacking)
        })
        .await;
        answered.expect("a blocking pack index call panicked")
    }
}

/// Whether a blob of `size` bytes is read from a pack, and so is written to one.
pub(super) fn packable(size: u64) -> bool {
    i64::try_from(size).is_ok_and(|size| size <= MAX_PACKED_READ_BYTES)
}

/// `records` in groups of about `limit` bytes of blobs, in order: a group is closed before the
/// record that would take it over, so a record larger than `limit` is a group of its own.
pub(super) fn groups(records: Vec<Record>, limit: u64) -> Vec<Vec<Record>> {
    let mut closed = Vec::new();
    let mut open = Vec::new();
    let mut bytes = 0_u64;
    for record in records {
        let size = record.digest.size_bytes;
        if bytes.saturating_add(size) > limit {
            // Nothing to close before the first record, however large.
            if !open.is_empty() {
                closed.push(std::mem::take(&mut open));
                bytes = 0;
            }
        }
        bytes = bytes.saturating_add(size);
        open.push(record);
    }
    if !open.is_empty() {
        closed.push(open);
    }
    closed
}

/// The bytes of `group`, read from where the records say they are.
///
/// # Errors
/// [`CasError::Read`] when a record's bytes cannot be read (a file that shrank).
pub(super) async fn load(group: &[Record]) -> Result<Vec<Blob>, CasError> {
    let mut blobs = Vec::with_capacity(group.len());
    for record in group {
        let size = record.digest.size_bytes;
        let read = record.read(0, size).await;
        let bytes =
            read.map_err(|e| CasError::Read(label(&proto_digest(&record.digest)), e.to_string()))?;
        blobs.push(Blob {
            digest: BlobDigest {
                hash: record.digest.hash,
                size,
            },
            bytes: Bytes::from(bytes),
        });
    }
    Ok(blobs)
}

/// The bytes of `chunks`, which are the blob `digest` names.
///
/// # Errors
/// [`CasError::Read`] when a chunk cannot be read; [`CasError::Corrupt`] when the chunks
/// together are longer than the digest says (shorter ones are caught by the hash of the bytes).
pub(super) async fn gather(digest: &Digest, chunks: Chunks) -> Result<Vec<u8>, CasError> {
    let blob = label(digest);
    let size = usize::try_from(digest.size_bytes).unwrap_or(0);
    let mut bytes = Vec::new();
    let mut chunks = chunks;
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|e| CasError::Read(blob.clone(), e.to_string()))?;
        if bytes.len().saturating_add(chunk.len()) > size {
            return Err(CasError::Corrupt(blob, "more bytes than named".to_owned()));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
