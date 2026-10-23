//! The `update/*` rows of the desired release in the state store
//! (`docs/design/pull-updates.md` section 6.2): what the operator's calls write and the
//! offers read.
//!
//! | key | value | written by |
//! |---|---|---|
//! | `update/desired` | `{generation, commit, set_at}`, and the source when it is not a fetch ([`Desired`]) | an offer |
//! | `update/policy` | the nine settings (the pause among them), and the limits when they are not the default ([`Policy`]) | the pause button |
//!
//! and one counter, `update/generation`, from which generations are drawn. (The release
//! rows belong to the operator API.) The rows older builds kept for hosts and places, and
//! the order counters, are left in the store and only read ([`crate::stored_update`]). No row
//! has a time to live. An offer and the pause are compare-and-set on the version read, so a
//! writer that was overtaken re-reads; neither writes over a row it did not read.
//!
//! The rows are plain key-value rows of the state store, so they work on every backend and
//! need no table of their own; `kbf-state` is unchanged.

use std::sync::Arc;

use kbf_state::{Cond, Conflict, Store, StoreError, Txn, Version, Versioned};
use kbf_types::FarmTime;

use super::codec::{
    Corrupt, Desired, decode_desired, decode_policy, encode_desired, encode_policy,
};
use super::policy::Policy;
use super::run::{Generation, Release, Source};

const DESIRED: &str = "update/desired";
/// The counter the generations are drawn from.
const GENERATION: &str = "update/generation";
const POLICY: &str = "update/policy";
/// Reads and writes tried before a call gives up on writers it keeps racing.
const ATTEMPTS: usize = 16;

/// Why a row could not be read or written.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RowError {
    /// The store failed, or this handle is no longer its writer.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A row's bytes are not a row this build reads. The run does not guess.
    #[error("the row {key} cannot be read: {why}")]
    Corrupt {
        /// The row.
        key: String,
        /// What is wrong with it.
        why: String,
    },
    /// A row changed since it was read: another controller moved. Read again.
    #[error("a row changed under the writer: {0}")]
    Conflict(#[from] Conflict),
    /// Other writers kept changing the row for as long as the call tried.
    #[error("{0} kept changing under the writer")]
    Contended(&'static str),
    /// An offer expected another generation than the one desired now.
    #[error("the desired release is at generation {found}, not {expected}")]
    Stale {
        /// What the offer expected.
        expected: u64,
        /// What the row holds (0 before any offer).
        found: u64,
    },
    /// An offer names no release.
    #[error("an offer must name a release")]
    NoRelease,
}

/// A row as it was read: its value, and the version a write of it must name. `version` is
/// `None` for a row that does not exist, which is written with "must be absent".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored<T> {
    /// The value; the default for a row that does not exist.
    pub value: T,
    /// The version read, or `None` if there was no row.
    pub version: Option<Version>,
}

impl<T> Stored<T> {
    /// The precondition that the row is still as it was read.
    #[must_use]
    pub fn cond(&self) -> Cond {
        self.version.map_or(Cond::Absent, Cond::Version)
    }
}

/// The update rows of one state store.
#[derive(Clone)]
pub struct Rows {
    store: Arc<dyn Store>,
}

impl std::fmt::Debug for Rows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Rows")
    }
}

fn read<T>(
    key: &str,
    row: &Versioned,
    decode: impl FnOnce(&[u8]) -> Result<T, Corrupt>,
) -> Result<T, RowError> {
    decode(&row.value).map_err(|Corrupt(why)| RowError::Corrupt {
        key: key.to_owned(),
        why,
    })
}

impl Rows {
    /// The rows of `store`.
    #[must_use]
    pub fn new(store: Arc<dyn Store>) -> Self {
        Self { store }
    }

    /// The state store the rows live in, for the readers that look at other rows of it.
    pub(crate) fn store(&self) -> &Arc<dyn Store> {
        &self.store
    }

    /// The run, if one was ever offered.
    ///
    /// # Errors
    /// [`RowError`]: the store failed or the row cannot be read.
    pub fn desired(&self) -> Result<Stored<Option<Desired>>, RowError> {
        match self.store.get(DESIRED)? {
            None => Ok(Stored {
                value: None,
                version: None,
            }),
            Some(row) => Ok(Stored {
                value: Some(read(DESIRED, &row, decode_desired)?),
                version: Some(row.version),
            }),
        }
    }

    /// Sets the desired release to `commit`, to be fetched ([`Source::Fetch`]), under a higher
    /// generation. A rollback is the older commit offered the same way.
    /// Setting the commit that is already desired, from the same source, changes nothing and
    /// keeps the generation (`pull-updates.md` section 5), so a repeated click is not a new
    /// run; the same commit from another source is a new run. The generation is drawn from
    /// the counter `update/generation`; it is never below the current run's plus one,
    /// whatever the counter says. The pause is in `update/policy` and an offer does not touch
    /// it.
    ///
    /// # Errors
    /// [`RowError`]: the store failed, the row cannot be read, or `commit` is empty.
    pub fn offer(&self, commit: &Release, now: FarmTime) -> Result<Desired, RowError> {
        self.offer_expecting(commit, Source::Fetch, now, None)
    }

    /// [`Rows::offer`] from `source`, that writes only if the desired release is at generation
    /// `expected` (0 before any offer): a compare-and-set, so a click made on a stale page
    /// does not replace what another operator has just set. The check and the write are one
    /// conditional write of the row.
    ///
    /// # Errors
    /// [`RowError::Stale`] when the generation is another; otherwise as [`Rows::offer`].
    pub fn offer_expecting(
        &self,
        commit: &Release,
        source: Source,
        now: FarmTime,
        expected: Option<u64>,
    ) -> Result<Desired, RowError> {
        if commit.as_str().is_empty() {
            return Err(RowError::NoRelease);
        }
        for _ in 0..ATTEMPTS {
            let current = self.desired()?;
            if let Some(expected) = expected {
                let found = current.value.as_ref().map_or(0, |d| d.generation.0);
                if found != expected {
                    return Err(RowError::Stale { expected, found });
                }
            }
            // The run is the pair: `Release` has equality only, so the pair is compared
            // field by field and never ordered.
            let is_this_run = |d: &&Desired| d.commit == *commit && d.source == source;
            if let Some(same) = current.value.as_ref().filter(is_this_run) {
                return Ok(same.clone());
            }
            let drawn = self.store.txn(Txn::new().next(GENERATION))??;
            let counter = drawn.counters.first().copied().ok_or_else(|| {
                StoreError::Failed("the store returned no value for a counter".to_owned())
            })?;
            let above = current
                .value
                .as_ref()
                .map_or(1, |d| d.generation.0.saturating_add(1));
            let next = Desired {
                generation: Generation(counter.max(above)),
                commit: commit.clone(),
                set_at: now,
                source,
            };
            let txn = Txn::new().put(DESIRED, encode_desired(&next), current.cond());
            if self.store.txn(txn)?.is_ok() {
                return Ok(next);
            }
        }
        Err(RowError::Contended(DESIRED))
    }

    /// Pauses or resumes: the flag `update/policy.paused`. The rest of the policy stays as it was; the row is written by
    /// compare-and-set on the version read, so a policy written in between is not lost.
    /// A pause needs no run.
    ///
    /// # Errors
    /// [`RowError`]: the store failed, the row cannot be read, or writers kept racing.
    pub fn set_paused(&self, paused: bool) -> Result<(), RowError> {
        for _ in 0..ATTEMPTS {
            let current = self.policy_row()?;
            let cond = current.cond();
            let next = Policy {
                paused,
                ..current.value
            };
            let txn = Txn::new().put(POLICY, encode_policy(&next), cond);
            if self.store.txn(txn)?.is_ok() {
                return Ok(());
            }
        }
        Err(RowError::Contended(POLICY))
    }

    /// The policy row, if the operator ever wrote one.
    ///
    /// # Errors
    /// [`RowError`]: the store failed or the row cannot be read.
    pub fn policy_if_set(&self) -> Result<Option<Policy>, RowError> {
        self.store
            .get(POLICY)?
            .map(|row| read(POLICY, &row, decode_policy))
            .transpose()
    }

    /// The policy: the row if there is one, else the defaults.
    ///
    /// # Errors
    /// [`RowError`]: the store failed or the row cannot be read.
    pub fn policy(&self) -> Result<Policy, RowError> {
        Ok(self.policy_row()?.value)
    }

    fn policy_row(&self) -> Result<Stored<Policy>, RowError> {
        match self.store.get(POLICY)? {
            None => Ok(Stored {
                value: Policy::default(),
                version: None,
            }),
            Some(row) => Ok(Stored {
                value: read(POLICY, &row, decode_policy)?,
                version: Some(row.version),
            }),
        }
    }
}
