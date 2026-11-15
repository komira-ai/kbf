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

impl Durable {
    /// Applies `samples`, the runs of one attempt key, to the stored summaries of the key
    /// and of its level 2 (the same row for a key with no configuration), in one
    /// transaction, reading again for as long as another writer commits first.
    ///
    /// # Errors
    /// The store cannot be read or written, or refuses the write for a newer writer.
    pub(in crate::farm) fn add_samples(&self, samples: &Samples) -> Result<(), StateError> {
        let (key, runs) = samples;
        let levels = levels_of(key);
        loop {
            let mut held = Summaries::default();
            let mut conds = Vec::with_capacity(levels.len());
            for level in &levels {
                let name = estimate::row_key(level);
                let cond = match self.store.get(&name)? {
                    Some(row) => {
                        // A row that cannot be read is replaced, as it was when the rows
                        // were written from memory: the key starts again from these runs.
                        let readable =
                            estimate::decode(&name, &row.value).is_some_and(|(_, summary)| {
                                held.insert_encoded(level, &summary).is_ok()
                            });
                        if !readable {
                            tracing::warn!(%name, "a size summary row cannot be read: replaced");
                        }
                        Cond::Version(row.version)
                    }
                    None => Cond::Absent,
                };
                conds.push((name, cond));
            }
            for sample in runs {
                held.observe(key, &sample.run, sample.day, &sample.bounds);
            }
            let mut txn = Txn::new();
            for (level, (name, cond)) in levels.iter().zip(conds) {
                if let Some(summary) = held.get(level) {
                    txn = txn.put(name, estimate::encode(&summary.encode()), cond);
                }
            }
            if self.store.txn(txn)?.is_ok() {
                return Ok(());
            }
        }
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
