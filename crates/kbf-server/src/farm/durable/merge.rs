//! Writing the runs a frontend saw into the `est/` rows: read, apply, compare-and-set.
//!
//! The rows are shared by every frontend of a store. A row is never written from one
//! process's memory: the stored summaries of the key are read with their versions, the
//! runs seen since are applied to them, and the result is written with the version read
//! ([`Cond::Version`], [`Cond::Absent`] for a level with no row). A write that lost the
//! race to another frontend changed nothing, so the rows are read again and the same runs
//! are applied to what the winner wrote: every run is applied once, and no frontend's
//! samples are overwritten.

use kbf_estimator::{Key, Summaries};

use super::*;
use crate::learned::Samples;

/// How many times one key's write reads again after another frontend committed first,
/// before the runs are left for a later tick.
const CONFLICT_RETRIES: usize = 8;

/// Why a batch was not wholly applied: the runs of `batch[from..]` are still owed.
pub(in crate::farm) type Unwritten = (usize, StateError);

impl Durable {
    /// Applies `batch`, the runs of attempt keys, to the stored summaries of the keys and
    /// of their level 2 (the same row for a key with no configuration). The batch is read
    /// once and written in one transaction; if another writer committed first, each key is
    /// then applied on its own, reading again as often as [`CONFLICT_RETRIES`] allows.
    ///
    /// # Errors
    /// The store cannot be read or written, or refuses the write for a newer writer, or a
    /// key lost the race more than [`CONFLICT_RETRIES`] times: the first key not applied is
    /// returned with the error, and nothing after it was tried.
    pub(in crate::farm) fn add_samples(&self, batch: &[Samples]) -> Result<(), Unwritten> {
        if batch.len() > 1 {
            match self.attempt(batch) {
                Ok(None) => return Ok(()),
                Ok(Some(_)) => {}
                Err(error) => return Err((0, error)),
            }
        }
        for (at, samples) in batch.iter().enumerate() {
            self.add_one(samples).map_err(|error| (at, error))?;
        }
        Ok(())
    }

    fn add_one(&self, samples: &Samples) -> Result<(), StateError> {
        let mut lost = 0;
        loop {
            let Some(conflict) = self.attempt(std::slice::from_ref(samples))? else {
                return Ok(());
            };
            lost += 1;
            if lost == CONFLICT_RETRIES {
                return Err(StateError::Conflict(conflict));
            }
        }
    }

    /// One read, apply and compare-and-set of `batch`; the conflict if another writer
    /// committed a row of it since the read.
    fn attempt(&self, batch: &[Samples]) -> Result<Option<Conflict>, StateError> {
        let mut levels = BTreeMap::new();
        for (key, _) in batch {
            for level in levels_of(key) {
                levels.insert(estimate::row_key(&level), level);
            }
        }
        let mut rows = self.read_rows(&levels)?;
        let mut held = Summaries::default();
        let mut conds = BTreeMap::new();
        for (name, level) in &levels {
            let cond = match rows.remove(name) {
                Some(row) => {
                    // A row that cannot be read is replaced, as it was when the rows
                    // were written from memory: the key starts again from these runs.
                    let readable = estimate::decode(name, &row.value)
                        .is_some_and(|(_, summary)| held.insert_encoded(level, &summary).is_ok());
                    if !readable {
                        tracing::warn!(%name, "a size summary row cannot be read: replaced");
                    }
                    Cond::Version(row.version)
                }
                None => Cond::Absent,
            };
            conds.insert(name, cond);
        }
        for (key, runs) in batch {
            for sample in runs {
                held.observe(key, &sample.run, sample.day, &sample.bounds);
            }
        }
        let mut txn = Txn::new();
        for (name, level) in &levels {
            if let (Some(summary), Some(cond)) = (held.get(level), conds.remove(name)) {
                txn = txn.put(name.clone(), estimate::encode(&summary.encode()), cond);
            }
        }
        Ok(self.store.txn(txn)?.err())
    }

    /// The stored rows of `levels`: a key's own two rows are read one by one, more than
    /// that with one scan of the `est/` rows (the store has no multi-get).
    fn read_rows(
        &self,
        levels: &BTreeMap<String, Key>,
    ) -> Result<BTreeMap<String, Versioned>, StateError> {
        let mut rows = BTreeMap::new();
        if levels.len() > 2 {
            for (name, row) in self.store.scan_prefix(ESTIMATE)? {
                if levels.contains_key(&name) {
                    rows.insert(name, row);
                }
            }
        } else {
            for name in levels.keys() {
                if let Some(row) = self.store.get(name)? {
                    rows.insert(name.clone(), row);
                }
            }
        }
        Ok(rows)
    }
}

/// The keys `observe` of `key` changes: its level 2 and itself, once if they are one.
fn levels_of(key: &Key) -> Vec<Key> {
    let l2 = key.l2();
    if key.cfg().is_some() {
        vec![l2, key.clone()]
    } else {
        vec![l2]
    }
}
