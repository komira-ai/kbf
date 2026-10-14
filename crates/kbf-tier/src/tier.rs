//! The running tier: reservations, positioned writes, syncs, answers and seals.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use kbf_meta::{Epoch, ObjectId};
use kbf_segments::MAX_SEGMENT_BYTES;
use kbf_store::{Fs, FsFile};

use crate::block::block_name;
use crate::error::TierError;

/// How a tier is sized and when it seals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TierOptions {
    /// `--write-tier-bytes`: the most bytes the tier's block files may hold before
    /// they are flushed: every live block's reserved extent, open uploads' included.
    pub budget: u64,
    /// `--write-tier-seal-after`: how long the open grouped block takes reservations.
    pub seal_after: Duration,
    /// The size at which the open grouped block is sealed: [`MAX_SEGMENT_BYTES`].
    pub segment_limit: u64,
}

impl TierOptions {
    /// The seal delay of the design (30 s).
    pub const SEAL_AFTER: Duration = Duration::from_secs(30);

    /// A tier of `budget` bytes, sealing after [`TierOptions::SEAL_AFTER`] or at
    /// [`MAX_SEGMENT_BYTES`].
    #[must_use]
    pub const fn new(budget: u64) -> Self {
        Self {
            budget,
            seal_after: Self::SEAL_AFTER,
            segment_limit: MAX_SEGMENT_BYTES,
        }
    }
}

/// How full the tier is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    /// [`TierOptions::budget`].
    pub budget: u64,
    /// The bytes counted against it: the reserved extent of every block not yet
    /// unstaged or removed.
    pub used: u64,
}

/// A byte range reserved in one block file. Write it ([`WriteTier::write`]), sync it
/// ([`WriteTier::sync`]), and answer it: [`WriteTier::committed`] once its
/// `PutStaged` committed, or drop it (or its [`Synced`]) if that failed. Dropping it
/// unanswered is a failure: a seal never waits on a range whose caller went away.
#[derive(Debug)]
#[must_use = "an unanswered reservation is answered as failed when dropped"]
pub struct Reservation {
    object: ObjectId,
    offset: u64,
    len: u64,
    tier: Option<Weak<dyn Answer>>,
}

impl Reservation {
    /// The block (and so the object) the range is in.
    #[must_use]
    pub const fn object(&self) -> ObjectId {
        self.object
    }

    /// Where the range starts in the block: the offset of its first record in the
    /// object the block becomes.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// How many bytes it holds.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Whether it holds none.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The end of the range: `PutStaged`'s `end` for this object.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.offset + self.len
    }

    fn answer(&mut self, committed: bool) {
        if let Some(tier) = self.tier.take().and_then(|t| t.upgrade()) {
            tier.answer(self.object, self.end(), committed);
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.answer(false);
    }
}

/// A reservation whose bytes are durable (the file synced through its handle after
/// the writes, and the block's name synced): what a `PutStaged` may name. Only
/// [`WriteTier::sync`] makes one.
///
/// It takes no more writes: [`WriteTier::write`] takes a [`Reservation`], which
/// [`WriteTier::sync`] consumed, and a `Synced` hands none out. A byte written after
/// the sync would be acknowledged without being durable. What a caller does with one:
///
/// ```
/// # use kbf_tier::{Synced, WriteTier};
/// fn commit(tier: &dyn WriteTier, synced: Synced) {
///     let _put_staged = (synced.object(), synced.offset(), synced.end());
///     tier.committed(synced);
/// }
/// ```
///
/// and what it cannot:
///
/// ```compile_fail
/// # use kbf_tier::{Synced, WriteTier};
/// fn rewrite(tier: &dyn WriteTier, synced: &Synced) {
///     let _ = tier.write(synced.reservation(), 0, b"late");
/// }
/// ```
#[derive(Debug)]
#[must_use = "a synced reservation is answered as failed when dropped"]
pub struct Synced(Reservation);

impl Synced {
    /// The block (and so the object) the range is in: `PutStaged`'s object.
    #[must_use]
    pub const fn object(&self) -> ObjectId {
        self.0.object
    }

    /// Where the range starts in the block.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.0.offset
    }

    /// The end of the range: `PutStaged`'s `end` for this object.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.0.end()
    }
}

/// A sync that failed: the reservation (drop it to answer it as failed) and why.
#[derive(Debug)]
pub struct Unsynced {
    /// The reservation, still unanswered.
    pub reservation: Reservation,
    /// What failed.
    pub error: TierError,
}

/// The local write tier's disk layer, as the write group, segment uploads, the
/// flusher and the metadata (rollout stages 3 and 4b) use it. [`Tier`] is the one
/// implementation; the trait keeps the filesystem type out of its callers.
///
/// A caller's sequence, per range: reserve, write, sync, commit `PutStaged` naming
/// [`Synced::object`] and [`Synced::end`], then [`WriteTier::committed`]; on any
/// failure drop the reservation. Sealing: [`WriteTier::poll_seals`] names the blocks
/// whose `SealStaged` may be committed now; after that commit,
/// [`WriteTier::sealed`].
pub trait WriteTier: Send + Sync {
    /// Reserves `len` bytes in the open grouped block, opening a new block (and
    /// sealing the old one) if the range would pass the segment limit or the block's
    /// seal delay has passed at `now`.
    ///
    /// # Errors
    ///
    /// [`TierError::Full`] when the budget has no room (use the direct path);
    /// [`TierError::TooLarge`] for a range over the segment limit;
    /// [`TierError::Io`] if a new block file could not be created.
    fn reserve_group(&self, len: u64, now: Instant) -> Result<Reservation, TierError>;

    /// Reserves a block of its own for one segment of a segment upload, of `len`
    /// bytes (its records, up to the declared `index_offset`).
    ///
    /// # Errors
    ///
    /// [`TierError::Full`] when the budget has no room; [`TierError::Io`] if the
    /// block file could not be created.
    fn reserve_upload(&self, len: u64) -> Result<Reservation, TierError>;

    /// Writes `bytes` at `offset` within `reservation`'s range.
    ///
    /// # Errors
    ///
    /// [`TierError::OutOfRange`] for bytes outside the range;
    /// [`TierError::Poisoned`] after an earlier I/O error on the block;
    /// [`TierError::Io`], which poisons the block.
    fn write(&self, reservation: &Reservation, offset: u64, bytes: &[u8]) -> Result<(), TierError>;

    /// Makes the range's bytes and the block's name durable: `fdatasync` on the
    /// block's handle, then `sync_dir` once per block.
    ///
    /// # Errors
    ///
    /// An [`Unsynced`] holding the reservation: [`TierError::Poisoned`] if any I/O
    /// error on the block came before or during this sync (a failed sync on another
    /// range of the block may have taken this range's write-back error), or
    /// [`TierError::Io`], which poisons the block. The syncs of one block run one at
    /// a time, so none succeeds while another sync of the block is failing.
    fn sync(&self, reservation: Reservation) -> Result<Synced, Unsynced>;

    /// Answers a range whose `PutStaged` committed: the block's committed length
    /// becomes at least its end. An upload's block is sealed by the same commit.
    fn committed(&self, synced: Synced);

    /// Starts the seal of the open grouped block if its delay has passed at `now`,
    /// and names every block whose `SealStaged` may be committed: sealing, with every
    /// reservation answered and at least one committed. A sealing block whose
    /// reservations all failed is removed instead. A block is named on every call
    /// until [`WriteTier::sealed`].
    fn poll_seals(&self, now: Instant) -> Vec<ObjectId>;

    /// Records that `object`'s `SealStaged` committed.
    ///
    /// # Errors
    ///
    /// [`TierError::NotSealable`] unless [`WriteTier::poll_seals`] named it.
    fn sealed(&self, object: ObjectId) -> Result<(), TierError>;

    /// `len` bytes of `object`'s block from `offset`, all below its committed length.
    ///
    /// # Errors
    ///
    /// [`TierError::NotCommitted`] for bytes past the committed length (never
    /// acknowledged, so never served); [`TierError::UnknownBlock`]; [`TierError::Io`].
    fn read(&self, object: ObjectId, offset: u64, len: usize) -> Result<Vec<u8>, TierError>;

    /// Stops counting a sealed block against the budget once its object is flushed
    /// (`ObjectFlushed` committed). Its file is left as it is, for the flusher.
    ///
    /// # Errors
    ///
    /// [`TierError::NotSealable`] unless the block is sealed.
    fn unstage(&self, object: ObjectId) -> Result<(), TierError>;

    /// How full the tier is.
    fn usage(&self) -> Usage;
}

/// What a dropped or committed reservation tells its tier.
trait Answer: Send + Sync + std::fmt::Debug {
    fn answer(&self, object: ObjectId, end: u64, committed: bool);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Grouped,
    Upload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// The grouped block taking reservations, or an upload's block being written.
    Open,
    /// No new reservations; waiting for the outstanding ones to be answered.
    Sealing,
    /// Every reservation answered and one committed: `SealStaged` may be committed.
    SealReady,
    /// Sealed in the committed state: for the flusher.
    Sealed,
}

#[derive(Debug)]
pub(crate) struct Block<File> {
    kind: Kind,
    pub(crate) phase: Phase,
    /// The handle every write and sync of the block goes through; `None` once nothing
    /// will write it again.
    file: Option<Arc<Handle<File>>>,
    opened: Option<Instant>,
    /// The bytes reserved: the end of its last range. Counted against the budget.
    pub(crate) extent: u64,
    /// The committed length: the largest end of a committed range.
    pub(crate) committed: u64,
    committed_any: bool,
    outstanding: u64,
    dir_synced: bool,
    poisoned: bool,
}

impl<File> Block<File> {
    /// A block recovered at open: committed, never written again.
    pub(crate) const fn recovered(len: u64, phase: Phase) -> Self {
        Self {
            kind: Kind::Grouped,
            phase,
            file: None,
            opened: None,
            extent: len,
            committed: len,
            committed_any: true,
            outstanding: 0,
            dir_synced: true,
            poisoned: false,
        }
    }
}

/// A block's open file, and the lock its syncs take one at a time.
#[derive(Debug)]
struct Handle<File> {
    file: File,
    /// Held from the `fdatasync` through the poison check and the `sync_dir`. The
    /// kernel reports a write-back error to one sync of the shared handle only: a
    /// sync that overlapped a failing one could otherwise succeed before that one
    /// poisoned the block, or before the block's name was durable.
    sync: Mutex<()>,
}

/// A block's handle, and whether the block is poisoned.
type Opened<File> = (Arc<Handle<File>>, bool);

#[derive(Debug)]
pub(crate) struct State<File> {
    pub(crate) blocks: BTreeMap<ObjectId, Block<File>>,
    /// The grouped block taking reservations.
    open: Option<ObjectId>,
    pub(crate) used: u64,
}

impl<File> State<File> {
    pub(crate) const fn new(used: u64) -> Self {
        Self {
            blocks: BTreeMap::new(),
            open: None,
            used,
        }
    }
}

/// Allocates the sequence numbers of the tier's objects within its epoch, shared with
/// the writer that numbers the direct path's objects so the two never collide.
pub type NextSeq = Box<dyn Fn() -> u64 + Send + Sync>;

struct Shared<F: Fs> {
    fs: F,
    options: TierOptions,
    epoch: Epoch,
    state: Mutex<State<F::File>>,
    next_seq: NextSeq,
}

impl<F: Fs + std::fmt::Debug> std::fmt::Debug for Shared<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tier")
            .field("fs", &self.fs)
            .field("options", &self.options)
            .field("epoch", &self.epoch)
            .finish_non_exhaustive()
    }
}

/// The local write tier over the block files of one directory ([`Fs`]): see
/// [`WriteTier`]. Made by [`Tier::open`](crate::Tier::open).
pub struct Tier<F: Fs> {
    shared: Arc<Shared<F>>,
}

impl<F: Fs + std::fmt::Debug> std::fmt::Debug for Tier<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.shared.fmt(f)
    }
}

impl<F> Tier<F>
where
    F: Fs + Send + Sync + std::fmt::Debug + 'static,
    F::File: Send + Sync + std::fmt::Debug + 'static,
{
    pub(crate) fn from_parts(
        fs: F,
        options: TierOptions,
        epoch: Epoch,
        state: State<F::File>,
        next_seq: NextSeq,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                fs,
                options,
                epoch,
                state: Mutex::new(state),
                next_seq,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<F::File>> {
        self.shared.lock()
    }

    /// Creates a block of `kind` with one reservation of `len` bytes.
    fn new_block(
        &self,
        state: &mut State<F::File>,
        kind: Kind,
        len: u64,
        now: Option<Instant>,
    ) -> Result<Reservation, TierError> {
        let object = ObjectId::new(self.shared.epoch, (self.shared.next_seq)());
        let name = block_name(object);
        let file = self
            .shared
            .fs
            .create_positioned(&name)
            .map_err(|e| TierError::io("create", &name, e))?;
        state.blocks.insert(
            object,
            Block {
                kind,
                phase: Phase::Open,
                file: Some(Arc::new(Handle {
                    file,
                    sync: Mutex::new(()),
                })),
                opened: now,
                extent: 0,
                committed: 0,
                committed_any: false,
                outstanding: 0,
                dir_synced: false,
                poisoned: false,
            },
        );
        Ok(self.take_range(state, object, len))
    }

    fn take_range(&self, state: &mut State<F::File>, object: ObjectId, len: u64) -> Reservation {
        let block = state.blocks.get_mut(&object).expect("block just looked up");
        let offset = block.extent;
        block.extent += len;
        block.outstanding += 1;
        state.used += len;
        let tier: Weak<dyn Answer> = Arc::downgrade(&self.shared) as Weak<dyn Answer>;
        Reservation {
            object,
            offset,
            len,
            tier: Some(tier),
        }
    }

    fn check_budget(&self, state: &State<F::File>, len: u64) -> Result<(), TierError> {
        let budget = self.shared.options.budget;
        if state.used.saturating_add(len) > budget {
            return Err(TierError::Full {
                want: len,
                used: state.used,
                budget,
            });
        }
        Ok(())
    }

    /// The handle of the block a range is in, and whether the block is poisoned.
    fn handle(&self, object: ObjectId) -> Result<Opened<F::File>, TierError> {
        let state = self.lock();
        let block = state
            .blocks
            .get(&object)
            .ok_or(TierError::UnknownBlock(object))?;
        let file = block.file.clone().ok_or(TierError::UnknownBlock(object))?;
        Ok((file, block.poisoned))
    }

    fn poison(&self, object: ObjectId, error: TierError) -> TierError {
        let mut state = self.lock();
        let block = state
            .blocks
            .get_mut(&object)
            .expect("block of a reservation");
        block.poisoned = true;
        error
    }

    fn sync_range(&self, r: &Reservation) -> Result<(), TierError> {
        // Poisoning is checked after the sync, not before: see below.
        let (handle, _) = self.handle(r.object)?;
        let _one_at_a_time = handle.sync.lock().unwrap_or_else(PoisonError::into_inner);
        let name = block_name(r.object);
        handle
            .file
            .sync()
            .map_err(|e| self.poison(r.object, TierError::io("sync", &name, e)))?;
        let mut state = self.lock();
        let block = state
            .blocks
            .get_mut(&r.object)
            .expect("block of a reservation");
        // A failed write or sync of another range before this sync (a failing sync
        // cannot overlap it: the lock) may have reported, and so cleared, a
        // write-back error of this range's pages: this sync's success proves nothing
        // then.
        if block.poisoned {
            return Err(TierError::Poisoned(r.object));
        }
        if !block.dir_synced {
            drop(state);
            self.shared
                .fs
                .sync_dir()
                .map_err(|e| self.poison(r.object, TierError::io("sync_dir", &name, e)))?;
            let mut state = self.lock();
            let block = state
                .blocks
                .get_mut(&r.object)
                .expect("block of a reservation");
            block.dir_synced = true;
        }
        Ok(())
    }
}

impl<F: Fs> Shared<F> {
    fn lock(&self) -> MutexGuard<'_, State<F::File>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Moves a sealing block on once its reservations are all answered: to
    /// [`Phase::SealReady`] if one committed, else removes it.
    fn settle(&self, state: &mut State<F::File>, object: ObjectId) {
        let block = state.blocks.get_mut(&object).expect("block to settle");
        if block.phase != Phase::Sealing || block.outstanding > 0 {
            return;
        }
        block.file = None;
        if block.committed_any {
            block.phase = Phase::SealReady;
        } else {
            self.remove(state, object);
        }
    }

    /// Forgets a block nothing committed and deletes its file. A failed removal leaves
    /// an orphan, which the next start's sweep deletes.
    fn remove(&self, state: &mut State<F::File>, object: ObjectId) {
        let block = state.blocks.remove(&object).expect("block to remove");
        state.used -= block.extent;
        let _ = self.fs.remove(&block_name(object));
    }

    /// Whether `block`'s seal delay has passed at `now`.
    fn aged(&self, block: &Block<F::File>, now: Instant) -> bool {
        block
            .opened
            .is_some_and(|t| now.saturating_duration_since(t) >= self.options.seal_after)
    }

    /// Seals the open grouped block `object`: it takes no new range.
    fn begin_seal(&self, state: &mut State<F::File>, object: ObjectId) {
        state.open = None;
        let block = state.blocks.get_mut(&object).expect("open block");
        block.phase = Phase::Sealing;
        self.settle(state, object);
    }
}

impl<F> Answer for Shared<F>
where
    F: Fs + Send + Sync + std::fmt::Debug,
    F::File: Send + Sync + std::fmt::Debug,
{
    fn answer(&self, object: ObjectId, end: u64, committed: bool) {
        let mut state = self.lock();
        let block = state
            .blocks
            .get_mut(&object)
            .expect("block of a reservation");
        block.outstanding -= 1;
        if committed {
            block.committed = block.committed.max(end);
            block.committed_any = true;
        }
        if block.kind == Kind::Upload {
            if committed {
                block.phase = Phase::Sealed;
                block.file = None;
            } else {
                self.remove(&mut state, object);
            }
            return;
        }
        self.settle(&mut state, object);
    }
}

impl<F> WriteTier for Tier<F>
where
    F: Fs + Send + Sync + std::fmt::Debug + 'static,
    F::File: Send + Sync + std::fmt::Debug + 'static,
{
    fn reserve_group(&self, len: u64, now: Instant) -> Result<Reservation, TierError> {
        let limit = self.shared.options.segment_limit;
        if len > limit {
            return Err(TierError::TooLarge { len, limit });
        }
        let mut state = self.lock();
        let done =
            |b: &Block<F::File>| b.poisoned || self.shared.aged(b, now) || b.extent + len > limit;
        if let Some(object) = state.open.filter(|o| done(&state.blocks[o])) {
            self.shared.begin_seal(&mut state, object);
        }
        self.check_budget(&state, len)?;
        let r = match state.open {
            Some(object) => self.take_range(&mut state, object, len),
            None => {
                let r = self.new_block(&mut state, Kind::Grouped, len, Some(now))?;
                state.open = Some(r.object);
                r
            }
        };
        if state.blocks[&r.object].extent >= limit {
            self.shared.begin_seal(&mut state, r.object);
        }
        Ok(r)
    }

    fn reserve_upload(&self, len: u64) -> Result<Reservation, TierError> {
        let mut state = self.lock();
        self.check_budget(&state, len)?;
        self.new_block(&mut state, Kind::Upload, len, None)
    }

    fn write(&self, r: &Reservation, offset: u64, bytes: &[u8]) -> Result<(), TierError> {
        let len = bytes.len() as u64;
        if offset.checked_add(len).is_none_or(|end| end > r.len) {
            return Err(TierError::OutOfRange {
                offset,
                len,
                reserved: r.len,
            });
        }
        let (handle, poisoned) = self.handle(r.object)?;
        if poisoned {
            return Err(TierError::Poisoned(r.object));
        }
        handle.file.write_at(r.offset + offset, bytes).map_err(|e| {
            let error = TierError::io("write", &block_name(r.object), e);
            self.poison(r.object, error)
        })
    }

    fn sync(&self, reservation: Reservation) -> Result<Synced, Unsynced> {
        match self.sync_range(&reservation) {
            Ok(()) => Ok(Synced(reservation)),
            Err(error) => Err(Unsynced { reservation, error }),
        }
    }

    fn committed(&self, mut synced: Synced) {
        synced.0.answer(true);
    }

    fn poll_seals(&self, now: Instant) -> Vec<ObjectId> {
        let mut state = self.lock();
        let aged = state
            .open
            .filter(|o| self.shared.aged(&state.blocks[o], now));
        if let Some(object) = aged {
            self.shared.begin_seal(&mut state, object);
        }
        state
            .blocks
            .iter()
            .filter(|(_, b)| b.phase == Phase::SealReady)
            .map(|(object, _)| *object)
            .collect()
    }

    fn sealed(&self, object: ObjectId) -> Result<(), TierError> {
        let mut state = self.lock();
        match state.blocks.get_mut(&object) {
            Some(block) if block.phase == Phase::SealReady => {
                block.phase = Phase::Sealed;
                Ok(())
            }
            _ => Err(TierError::NotSealable(object)),
        }
    }

    fn read(&self, object: ObjectId, offset: u64, len: usize) -> Result<Vec<u8>, TierError> {
        let committed = self
            .lock()
            .blocks
            .get(&object)
            .map(|b| b.committed)
            .ok_or(TierError::UnknownBlock(object))?;
        let end = offset.saturating_add(len as u64);
        if end > committed {
            return Err(TierError::NotCommitted {
                object,
                end,
                committed,
            });
        }
        let name = block_name(object);
        self.shared
            .fs
            .read_at(&name, offset, len)
            .map_err(|e| TierError::io("read", &name, e))
    }

    fn unstage(&self, object: ObjectId) -> Result<(), TierError> {
        let mut state = self.lock();
        let extent = match state.blocks.get(&object) {
            Some(block) if block.phase == Phase::Sealed => block.extent,
            _ => return Err(TierError::NotSealable(object)),
        };
        state.blocks.remove(&object);
        state.used -= extent;
        Ok(())
    }

    fn usage(&self) -> Usage {
        Usage {
            budget: self.shared.options.budget,
            used: self.lock().used,
        }
    }
}
