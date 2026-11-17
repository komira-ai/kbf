//! The `exec/` rows of the state store: who runs the executor's workers and jobs when several
//! frontends share one Postgres (`docs/design/n-frontends.md`, sections 4.2 to 4.4).
//!
//! Three row families under one prefix, [`ROOT`]. No scan of any other family reads a prefix
//! of it, so a release that does not know them never sees them.
//!
//! | Key | Value |
//! |---|---|
//! | `exec/fe/<instance>` | [`Liveness`]: this process is alive until the key expires on the store's clock |
//! | `exec/w/<worker>` | [`WorkerRow`]: which process holds a virtual worker |
//! | `exec/j/<worker>/<job id>` | [`JobRow`]: a job submitted for a lease of that worker |
//!
//! There is no leader and no lease on a role. Every call here is one transaction that is
//! correct whichever process runs it and however many run it at once:
//!
//! - The **version of the worker row is the fencing token** ([`Held`]). A write of a job row,
//!   a renewal of a holder's liveness row, a release and a retirement carry a check of the
//!   worker row at that version in the same transaction, so a holder that lost the worker
//!   writes nothing.
//! - A **claim** ([`ExecRows::claim`]) is one transaction: the owner's liveness row is absent
//!   (or the row names no owner, or the claimer owns it already), the claimer's own liveness
//!   row is the one it last wrote, and the worker row is put at the version read. One
//!   claimer wins; the others get a conflict and read again.
//! - A **job row** is inserted `Cond::Absent` before the submit and deleted after the farm
//!   acknowledged the result.
//!
//! Nothing here reads a clock: expiry of a liveness row is the store's, on its own clock.
//!
//! # Format
//!
//! A value is one version byte ([`VERSION`]) and a JSON object. A reader ignores the fields it
//! does not name, so a later release adds a field and an earlier one still reads the row. A
//! row whose version byte this build does not know was written by a later release: a scan
//! skips it and counts it ([`ExecRows::unknown_rows`]), and a single read finds none. A row
//! that has a known version and is not what it says is [`RowsError::Corrupt`].
//!
//! A row never holds a presigned URL, a read token or a file or environment value of a job.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_state::{Commit, Cond, Conflict, Store, StoreError, Txn, Version, Versioned};
use serde::Deserialize;
use serde_json::{Value, json};

/// The first key component of every row of this module. No scan prefix of any other family
/// of the state store is a prefix of it.
pub const ROOT: &str = "exec/";

/// The prefix of the liveness rows: `exec/fe/<instance>`.
pub const FRONTEND_PREFIX: &str = "exec/fe/";

/// The prefix of the worker rows: `exec/w/<worker>`.
pub const WORKER_PREFIX: &str = "exec/w/";

/// The prefix of the job rows: `exec/j/<worker>/<job id>`.
pub const JOB_PREFIX: &str = "exec/j/";

/// The version byte every value begins with.
pub const VERSION: u8 = 1;

/// Why a row call failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RowsError {
    /// The store itself failed (or its connection did); nothing is known of the write.
    #[error("exec rows: {0}")]
    Store(#[from] StoreError),
    /// A precondition of the transaction failed: the row named is not as the caller read it.
    /// What the key says: a worker row, the caller no longer holds the worker; a liveness
    /// row of another process, it is alive; the caller's own liveness row, it is not the
    /// last one the caller wrote (it expired); a job row, it is already there.
    #[error("exec rows: the precondition on {key:?} failed")]
    Conflict {
        /// The key whose precondition failed.
        key: String,
    },
    /// A name that cannot be part of a key: empty, or holding a `/`.
    #[error("exec rows: {name:?} cannot be part of a key")]
    BadName {
        /// The name refused.
        name: String,
    },
    /// A row of a version this build knows that is not a row of its kind.
    #[error("exec rows: the row {key:?} is corrupt: {why}")]
    Corrupt {
        /// The key of the row.
        key: String,
        /// What is wrong with it.
        why: String,
    },
}

impl RowsError {
    /// Whether this is a failed precondition on `key`.
    #[must_use]
    pub fn conflicts_on(&self, key: &str) -> bool {
        matches!(self, RowsError::Conflict { key: k } if k == key)
    }
}

/// What a process says about itself every beat: the value of `exec/fe/<instance>`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Liveness {
    /// The frontend group (`--executor-frontend-id`).
    pub group: String,
    /// When the process started, in milliseconds since the Unix epoch.
    pub started_ms: u64,
    /// The release the process runs.
    pub release: String,
    /// The process has begun to drain.
    pub draining: bool,
}

/// The shape of a virtual worker, kept in its row so an adopter registers one the leases
/// booked on it still fit.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Shape {
    /// Thousandths of a core the farm may book.
    pub cpu_millis: u64,
    /// Bytes of memory the farm may book.
    pub memory_bytes: u64,
    /// The most actions in flight.
    pub slots: u64,
    /// The CPU architecture of its jobs.
    pub arch: String,
}

/// The value of `exec/w/<worker>`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct WorkerRow {
    /// The process that holds the worker; empty once it released it.
    pub owner_instance: String,
    /// The daemon instance id the worker said `Hello` with, which an adopter names as the
    /// predecessor of its own.
    pub farm_instance: String,
    /// The worker's shape.
    pub shape: Shape,
}

/// The value of `exec/j/<worker>/<job id>`: what an adopter needs to finish a job.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct JobRow {
    /// The farm lease's term.
    pub lease_term: u64,
    /// The farm lease's sequence number.
    pub lease_seq: u64,
    /// The action's digest, as text.
    pub action: String,
    /// The pack key the job writes under (a name, never a credential).
    pub pack_key: String,
    /// When the job was submitted, in milliseconds since the Unix epoch.
    pub submitted_ms: u64,
}

impl Liveness {
    fn body(&self) -> Value {
        json!({
            "draining": self.draining,
            "group": self.group,
            "release": self.release,
            "started_ms": self.started_ms,
        })
    }
}

impl Shape {
    fn body(&self) -> Value {
        json!({
            "arch": self.arch,
            "cpu_millis": self.cpu_millis,
            "memory_bytes": self.memory_bytes,
            "slots": self.slots,
        })
    }
}

impl WorkerRow {
    fn body(&self) -> Value {
        json!({
            "farm_instance": self.farm_instance,
            "owner_instance": self.owner_instance,
            "shape": self.shape.body(),
        })
    }
}

impl JobRow {
    fn body(&self) -> Value {
        json!({
            "action": self.action,
            "lease_seq": self.lease_seq,
            "lease_term": self.lease_term,
            "pack_key": self.pack_key,
            "submitted_ms": self.submitted_ms,
        })
    }
}

/// The bytes of a row: the version byte and the JSON object.
fn encode(body: &Value) -> Vec<u8> {
    let mut bytes = vec![VERSION];
    bytes.extend_from_slice(body.to_string().as_bytes());
    bytes
}

fn corrupt(key: &str, why: impl ToString) -> RowsError {
    RowsError::Corrupt {
        key: key.to_owned(),
        why: why.to_string(),
    }
}

/// The key of the liveness row of a process.
///
/// # Errors
/// [`RowsError::BadName`] for an `instance` that is empty or holds a `/`.
pub fn frontend_key(instance: &str) -> Result<String, RowsError> {
    Ok(format!("{FRONTEND_PREFIX}{}", name(instance)?))
}

/// The key of the row of a worker.
///
/// # Errors
/// [`RowsError::BadName`] for a `worker` that is empty or holds a `/`.
pub fn worker_key(worker: &str) -> Result<String, RowsError> {
    Ok(format!("{WORKER_PREFIX}{}", name(worker)?))
}

/// The key of the row of a job of a worker.
///
/// # Errors
/// [`RowsError::BadName`] for a `worker` or `job` that is empty or holds a `/`.
pub fn job_key(worker: &str, job: &str) -> Result<String, RowsError> {
    Ok(format!("{}{}", job_prefix(worker)?, name(job)?))
}

/// The prefix that holds the job rows of one worker. It ends in `/`, so no other worker's
/// rows lie under it (a worker's name holds none).
///
/// # Errors
/// [`RowsError::BadName`] for a `worker` that is empty or holds a `/`.
pub fn job_prefix(worker: &str) -> Result<String, RowsError> {
    Ok(format!("{JOB_PREFIX}{}/", name(worker)?))
}

fn name(name: &str) -> Result<&str, RowsError> {
    if name.is_empty() || name.contains('/') {
        return Err(RowsError::BadName {
            name: name.to_owned(),
        });
    }
    Ok(name)
}

/// The proof that a process holds a worker: the version of the worker row it wrote. Every
/// write of the holder carries a check of the row at this version, so once another process
/// writes the row the check fails for good (a version is never reissued).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    /// The worker.
    pub worker: String,
    /// The version of the worker row the holder wrote.
    pub version: Version,
}

/// The proof that a process's liveness row is the last one it wrote. A claim checks it, so a
/// process whose row expired claims nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Live {
    /// The process.
    pub instance: String,
    /// The version of the liveness row the process wrote.
    pub version: Version,
}

/// A worker row as a scan read it: what a claim is made against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerEntry {
    /// The worker.
    pub worker: String,
    /// Its row.
    pub row: WorkerRow,
    /// The version the row had.
    pub version: Version,
}

/// A job row as a scan read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobEntry {
    /// The job id.
    pub job: String,
    /// Its row.
    pub row: JobRow,
}

/// What [`ExecRows::insert_job`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    /// The row is written.
    Written,
    /// The row was there already, from an earlier try whose answer was lost; the caller
    /// still holds the worker, and goes on.
    Present,
}

/// The `exec/` rows over a state store.
pub struct ExecRows {
    store: Arc<dyn Store>,
    unknown: AtomicU64,
}

impl ExecRows {
    /// The rows of `store`.
    #[must_use]
    pub fn new(store: Arc<dyn Store>) -> Self {
        ExecRows {
            store,
            unknown: AtomicU64::new(0),
        }
    }

    /// How many rows of a version this build does not know the reads of this handle have
    /// skipped.
    #[must_use]
    pub fn unknown_rows(&self) -> u64 {
        self.unknown.load(Ordering::Relaxed)
    }

    /// Commits `txn`, which one of the `_txn` functions built (or a caller of a test took
    /// guards out of).
    ///
    /// # Errors
    /// [`RowsError::Conflict`] when a precondition failed (nothing is written),
    /// [`RowsError::Store`] when the store failed.
    pub fn commit(&self, txn: Txn) -> Result<Commit, RowsError> {
        match self.store.txn(txn)? {
            Ok(commit) => Ok(commit),
            Err(Conflict { key, .. }) => Err(RowsError::Conflict { key }),
        }
    }

    // ---- liveness ----------------------------------------------------------------------

    /// The transaction of a beat: the liveness row of `instance` is put with a time to live
    /// of `ttl` (one writer, `Cond::Any`), after a check of each worker row of `held` at the
    /// version the holder wrote, so a process that lost a worker does not renew.
    ///
    /// # Errors
    /// [`RowsError::BadName`] for a name that is not a key part.
    pub fn beat_txn(
        instance: &str,
        row: &Liveness,
        ttl: Duration,
        held: &[Held],
    ) -> Result<Txn, RowsError> {
        let mut txn = Txn::new();
        for held in held {
            txn = txn.check(worker_key(&held.worker)?, Cond::Version(held.version));
        }
        Ok(txn.put_ttl(frontend_key(instance)?, encode(&row.body()), ttl, Cond::Any))
    }

    /// Renews the liveness row of `instance`.
    ///
    /// # Errors
    /// [`RowsError::Conflict`] on the key of a worker of `held` that the caller no longer
    /// holds (nothing is renewed), [`RowsError::BadName`], [`RowsError::Store`].
    pub fn beat(
        &self,
        instance: &str,
        row: &Liveness,
        ttl: Duration,
        held: &[Held],
    ) -> Result<Live, RowsError> {
        let commit = self.commit(Self::beat_txn(instance, row, ttl, held)?)?;
        Ok(Live {
            instance: instance.to_owned(),
            version: Version(commit.revision.0),
        })
    }

    /// Deletes the liveness row of `instance` (a process that ends says so).
    ///
    /// # Errors
    /// [`RowsError::BadName`], [`RowsError::Store`].
    pub fn leave(&self, instance: &str) -> Result<(), RowsError> {
        let txn = Txn::new().delete(frontend_key(instance)?, Cond::Any);
        self.commit(txn).map(drop)
    }

    /// The liveness row of `instance`; `None` when it is absent or expired, and when a later
    /// release wrote it (counted in [`ExecRows::unknown_rows`]).
    ///
    /// # Errors
    /// [`RowsError::Corrupt`], [`RowsError::BadName`], [`RowsError::Store`].
    pub fn liveness(&self, instance: &str) -> Result<Option<Liveness>, RowsError> {
        let key = frontend_key(instance)?;
        match self.store.get(&key)? {
            Some(row) => self.liveness_row(&key, &row),
            None => Ok(None),
        }
    }

    /// Every live process and what it says, in key order.
    ///
    /// # Errors
    /// [`RowsError::Corrupt`], [`RowsError::Store`].
    pub fn live_instances(&self) -> Result<Vec<(String, Liveness)>, RowsError> {
        let mut out = Vec::new();
        for (key, row) in self.store.scan_prefix(FRONTEND_PREFIX)? {
            if let Some(liveness) = self.liveness_row(&key, &row)? {
                out.push((key[FRONTEND_PREFIX.len()..].to_owned(), liveness));
            }
        }
        Ok(out)
    }

    fn liveness_row(&self, key: &str, row: &Versioned) -> Result<Option<Liveness>, RowsError> {
        let Some(body) = self.body(key, row)? else {
            return Ok(None);
        };
        serde_json::from_slice(body)
            .map(Some)
            .map_err(|e| corrupt(key, e))
    }

    // ---- workers -----------------------------------------------------------------------

    /// Writes the row of a worker nobody has a row for: the first holder of a worker.
    ///
    /// # Errors
    /// [`RowsError::Conflict`] when the row exists, [`RowsError::BadName`], [`RowsError::Store`].
    pub fn create_worker(&self, worker: &str, row: &WorkerRow) -> Result<Held, RowsError> {
        let txn = Txn::new().put(worker_key(worker)?, encode(&row.body()), Cond::Absent);
        let commit = self.commit(txn)?;
        Ok(held_by(worker, &commit))
    }

    /// Every worker row, in key order. A row of a later release is skipped and counted.
    ///
    /// # Errors
    /// [`RowsError::Corrupt`], [`RowsError::Store`].
    pub fn workers(&self) -> Result<Vec<WorkerEntry>, RowsError> {
        let mut out = Vec::new();
        for (key, stored) in self.store.scan_prefix(WORKER_PREFIX)? {
            let Some(body) = self.body(&key, &stored)? else {
                continue;
            };
            let row = serde_json::from_slice(body).map_err(|e| corrupt(&key, e))?;
            out.push(WorkerEntry {
                worker: key[WORKER_PREFIX.len()..].to_owned(),
                row,
                version: stored.version,
            });
        }
        Ok(out)
    }

    /// The transaction of a claim of the worker `entry` read, by the process `me`: the row is
    /// put at the version read with `me` as its owner and `farm_instance` as the instance the
    /// worker will say `Hello` with. It checks that the owner's liveness row is absent
    /// unless the row names no owner or names `me`, and that `me`'s own liveness row is the
    /// one it last wrote.
    ///
    /// # Errors
    /// [`RowsError::BadName`].
    pub fn claim_txn(
        me: &Live,
        entry: &WorkerEntry,
        farm_instance: &str,
    ) -> Result<Txn, RowsError> {
        let mut txn = Txn::new();
        let owner = entry.row.owner_instance.as_str();
        if !owner.is_empty() && owner != me.instance {
            txn = txn.check(frontend_key(owner)?, Cond::Absent);
        }
        let row = WorkerRow {
            owner_instance: me.instance.clone(),
            farm_instance: farm_instance.to_owned(),
            shape: entry.row.shape.clone(),
        };
        Ok(txn
            .check(frontend_key(&me.instance)?, Cond::Version(me.version))
            .put(
                worker_key(&entry.worker)?,
                encode(&row.body()),
                Cond::Version(entry.version),
            ))
    }

    /// Claims the worker `entry` read: one transaction, so of several claimers one wins and
    /// the others get [`RowsError::Conflict`] and read again.
    ///
    /// # Errors
    /// [`RowsError::Conflict`] (on the owner's liveness row, the owner is alive; on the
    /// worker row, another process wrote it since the read; on `me`'s liveness row, `me` is
    /// not alive), [`RowsError::BadName`], [`RowsError::Store`].
    pub fn claim(
        &self,
        me: &Live,
        entry: &WorkerEntry,
        farm_instance: &str,
    ) -> Result<Held, RowsError> {
        let commit = self.commit(Self::claim_txn(me, entry, farm_instance)?)?;
        Ok(held_by(&entry.worker, &commit))
    }

    /// The transaction of a release: the worker row is put with no owner, at the version the
    /// holder wrote.
    ///
    /// # Errors
    /// [`RowsError::BadName`].
    pub fn release_txn(held: &Held, row: &WorkerRow) -> Result<Txn, RowsError> {
        let row = WorkerRow {
            owner_instance: String::new(),
            ..row.clone()
        };
        Ok(Txn::new().put(
            worker_key(&held.worker)?,
            encode(&row.body()),
            Cond::Version(held.version),
        ))
    }

    /// Hands the worker back (a hard stop): any live process may claim it at once. `row` is
    /// what the holder last wrote.
    ///
    /// # Errors
    /// [`RowsError::Conflict`] when the caller no longer holds the worker,
    /// [`RowsError::BadName`], [`RowsError::Store`].
    pub fn release(&self, held: &Held, row: &WorkerRow) -> Result<(), RowsError> {
        self.commit(Self::release_txn(held, row)?).map(drop)
    }

    /// Deletes the row of a worker whose jobs are all done. The caller has seen
    /// [`ExecRows::jobs`] empty; a job row inserted after is not found by anyone.
    ///
    /// # Errors
    /// [`RowsError::Conflict`] when the caller no longer holds the worker,
    /// [`RowsError::BadName`], [`RowsError::Store`].
    pub fn retire(&self, held: &Held) -> Result<(), RowsError> {
        let txn = Txn::new().delete(worker_key(&held.worker)?, Cond::Version(held.version));
        self.commit(txn).map(drop)
    }

    // ---- jobs --------------------------------------------------------------------------

    /// The transaction of a job row insert: a check of the worker row at the holder's
    /// version, and the row put `Cond::Absent`.
    ///
    /// # Errors
    /// [`RowsError::BadName`].
    pub fn insert_job_txn(held: &Held, job: &str, row: &JobRow) -> Result<Txn, RowsError> {
        Ok(Txn::new()
            .check(worker_key(&held.worker)?, Cond::Version(held.version))
            .put(
                job_key(&held.worker, job)?,
                encode(&row.body()),
                Cond::Absent,
            ))
    }

    /// Writes the row of `job` before it is submitted. A row that is there already (an
    /// earlier try whose answer was lost) is [`Insert::Present`], provided the caller
    /// still holds the worker.
    ///
    /// # Errors
    /// [`RowsError::Conflict`] on the worker row when the caller no longer holds the worker
    /// (it submits nothing), [`RowsError::BadName`], [`RowsError::Store`].
    pub fn insert_job(&self, held: &Held, job: &str, row: &JobRow) -> Result<Insert, RowsError> {
        match self.commit(Self::insert_job_txn(held, job, row)?) {
            Ok(_) => Ok(Insert::Written),
            Err(e) if e.conflicts_on(&job_key(&held.worker, job)?) => {
                // The store names one failed precondition; the worker row may have failed
                // too, and the holder must not go on then.
                let key = worker_key(&held.worker)?;
                match self.store.get(&key)? {
                    Some(row) if row.version == held.version => Ok(Insert::Present),
                    _ => Err(RowsError::Conflict { key }),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// The transaction of a job row delete: a check of the worker row at the holder's
    /// version, and the delete (of an absent row, nothing).
    ///
    /// # Errors
    /// [`RowsError::BadName`].
    pub fn delete_job_txn(held: &Held, job: &str) -> Result<Txn, RowsError> {
        Ok(Txn::new()
            .check(worker_key(&held.worker)?, Cond::Version(held.version))
            .delete(job_key(&held.worker, job)?, Cond::Any))
    }

    /// Deletes the row of `job`, after the farm acknowledged its result.
    ///
    /// # Errors
    /// [`RowsError::Conflict`] when the caller no longer holds the worker,
    /// [`RowsError::BadName`], [`RowsError::Store`].
    pub fn delete_job(&self, held: &Held, job: &str) -> Result<(), RowsError> {
        self.commit(Self::delete_job_txn(held, job)?).map(drop)
    }

    /// The job rows of a worker, in key order. A row of a later release is skipped and
    /// counted.
    ///
    /// # Errors
    /// [`RowsError::Corrupt`], [`RowsError::BadName`], [`RowsError::Store`].
    pub fn jobs(&self, worker: &str) -> Result<Vec<JobEntry>, RowsError> {
        let prefix = job_prefix(worker)?;
        let mut out = Vec::new();
        for (key, stored) in self.store.scan_prefix(&prefix)? {
            let Some(body) = self.body(&key, &stored)? else {
                continue;
            };
            let row = serde_json::from_slice(body).map_err(|e| corrupt(&key, e))?;
            out.push(JobEntry {
                job: key[prefix.len()..].to_owned(),
                row,
            });
        }
        Ok(out)
    }

    /// The JSON of a row's value: `None` when its version byte is not [`VERSION`] (counted),
    /// an error when it is empty.
    fn body<'a>(&self, key: &str, stored: &'a Versioned) -> Result<Option<&'a [u8]>, RowsError> {
        match stored.value.split_first() {
            None => Err(corrupt(key, "no version byte")),
            Some((&VERSION, body)) => Ok(Some(body)),
            Some(_) => {
                self.unknown.fetch_add(1, Ordering::Relaxed);
                Ok(None)
            }
        }
    }
}

/// The token of the holder whose write of the worker row `commit` is. The transactions that
/// make one have a single write, so the revision they return is that row's version.
fn held_by(worker: &str, commit: &Commit) -> Held {
    Held {
        worker: worker.to_owned(),
        version: Version(commit.revision.0),
    }
}

#[cfg(test)]
#[path = "rows_tests.rs"]
mod tests;
