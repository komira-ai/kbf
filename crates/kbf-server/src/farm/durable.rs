//! The scheduler's state in the state store (`docs/design/architecture-v2.md`, sections
//! 4.2 and 4.4): every change is written through [`kbf_state::Store`] before it is
//! acknowledged, and a scheduler that starts over a store takes up what the store holds.
//!
//! Rows:
//!
//! - `op/<id>`: an unfinished operation's request, written at submit, deleted when it
//!   finishes or is refused;
//! - `queue/<id>`: present while the operation waits for a lease. A grant requires it
//!   and deletes it, so one operation never holds two live leases;
//! - `lease/<lease>`: the fencing token of a committed grant, present while the
//!   operation holds the lease. A result is accepted only while it is there, which is the
//!   compare-and-set on the operation's lease;
//! - `session/<worker>`: what the scheduler knows of a daemon's newest session (its
//!   instance, capacity and capabilities);
//! - `meta/epoch`, `meta/next_op`, `meta/next_seq`: the store incarnation and the first
//!   operation id and lease sequence no earlier start used.
//!
//! Values are the `kbf-log` encoding of the scheduler's own inputs (a `Submit`, the
//! `Committed` lease, a `WorkerUp`), so the strict codec the replicated log already runs
//! decides what a row may hold.

#![allow(dead_code, unused_imports)]

use std::sync::{Arc, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use kbf_front::{MetaLog, Stage};
use kbf_log::{FarmBatch, FarmInput, Format, LogCommand};
use kbf_sched::{DaemonInstance, Recovered, RecoveredLease, Request, Requeue};
use kbf_state::{Cond, Conflict, Store, StoreError, Txn, Versioned};
use kbf_types::{
    ControlRecord, Effect, FarmTime, LeaseGrant, LeaseId, OperationId, StartLease, WaiterId,
    WorkerId,
};

use kbf_objstore::ObjectStore;
use tokio::sync::watch;

use super::{Farm, FarmMachine, Reported, Sent, Stamp, State, Waiter, process_term};

const EPOCH: &str = "meta/epoch";
const NEXT_OP: &str = "meta/next_op";
const NEXT_SEQ: &str = "meta/next_seq";

/// What a start does with the store's epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EpochPolicy {
    /// Keep it: the scheduler resumes the store's incarnation, and a daemon that
    /// reconnects keeps the runs of its leases.
    Keep,
    /// Take the next one: the start is a new incarnation, and a daemon that sees it
    /// kills the runs of the leases it held before.
    Bump,
}

/// Why the state store could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// The backend failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A row changed under the scheduler.
    #[error("a row changed under the scheduler: {0}")]
    Conflict(#[from] Conflict),
    /// The row a write depends on is not there: the write is stale (a result of a lease
    /// the operation no longer holds, a grant of an operation that is not queued).
    #[error("the state store has no {0}: the write is stale")]
    Missing(String),
    /// A row cannot be read.
    #[error("the state store row {key} cannot be read: {why}")]
    Corrupt {
        /// The row.
        key: String,
        /// What is wrong with it.
        why: String,
    },
    /// A value cannot be encoded.
    #[error("a state store value cannot be encoded: {0}")]
    Encode(#[from] kbf_log::EncodeError),
    /// This build has no backend for the store's scheme.
    #[error("this build has no {0} state store backend")]
    NoBackend(&'static str),
}

/// What a store holds of the scheduler, as a start reads it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Stored {
    /// The first operation id no earlier start used.
    pub next_operation: u64,
    /// The first lease sequence no earlier start used.
    pub next_seq: u64,
    /// Every unfinished operation, in id order.
    pub operations: Vec<StoredOperation>,
}

/// An unfinished operation of a store.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct StoredOperation {
    pub operation: OperationId,
    pub request: Request,
    /// The committed lease it holds.
    pub lease: Option<LeaseGrant>,
}

impl StoredOperation {
    /// This operation as the scheduler takes it up, held for `waiter`.
    pub(super) fn recovered(self, waiter: WaiterId) -> Recovered {
        Recovered {
            operation: self.operation,
            request: self.request,
            waiter,
            lease: self.lease.map(|grant| RecoveredLease {
                lease: grant.lease,
                worker: grant.worker,
            }),
        }
    }
}

/// The scheduler's handle on its state store.
pub(super) struct Durable {
    store: Arc<dyn Store>,
}

impl std::fmt::Debug for Durable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Durable")
    }
}

fn op_key(operation: OperationId) -> String {
    format!("op/{:016x}", operation.0)
}

fn queue_key(operation: OperationId) -> String {
    format!("queue/{:016x}", operation.0)
}

fn lease_key(lease: LeaseId) -> String {
    format!("lease/{:016x}.{:016x}", lease.term, lease.seq)
}

fn corrupt(key: &str, why: &str) -> StateError {
    StateError::Corrupt {
        key: key.to_owned(),
        why: why.to_owned(),
    }
}

fn encode(input: FarmInput) -> Result<Vec<u8>, StateError> {
    let batch = FarmBatch {
        now: FarmTime::default(),
        wall: UNIX_EPOCH,
        inputs: vec![input],
    };
    Ok(kbf_log::encode(Format::V1, &LogCommand::Farm(batch))?)
}

fn decode(key: &str, bytes: &[u8]) -> Result<FarmInput, StateError> {
    match kbf_log::decode(Format::V1, bytes) {
        Ok(LogCommand::Farm(FarmBatch { mut inputs, .. })) if inputs.len() == 1 => {
            Ok(inputs.remove(0))
        }
        Ok(_) => Err(corrupt(key, "not a single scheduler input")),
        Err(e) => Err(corrupt(key, &e.to_string())),
    }
}

fn number(key: &str, row: &Versioned) -> Result<u64, StateError> {
    let bytes = <[u8; 8]>::try_from(row.value.as_slice())
        .map_err(|_| corrupt(key, "not an 8-byte number"))?;
    Ok(u64::from_be_bytes(bytes))
}

impl Durable {
    // Stubs: this change's tests fail against these, which write and read nothing.
    pub(super) fn open(
        store: Arc<dyn Store>,
        _policy: EpochPolicy,
        first: u64,
    ) -> Result<(Self, u64), StateError> {
        Ok((Self { store }, first))
    }

    pub(super) fn load(&self) -> Result<Stored, StateError> {
        let _ = &self.store;
        Ok(Stored {
            next_operation: 0,
            next_seq: 0,
            operations: Vec::new(),
        })
    }

    pub(super) fn submitted(&self, _: OperationId, _: &Request) -> Result<(), StateError> {
        Ok(())
    }

    pub(super) fn requeued(&self, _: &Requeue) -> Result<(), StateError> {
        Ok(())
    }

    pub(super) fn persist(&self, _: &[Effect]) -> Result<(), StateError> {
        Ok(())
    }

    pub(super) fn session(
        &self,
        _: &WorkerId,
        _: &DaemonInstance,
        _: bool,
        _: &Reported,
    ) -> Result<(), StateError> {
        Ok(())
    }
}

impl<M: MetaLog, O: ObjectStore> Farm<M, O> {
    /// This farm, with `store` as the truth the scheduler keeps its state in: every
    /// operation, lease grant, result and refusal is written to it before it is
    /// acknowledged, and the unfinished operations and leases it already holds are taken
    /// up again (see [`kbf_sched::Scheduler::recover`]; the callers they had are gone,
    /// and a client that asks for the action again joins the operation).
    ///
    /// The term and the lease epoch `Welcome` names are the store's epoch, which
    /// `epoch` decides: kept, a daemon that reconnects keeps the runs of its leases;
    /// bumped, it kills them. Call it before the farm serves anything.
    ///
    /// # Errors
    /// The store cannot be read, or holds a row that is not one the scheduler wrote.
    pub fn with_state_store(
        mut self,
        store: Arc<dyn Store>,
        epoch: EpochPolicy,
    ) -> Result<Self, StateError> {
        // Stub: the store is opened and nothing is replayed.
        let (durable, epoch) = Durable::open(store, epoch, process_term())?;
        let _stored = durable.load()?;
        self.term = epoch;
        self.lease_epoch = epoch;
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        state.durable = Some(durable);
        Ok(self)
    }
}

impl State {
    /// Runs `write` on the state store, if the farm has one. Whether it took: a
    /// refusal is logged here, and the caller does not acknowledge what depends on it.
    pub(super) fn durably(
        &self,
        what: &str,
        write: impl FnOnce(&Durable) -> Result<(), StateError>,
    ) -> bool {
        let Some(durable) = &self.durable else {
            return true;
        };
        match write(durable) {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(%error, "the state store did not take {what}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests;
