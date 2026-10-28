//! What several frontends of one state store share: the blocks of operation ids and lease
//! sequences each reserves with a compare-and-set on the counter row, and the epoch row,
//! made once, kept by a start and raised by the operator. The rows are described in the
//! module above.

use std::ops::Range;
use std::sync::PoisonError;

use kbf_sched::PLACEMENT_ROUND;
use kbf_state::{Cond, Txn};
use kbf_types::OperationId;

use super::{Durable, EPOCH, EpochBump, EpochPolicy, NEXT_OP, NEXT_SEQ, StateError, number};
use crate::farm::engine::Engine;

/// How many operation ids, or lease sequences, one reservation takes from `meta/next_op`
/// or `meta/next_seq`. A frontend writes the counter once for this many ids, so a block
/// is as many operations as a frontend submits, or leases it grants, between two writes
/// of the counter. A frontend that stops leaves the rest of its block unused: ids are 64
/// bits, and an unused id costs nothing.
pub const ID_BLOCK: u64 = 1024;

/// A frontend reserves its next block of lease sequences when fewer than this many are
/// left: two placement rounds, so a feed never finds the block spent in the middle of a
/// round of grants.
const SEQS_LOW: u64 = 2 * PLACEMENT_ROUND as u64;

// A fresh block must hold more sequences than SEQS_LOW, or every feed would reserve one.
const _: () = assert!(SEQS_LOW < ID_BLOCK);

/// How many times in all a compare-and-set that another frontend wrote under is tried
/// again, reading the row again each time.
pub(super) const CAS_TRIES: usize = 8;

/// Runs `attempt`, and again while it loses a compare-and-set to another writer (it reads
/// the rows it depends on again each time), at most [`CAS_TRIES`] times in all. The last
/// conflict is returned.
pub(super) fn contended<T>(
    mut attempt: impl FnMut() -> Result<T, StateError>,
) -> Result<T, StateError> {
    let mut tried = 1;
    loop {
        match attempt() {
            Err(StateError::Conflict(_)) if tried < CAS_TRIES => tried += 1,
            done => return done,
        }
    }
}

impl Durable {
    /// The epoch this start runs under. A store with no epoch row gets one made with an
    /// absent-only write; if another frontend made it first, the write conflicts and the
    /// row is read again, so several frontends that open an empty store together end with
    /// the one epoch. A bump that loses to another writer reads the new row and bumps it.
    pub(super) fn settle_epoch(&self, policy: EpochPolicy, first: u64) -> Result<u64, StateError> {
        contended(|| {
            let row = self.store.get(EPOCH)?;
            let (epoch, write) = match (&row, policy) {
                (None, _) => (first, Some(Cond::Absent)),
                (Some(row), EpochPolicy::Keep) => (number(EPOCH, row)?, None),
                (Some(row), EpochPolicy::Bump) => {
                    (number(EPOCH, row)? + 1, Some(Cond::Version(row.version)))
                }
            };
            if let Some(cond) = write {
                self.commit(Txn::new().put(EPOCH, epoch.to_be_bytes(), cond))?;
            }
            Ok(epoch)
        })
    }

    /// Raises the stored epoch by one: an operator's bump, for the day the store was
    /// restored from a backup and the daemons must drop the leases the lost history
    /// granted. A frontend reads the epoch when it starts, so one that runs now keeps the
    /// epoch it started with. Returns the epoch before and the one stored now. A bump
    /// that loses to another writer reads the new row and bumps that.
    pub(super) fn bump_epoch(&self) -> Result<EpochBump, StateError> {
        contended(|| {
            let row = self.row(EPOCH)?;
            let previous = number(EPOCH, &row)?;
            let epoch = previous + 1;
            let cond = Cond::Version(row.version);
            self.commit(Txn::new().put(EPOCH, epoch.to_be_bytes(), cond))?;
            Ok(EpochBump { previous, epoch })
        })
    }

    /// Reserves a block of [`ID_BLOCK`] ids of `counter` (`meta/next_op` or
    /// `meta/next_seq`) for this frontend: the stored value, or `own` if that is later, up to
    /// [`ID_BLOCK`] past it. The counter is written to the end of the block with a
    /// compare-and-set on the version read, so it never goes back and no other frontend's
    /// block overlaps; a write that loses to another frontend reads the counter again.
    ///
    /// # Errors
    /// The store cannot be read or written, the counter is not a number, or other
    /// frontends won every one of [`CAS_TRIES`] tries ([`StateError::Conflict`]).
    fn reserve(&self, counter: &str, own: u64) -> Result<Range<u64>, StateError> {
        contended(|| {
            let (stored, cond) = match self.store.get(counter)? {
                Some(row) => (number(counter, &row)?, Cond::Version(row.version)),
                None => (0, Cond::Absent),
            };
            let start = stored.max(own);
            let end = start.saturating_add(ID_BLOCK);
            self.commit(Txn::new().put(counter, end.to_be_bytes(), cond))?;
            Ok(start..end)
        })
    }

    /// The id an operation is submitted under: `wanted` if it lies in the block this handle
    /// reserved, the start of that block if `wanted` is behind it (an earlier submit in
    /// the block failed, so the scheduler did not move on), else the start of a block
    /// reserved now (`wanted`, or the store's counter if that is later). The caller makes
    /// the scheduler number from the id returned.
    ///
    /// # Errors
    /// As [`Durable::reserve`].
    pub(super) fn reserve_operation(&self, wanted: OperationId) -> Result<OperationId, StateError> {
        let mut ops = self.ops.lock().unwrap_or_else(PoisonError::into_inner);
        if ops.contains(&wanted.0) {
            return Ok(wanted);
        }
        if wanted.0 < ops.start {
            return Ok(OperationId(ops.start));
        }
        *ops = self.reserve(NEXT_OP, wanted.0)?;
        Ok(OperationId(ops.start))
    }

    /// Reserves a block of lease sequences from `own` on (or the store's counter, if that
    /// is later), for the scheduler to number leases in ([`kbf_sched::Scheduler::use_seq_block`]).
    ///
    /// # Errors
    /// As [`Durable::reserve`].
    pub(super) fn reserve_seqs(&self, own: u64) -> Result<Range<u64>, StateError> {
        self.reserve(NEXT_SEQ, own)
    }
}

impl Engine {
    /// Reserves the next block of lease sequences once the scheduler has fewer than
    /// [`SEQS_LOW`] left in its block, and gives it to the scheduler, which places no lease
    /// past the end of its block. Called at the start of every feed, after
    /// [`Engine::watch_store`]: while the store is down nothing is placed, and the
    /// reservation waits for it. A reservation the store refuses is tried again at the
    /// next feed, and the scheduler places what is left of its block meanwhile.
    pub(in crate::farm) fn reserve_seqs(&mut self) {
        if self.sched.seqs_left() >= SEQS_LOW {
            return;
        }
        if self.store_down {
            return;
        }
        let own = self.sched.next_seq();
        let mut block = None;
        self.durably("a block of lease sequences", |d| {
            block = Some(d.reserve_seqs(own)?);
            Ok(())
        });
        if let Some(block) = block {
            self.sched.use_seq_block(block);
        }
    }
}
