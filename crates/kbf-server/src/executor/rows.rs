//! The executor's rows of the state store: what lets any frontend of one store answer an
//! operation by name, and take up an operation whose frontend has gone.
//!
//! | Key | Value | Written |
//! |---|---|---|
//! | `meta/next_op` | the next free operation id, 8 bytes big-endian | a block of [`ID_BLOCK`] ids reserved by compare-and-set |
//! | `exec/live/<instance>` | empty | by its process, every quarter of `--executor-live-secs`, with that time to live |
//! | `exec/op/<operation>` | [`OpRow`] | before each attempt's submit; deleted when the operation finishes |
//! | `done/<operation>` | `Finished::to_row` | when the operation finishes, for at least [`DONE_FLOOR`] |
//! | `doneof/<operation>` | the action and instance (version 1, size, hash, instance) | beside `done/` |
//!
//! `meta/next_op`, `done/` and `doneof/` keep the format of earlier releases, so a frontend of
//! the release before and one of this release over one store never give one id twice and each
//! answers by name an operation the other finished. The `exec/live/` and `exec/op/` families
//! are new: no scan of an earlier release reads a prefix of them (B8: it loses nothing by not
//! knowing them, they are this executor's own bookkeeping).
//!
//! **Who runs an operation.** An `exec/op/` row names the process that runs the operation, its
//! `owner`. Only the owner writes it, each write a compare-and-set on the version it read with
//! itself as the owner. A process whose `exec/live/` row has expired on the store's clock is
//! gone: another frontend takes its unfinished operations ([`Rows::claim`]), each in one
//! transaction that checks the liveness row is still absent and puts the operation row at the
//! version read, so one claimer wins. A gone owner that comes back finds its next write refused
//! and leaves the operation to the claimer ([`RowsError::Conflict`]).
//!
//! # Format
//!
//! An `exec/op/` value is one version byte ([`VERSION`]) and a JSON object. A reader ignores the
//! fields it does not name, so a later release adds a field and this one still reads the row. A
//! row whose version byte this build does not know was written by a later release: a scan skips
//! it and counts it ([`Rows::unknown_rows`]). A row never holds a file or environment value of
//! a job.

use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_front::Finished;
use kbf_state::{Cond, Store, StoreError, Txn, Version};
use kbf_types::{ActionKey, Digest, DigestFunction, OperationId};
use serde::{Deserialize, Serialize};

/// The first key component of every row family of the executor. No scan prefix of any other
/// family of the state store is a prefix of it.
pub const ROOT: &str = "exec/";

/// The prefix of the liveness rows: `exec/live/<instance>`.
pub const LIVE_PREFIX: &str = "exec/live/";

/// The prefix of the operation rows: `exec/op/<operation, 16 hex digits>`.
pub const OP_PREFIX: &str = "exec/op/";

/// The version byte every `exec/op/` value begins with.
pub const VERSION: u8 = 1;

/// The counter of operation ids every frontend of the store reserves its blocks from.
const NEXT_OP: &str = "meta/next_op";

/// The prefix of a finished operation's answer.
const DONE: &str = "done/";

/// The prefix of a finished operation's action and instance.
const DONE_OF: &str = "doneof/";

/// The version byte of a `doneof/` row.
const DONE_OF_VERSION: u8 = 1;

/// How many operation ids one reservation takes from `meta/next_op`. A frontend that stops
/// leaves the rest of its block unused: ids are 64 bits, and an unused id costs nothing.
pub const ID_BLOCK: u64 = 1024;

/// The least time a `done/` row is kept, whatever `--finished-retention-secs` says: long
/// enough for a client that lost its stream to ask again on another frontend.
pub const DONE_FLOOR: Duration = Duration::from_secs(600);

/// How many times a compare-and-set that another writer won is tried again, reading the row
/// again each time.
const CAS_TRIES: usize = 8;

/// Why a row call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RowsError {
    /// The store itself failed (or its connection did); nothing is known of the write.
    #[error("exec rows: {0}")]
    Store(#[from] StoreError),
    /// The row named is not as the caller read it: another process runs the operation, or
    /// wrote the counter first every time.
    #[error("exec rows: the precondition on {key:?} failed")]
    Conflict {
        /// The key whose precondition failed.
        key: String,
    },
    /// A row of a known version that is not what it says.
    #[error("exec rows: {key:?} is corrupt: {why}")]
    Corrupt {
        /// The key.
        key: String,
        /// What is wrong.
        why: String,
    },
}

/// What an unfinished operation's row holds: enough for any frontend to read its job back,
/// finish it and answer it by name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpRow {
    /// The process that runs the operation (its `kbf-instance`).
    pub owner: String,
    /// The instance name the operation was submitted under.
    pub instance: String,
    /// The action's SHA-256, in lower-case hex.
    pub hash: String,
    /// The action's size in bytes.
    pub size: u64,
    /// The attempt whose job is submitted, from 0.
    pub attempt: u32,
    /// The id of that attempt's job.
    pub job: String,
    /// The put directory the job commits into, `cas/b/<build>/<nonce>`, always written. A row
    /// with none is read as one whose pack cannot be found: the process that takes the
    /// operation up runs it again.
    #[serde(default)]
    pub key: Option<String>,
    /// The action's platform asks for a whole machine.
    #[serde(default)]
    pub whole_machine: bool,
}

impl OpRow {
    /// The instance and action of the operation.
    ///
    /// # Errors
    /// The hash is not 64 hex digits.
    pub fn action_key(&self) -> Result<ActionKey, String> {
        let digest = Digest::parse(
            DigestFunction::Sha256,
            &format!("{}/{}", self.hash, self.size),
        )
        .map_err(|e| e.to_string())?;
        Ok(ActionKey {
            instance: self.instance.clone(),
            action: digest,
        })
    }
}

/// Where an operation stands, as the store has it.
#[derive(Clone, Debug, PartialEq)]
pub enum Look {
    /// Unfinished: its row.
    Open(OpRow),
    /// Finished: its action and answer.
    Done(ActionKey, Finished),
    /// The store has neither: it never existed, or its answer is past its retention.
    Gone,
}

/// An unfinished operation of a gone process, as read: its row and the version to claim it at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Orphan {
    /// The operation.
    pub operation: OperationId,
    /// Its row.
    pub row: OpRow,
    /// The row's version.
    pub version: Version,
}

/// The executor's handle on the state store.
pub struct Rows {
    store: Arc<dyn Store>,
    /// This process's id, the `owner` of the rows it writes.
    instance: String,
    /// The block of operation ids this process holds; empty until the first is asked for.
    ids: Mutex<Range<u64>>,
    /// Rows skipped for a version byte this build does not know.
    unknown: AtomicU64,
}

/// The key of `operation`'s row.
#[must_use]
pub fn op_key(operation: OperationId) -> String {
    format!("{OP_PREFIX}{:016x}", operation.0)
}

fn done_key(operation: OperationId) -> String {
    format!("{DONE}{:016x}", operation.0)
}

fn done_of_key(operation: OperationId) -> String {
    format!("{DONE_OF}{:016x}", operation.0)
}

/// `key` as the value of a `doneof/` row.
fn encode_done_of(key: &ActionKey) -> Vec<u8> {
    let mut row = vec![DONE_OF_VERSION];
    row.extend(key.action.size_bytes.to_be_bytes());
    row.extend(key.action.hash);
    row.extend(key.instance.as_bytes());
    row
}

/// The action a `doneof/` row holds, or what is wrong with the row.
fn decode_done_of(row: &[u8]) -> Result<ActionKey, &'static str> {
    let [DONE_OF_VERSION, rest @ ..] = row else {
        return Err("not a version 1 action row");
    };
    let cut = "an action row is cut short";
    let (size, rest) = rest.split_first_chunk::<8>().ok_or(cut)?;
    let (hash, instance) = rest.split_first_chunk::<32>().ok_or(cut)?;
    let instance = std::str::from_utf8(instance).map_err(|_| "an instance name is not UTF-8")?;
    Ok(ActionKey {
        instance: instance.to_owned(),
        action: Digest::new(DigestFunction::Sha256, *hash, u64::from_be_bytes(*size)),
    })
}

fn corrupt(key: &str, why: impl ToString) -> RowsError {
    RowsError::Corrupt {
        key: key.to_owned(),
        why: why.to_string(),
    }
}

/// The value of an `exec/op/` row.
fn encode(row: &OpRow) -> Vec<u8> {
    let mut value = vec![VERSION];
    // A struct of strings, numbers and booleans always serializes.
    value.extend(serde_json::to_vec(row).unwrap_or_default());
    value
}

/// The row `value` holds; `None` for a version this build does not know.
fn decode(key: &str, value: &[u8]) -> Result<Option<OpRow>, RowsError> {
    match value.split_first() {
        Some((&VERSION, json)) => serde_json::from_slice(json)
            .map(Some)
            .map_err(|e| corrupt(key, e)),
        Some(_) => Ok(None),
        None => Err(corrupt(key, "the row is empty")),
    }
}

/// The operation an `exec/op/` key names.
fn operation_of(key: &str) -> Option<OperationId> {
    let hex = key.strip_prefix(OP_PREFIX)?;
    (hex.len() == 16)
        .then(|| u64::from_str_radix(hex, 16).ok())
        .flatten()
        .map(OperationId)
}

impl Rows {
    /// The rows of the process `instance` over `store`.
    #[must_use]
    pub fn new(store: Arc<dyn Store>, instance: &str) -> Self {
        Self {
            store,
            instance: instance.to_owned(),
            ids: Mutex::new(0..0),
            unknown: AtomicU64::new(0),
        }
    }

    /// Rows skipped so far for a version byte this build does not know.
    #[must_use]
    pub fn unknown_rows(&self) -> u64 {
        self.unknown.load(Ordering::Relaxed)
    }

    fn commit(&self, txn: Txn) -> Result<(), RowsError> {
        match self.store.txn(txn)? {
            Ok(_) => Ok(()),
            Err(conflict) => Err(RowsError::Conflict { key: conflict.key }),
        }
    }

    /// The next operation id: from this process's block, or from a block reserved now with a
    /// compare-and-set on `meta/next_op`, so no two frontends of the store give one id.
    ///
    /// # Errors
    /// The store cannot be read or written, the counter is not a number, or other frontends won
    /// every try ([`RowsError::Conflict`]).
    pub fn next_id(&self) -> Result<OperationId, RowsError> {
        let mut ids = self.ids.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(id) = ids.next() {
            return Ok(OperationId(id));
        }
        let block = self.reserve()?;
        *ids = block;
        Ok(OperationId(ids.next().unwrap_or_default()))
    }

    fn reserve(&self) -> Result<Range<u64>, RowsError> {
        let mut tried = 1;
        loop {
            let (stored, cond) = match self.store.get(NEXT_OP)? {
                Some(row) => {
                    let bytes: [u8; 8] = row
                        .value
                        .as_slice()
                        .try_into()
                        .map_err(|_| corrupt(NEXT_OP, "not an 8-byte number"))?;
                    (u64::from_be_bytes(bytes), Cond::Version(row.version))
                }
                None => (0, Cond::Absent),
            };
            // Id 0 is never given: a row of an earlier release reads 0 as "none".
            let start = stored.max(1);
            let end = start.saturating_add(ID_BLOCK);
            match self.commit(Txn::new().put(NEXT_OP, end.to_be_bytes(), cond)) {
                Err(RowsError::Conflict { .. }) if tried < CAS_TRIES => tried += 1,
                Err(error) => return Err(error),
                Ok(()) => return Ok(start..end),
            }
        }
    }

    /// Renews this process's liveness row for `live`.
    ///
    /// # Errors
    /// The store cannot be written.
    pub fn beat(&self, live: Duration) -> Result<(), RowsError> {
        let key = format!("{LIVE_PREFIX}{}", self.instance);
        self.commit(Txn::new().put_ttl(key, Vec::new(), live, Cond::Any))
    }

    /// Deletes this process's liveness row: its operations may be taken up at once.
    ///
    /// # Errors
    /// The store cannot be written.
    pub fn leave(&self) -> Result<(), RowsError> {
        let key = format!("{LIVE_PREFIX}{}", self.instance);
        self.commit(Txn::new().delete(key, Cond::Any))
    }

    /// The condition a write of the row at `key` by this process holds to: that it is still
    /// absent, if it is, or at the version read, if this process owns it.
    ///
    /// # Errors
    /// [`RowsError::Conflict`] when another process owns the row (or it is of a version this
    /// build does not know); the store cannot be read, or the row is not what it says.
    fn ours(&self, key: &str) -> Result<Cond, RowsError> {
        let Some(found) = self.store.get(key)? else {
            return Ok(Cond::Absent);
        };
        match decode(key, &found.value)? {
            Some(row) if row.owner == self.instance => Ok(Cond::Version(found.version)),
            _ => Err(RowsError::Conflict {
                key: key.to_owned(),
            }),
        }
    }

    /// Writes `row` as the row of `operation`, which this process runs: a new row, or one this
    /// process owns.
    ///
    /// The row of a first attempt is written in one transaction, on the condition that there is
    /// none yet, which is so for a new operation; only when there is one is it read, and
    /// written over at the version read if this process owns it. A later attempt's row is read
    /// first, since this process wrote the attempt before it.
    ///
    /// # Errors
    /// [`RowsError::Conflict`] when another process owns the row; the store cannot be written.
    pub fn put_op(&self, operation: OperationId, row: &OpRow) -> Result<(), RowsError> {
        let key = op_key(operation);
        let row = OpRow {
            owner: self.instance.clone(),
            ..row.clone()
        };
        let value = encode(&row);
        if row.attempt == 0 {
            match self.commit(Txn::new().put(key.clone(), value.clone(), Cond::Absent)) {
                Err(RowsError::Conflict { .. }) => {}
                written => return written,
            }
        }
        let cond = self.ours(&key)?;
        self.commit(Txn::new().put(key, value, cond))
    }

    /// Finishes `operation` of `action` as `finished`: its row deleted, if this process owns it
    /// (or no row is left), and its answer and action put for `retention` (at least
    /// [`DONE_FLOOR`]). An answer another frontend wrote first is kept, and returned.
    ///
    /// # Errors
    /// [`RowsError::Conflict`] when another process owns the row; the store cannot be written.
    pub fn finish(
        &self,
        operation: OperationId,
        action: &ActionKey,
        finished: &Finished,
        retention: Duration,
    ) -> Result<Finished, RowsError> {
        let key = op_key(operation);
        let ttl = retention.max(DONE_FLOOR);
        let txn = match self.ours(&key)? {
            Cond::Version(version) => Txn::new().delete(key.clone(), Cond::Version(version)),
            // No row is left: the answer is put alone.
            _ => Txn::new(),
        };
        let txn = txn
            .put_ttl(done_key(operation), finished.to_row(), ttl, Cond::Absent)
            .put_ttl(
                done_of_key(operation),
                encode_done_of(action),
                ttl,
                Cond::Absent,
            );
        match self.commit(txn) {
            Err(RowsError::Conflict { key: lost }) if lost != key => {
                // Another frontend answered it first: its answer stands, and the row goes.
                let answer = self.done(operation)?.map(|(_, answer)| answer);
                let answer = answer.ok_or_else(|| corrupt(&lost, "the answer is gone"))?;
                self.commit(Txn::new().delete(key, Cond::Any))?;
                Ok(answer)
            }
            Err(error) => Err(error),
            Ok(()) => Ok(finished.clone()),
        }
    }

    /// How `operation` finished, with its action, if the store still has its answer.
    ///
    /// # Errors
    /// The store cannot be read, or a row is not what it says.
    pub fn done(&self, operation: OperationId) -> Result<Option<(ActionKey, Finished)>, RowsError> {
        let of_key = done_of_key(operation);
        let (Some(of), Some(done)) = (
            self.store.get(&of_key)?,
            self.store.get(&done_key(operation))?,
        ) else {
            return Ok(None);
        };
        let action = decode_done_of(&of.value).map_err(|why| corrupt(&of_key, why))?;
        let finished =
            Finished::from_row(&done.value).map_err(|why| corrupt(&done_key(operation), why))?;
        Ok(Some((action, finished)))
    }

    /// The row of the unfinished `operation`, if the store has one of a version this build
    /// reads.
    ///
    /// # Errors
    /// The store cannot be read, or the row is not what it says.
    pub fn op(&self, operation: OperationId) -> Result<Option<OpRow>, RowsError> {
        let key = op_key(operation);
        match self.store.get(&key)? {
            Some(found) => decode(&key, &found.value),
            None => Ok(None),
        }
    }

    /// Where `operation` stands. Its row is read before its answer: an operation finishes in
    /// one transaction that deletes the row and puts the answer, so a row not seen means the
    /// answer, if there is one, is there for the read that follows, and a finish between the
    /// two reads is never taken for an operation that is gone.
    ///
    /// # Errors
    /// The store cannot be read, or a row is not what it says.
    pub fn look(&self, operation: OperationId) -> Result<Look, RowsError> {
        if let Some(row) = self.op(operation)? {
            return Ok(Look::Open(row));
        }
        Ok(match self.done(operation)? {
            Some((action, finished)) => Look::Done(action, finished),
            None => Look::Gone,
        })
    }

    /// The unfinished operations of processes whose liveness row has expired, this process's
    /// own excepted. A row of a version this build does not know is skipped and counted, and a
    /// corrupt one is skipped and logged: neither stops the others being taken up.
    ///
    /// # Errors
    /// The store cannot be read.
    pub fn orphans(&self) -> Result<Vec<Orphan>, RowsError> {
        let mut orphans = Vec::new();
        let mut live = std::collections::BTreeMap::new();
        for (key, found) in self.store.scan_prefix(OP_PREFIX)? {
            let Some(operation) = operation_of(&key) else {
                continue;
            };
            let row = match decode(&key, &found.value) {
                Ok(Some(row)) => row,
                Ok(None) => {
                    self.unknown.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Err(error) => {
                    tracing::warn!(%error, "an operation row is skipped");
                    continue;
                }
            };
            if row.owner == self.instance {
                continue;
            }
            let alive = match live.get(&row.owner) {
                Some(alive) => *alive,
                None => {
                    let key = format!("{LIVE_PREFIX}{}", row.owner);
                    let alive = self.store.get(&key)?.is_some();
                    live.insert(row.owner.clone(), alive);
                    alive
                }
            };
            if !alive {
                orphans.push(Orphan {
                    operation,
                    row,
                    version: found.version,
                });
            }
        }
        Ok(orphans)
    }

    /// Takes `orphan` up: its row is this process's from now on, if its owner is still gone and
    /// no other process took it first. Returns the row as written.
    ///
    /// # Errors
    /// [`RowsError::Conflict`] when another process took it, or its owner came back; the store
    /// cannot be written.
    pub fn claim(&self, orphan: &Orphan) -> Result<OpRow, RowsError> {
        let row = OpRow {
            owner: self.instance.clone(),
            ..orphan.row.clone()
        };
        let owner_live = format!("{LIVE_PREFIX}{}", orphan.row.owner);
        let txn = Txn::new().check(owner_live, Cond::Absent).put(
            op_key(orphan.operation),
            encode(&row),
            Cond::Version(orphan.version),
        );
        self.commit(txn)?;
        Ok(row)
    }
}

#[cfg(test)]
#[path = "rows_tests.rs"]
mod tests;
