//! Repair: the debt a chain write leaves when the replica could not take its copy
//! (`docs/design/architecture-v2.md`, section 8: with the replica down, the primary
//! acknowledges after its own durable write and records `repair/<digest>`).
//!
//! A [`RepairRecord`] says "this node holds a blob that a second owner does not". The
//! record is the whole difference between a one-copy write that is acknowledged and one
//! that is a silent loss of redundancy, so it is kept in the [`Journal`]: a file on the
//! node's own disk, made durable before the write is acknowledged and kept until the
//! second copy exists. [`Repairs`] is the journal behind an async interface and the seam to
//! the frontend ([`RepairLedger`]); [`RepairWorker`] makes the copies owed, to the owners
//! the map names now, when they can take them, and only then settles the debt.

mod journal;
mod ledger;
#[cfg(test)]
mod tests;
mod worker;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

pub use journal::{JOURNAL, JOURNAL_TMP, Journal, JournalError, Options};
use kbf_map::MemberId;
use kbf_store::Fs;
use kbf_types::Digest;
pub use ledger::{NoLedger, RepairLedger};
pub use worker::{Pass, RepairConfig, RepairWorker};

use crate::StorageError;

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

/// What [`Repairs`] needs of a journal, so it need not be generic over the disk it is on.
trait Debts: Send {
    fn owed(&mut self, record: RepairRecord) -> Result<(), JournalError>;
    fn done(&mut self, digest: &Digest) -> Result<(), JournalError>;
    fn records(&self) -> Vec<RepairRecord>;
}

impl<F: Fs + Send> Debts for Journal<F>
where
    F::File: Send,
{
    fn owed(&mut self, record: RepairRecord) -> Result<(), JournalError> {
        Journal::owed(self, record)
    }

    fn done(&mut self, digest: &Digest) -> Result<(), JournalError> {
        Journal::done(self, digest)
    }

    fn records(&self) -> Vec<RepairRecord> {
        Journal::records(self).copied().collect()
    }
}

/// The copies this node owes: the journal on its disk, and the seam through which a
/// frontend is told ([`RepairLedger`]).
///
/// **The rule this exists for.** A write is answered with one copy only after
/// [`Repairs::owe`] has returned: the record is on the disk. The frontend is told after
/// that, and never waited for.
pub struct Repairs {
    debts: Arc<Mutex<Box<dyn Debts>>>,
    ledger: Arc<dyn RepairLedger>,
}

impl Repairs {
    /// The debts in `fs`'s directory (creating the journal), reporting to `ledger`.
    ///
    /// # Errors
    /// The journal cannot be opened ([`Journal::open`]).
    pub fn open<F>(
        fs: F,
        options: Options,
        ledger: Arc<dyn RepairLedger>,
    ) -> Result<Self, JournalError>
    where
        F: Fs + Send + 'static,
        F::File: Send,
    {
        let journal = Journal::open(fs, options)?;
        Ok(Self {
            debts: Arc::new(Mutex::new(Box::new(journal))),
            ledger,
        })
    }

    fn lock(debts: &Mutex<Box<dyn Debts>>) -> MutexGuard<'_, Box<dyn Debts>> {
        debts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records `record` as owed. It is durable when this returns; the ledger is told
    /// afterwards, off to the side.
    ///
    /// # Errors
    /// [`StorageError::Unavailable`] if the journal could not make it durable (or has
    /// stopped after an earlier failure): nothing is owed, and the caller must not answer
    /// the write with one copy.
    pub async fn owe(&self, record: RepairRecord) -> Result<(), StorageError> {
        let debts = Arc::clone(&self.debts);
        tokio::task::spawn_blocking(move || Self::lock(&debts).owed(record))
            .await
            .map_err(|e| StorageError::Internal(format!("the repair journal: {e}")))?
            .map_err(|e| StorageError::Unavailable(Arc::new(e)))?;
        let ledger = Arc::clone(&self.ledger);
        tokio::spawn(async move { ledger.owed(&record).await });
        Ok(())
    }

    /// Records that `digest` is no longer owed: every owner holds it. Not synced (a debt that
    /// returns after a power cut is settled again at the cost of a check). The ledger is
    /// told afterwards, off to the side.
    ///
    /// # Errors
    /// [`StorageError::Unavailable`] if the journal could not write it (the debt stays).
    pub(crate) async fn settle(&self, digest: &Digest) -> Result<(), StorageError> {
        let debts = Arc::clone(&self.debts);
        let settled = *digest;
        tokio::task::spawn_blocking(move || Self::lock(&debts).done(&settled))
            .await
            .map_err(|e| StorageError::Internal(format!("the repair journal: {e}")))?
            .map_err(|e| StorageError::Unavailable(Arc::new(e)))?;
        let ledger = Arc::clone(&self.ledger);
        tokio::spawn(async move { ledger.settled(&settled).await });
        Ok(())
    }

    /// Tells the ledger of every copy owed, off to the side: a frontend that was away, or
    /// lost what it knew, learns it again.
    pub(crate) fn announce(&self) {
        let records = self.open_records();
        let ledger = Arc::clone(&self.ledger);
        tokio::spawn(async move {
            for record in records {
                ledger.owed(&record).await;
            }
        });
    }

    /// Every copy owed now, in digest order.
    #[must_use]
    pub fn open_records(&self) -> Vec<RepairRecord> {
        Self::lock(&self.debts).records()
    }
}
