//! The encoding of the values of the `op/`, `lease/` and `session/` rows ([`RowCodec`]).
//!
//! The store layer decides which rows exist, what their keys are and when each is written;
//! the codec decides only what bytes a request, a lease grant and a daemon's registration
//! are as a row value. [`KbfLog`], the one implementation, writes the `kbf-log` encoding of
//! the scheduler's own input for each, so the codec the replicated log already runs decides
//! what a row may hold.

use kbf_caps::NodeCaps;
use kbf_log::FarmInput;
use kbf_sched::{DaemonInstance, Request};
use kbf_types::{ControlRecord, LeaseGrant, Resources, WorkerId};

use super::{StateError, encode, single};

/// What a daemon's registration says, as its `session/` row keeps it.
#[derive(Debug, Clone)]
pub(super) struct WorkerUp {
    /// The worker.
    pub(super) worker: WorkerId,
    /// The daemon process that registered it.
    pub(super) instance: DaemonInstance,
    /// The server found the daemon process before it stopped, its runs killed.
    pub(super) predecessor_stopped: bool,
    /// What placement may book on it.
    pub(super) capacity: Resources,
    /// What it offers.
    pub(super) caps: NodeCaps,
}

/// What a row value holds, as [`RowCodec::read`] finds it.
#[derive(Debug)]
pub(super) enum Value {
    /// A request (`op/`).
    Submit(Request),
    /// A lease grant (`lease/`).
    Lease(LeaseGrant),
    /// A daemon's registration (`session/`).
    Session(WorkerUp),
    /// A value the codec reads, but of a kind no row holds.
    Other,
    /// A value a later binary wrote: this one cannot read it. A scan skips such a row and
    /// logs it at a start; a row a write depends on is refused.
    Unknown,
}

/// The bytes of the values of the `op/`, `lease/` and `session/` rows, and the reading of
/// them back.
pub(super) trait RowCodec: Send + Sync {
    /// The value of the `op/` row of `request`.
    ///
    /// # Errors
    /// The request cannot be encoded.
    fn submit(&self, request: &Request) -> Result<Vec<u8>, StateError>;

    /// The value of the `lease/` row of `grant`.
    ///
    /// # Errors
    /// The grant cannot be encoded.
    fn lease(&self, grant: &LeaseGrant) -> Result<Vec<u8>, StateError>;

    /// The value of the `session/` row of a registration.
    ///
    /// # Errors
    /// The registration cannot be encoded.
    fn session(&self, up: &WorkerUp) -> Result<Vec<u8>, StateError>;

    /// What the row `key` holds in `bytes`. A value a later binary wrote is
    /// [`Value::Unknown`], not an error.
    ///
    /// # Errors
    /// [`StateError::Corrupt`], naming `key`, if the bytes are not a value of this codec.
    fn read(&self, key: &str, bytes: &[u8]) -> Result<Value, StateError>;
}

/// The `kbf-log` encoding: a row value is a `FarmInput` of the replicated log's own format.
#[derive(Debug, Clone, Copy)]
pub(super) struct KbfLog;

impl RowCodec for KbfLog {
    fn submit(&self, request: &Request) -> Result<Vec<u8>, StateError> {
        encode(FarmInput::Submit(request.clone()))
    }

    fn lease(&self, grant: &LeaseGrant) -> Result<Vec<u8>, StateError> {
        encode(FarmInput::Committed(ControlRecord::Lease(grant.clone())))
    }

    fn session(&self, up: &WorkerUp) -> Result<Vec<u8>, StateError> {
        encode(FarmInput::WorkerUp {
            worker: up.worker.clone(),
            instance: up.instance.clone(),
            predecessor_stopped: up.predecessor_stopped,
            capacity: up.capacity,
            caps: up.caps.clone(),
        })
    }

    fn read(&self, key: &str, bytes: &[u8]) -> Result<Value, StateError> {
        Ok(match single(key, bytes)? {
            None => Value::Unknown,
            Some(FarmInput::Submit(request)) => Value::Submit(request),
            Some(FarmInput::Committed(ControlRecord::Lease(grant))) => Value::Lease(grant),
            Some(FarmInput::WorkerUp {
                worker,
                instance,
                predecessor_stopped,
                capacity,
                caps,
            }) => Value::Session(WorkerUp {
                worker,
                instance,
                predecessor_stopped,
                capacity,
                caps,
            }),
            Some(_) => Value::Other,
        })
    }
}
