//! The encoding of the values of the `op/`, `lease/` and `session/` rows ([`RowCodec`]).
//!
//! The store layer decides which rows exist, what their keys are and when each is written;
//! the codec decides only what bytes a request, a lease grant and a daemon's registration
//! are as a row value. [`KbfLog`], the one implementation, writes the `kbf-log` encoding of
//! the scheduler's own input for each, so the codec the replicated log already runs decides
//! what a row may hold.

use std::time::UNIX_EPOCH;

use kbf_caps::NodeCaps;
use kbf_log::{FarmBatch, FarmInput, Format, LogCommand};
use kbf_sched::{DaemonInstance, Request};
use kbf_state::Versioned;
use kbf_types::{ControlRecord, FarmTime, LeaseGrant, Resources, WorkerId};

use super::{Durable, StateError, corrupt};

/// `input` as the `kbf-log` encoding of a one-input batch: the bytes of a row value in
/// [`KbfLog`].
pub(super) fn encode(input: FarmInput) -> Result<Vec<u8>, StateError> {
    let batch = FarmBatch {
        now: FarmTime::default(),
        wall: UNIX_EPOCH,
        inputs: vec![input],
    };
    Ok(kbf_log::encode(Format::V1, &LogCommand::Farm(batch))?)
}

/// The one scheduler input `bytes` hold, or `None` for a command this binary does not know,
/// which a later one wrote.
fn single(key: &str, bytes: &[u8]) -> Result<Option<FarmInput>, StateError> {
    match kbf_log::decode(Format::V1, bytes) {
        Ok(LogCommand::Farm(FarmBatch { mut inputs, .. })) if inputs.len() == 1 => {
            Ok(Some(inputs.remove(0)))
        }
        Ok(LogCommand::Unknown) => Ok(None),
        Ok(_) => Err(corrupt(key, "not a single scheduler input")),
        Err(e) => Err(corrupt(key, &e.to_string())),
    }
}

/// The input a row holds, refused if the command is one this binary does not know.
#[cfg(test)]
pub(super) fn decode(key: &str, bytes: &[u8]) -> Result<FarmInput, StateError> {
    // A command this build does not know has no text of its own from the decoder.
    single(key, bytes)?.ok_or_else(|| not_known(key))
}

fn not_known(key: &str) -> StateError {
    corrupt(key, "a command this binary does not know")
}

/// Why `value`, which a row holds, is not the `what` the reader wants.
fn wrong(key: &str, value: &Value, what: &str) -> StateError {
    if matches!(value, Value::Unknown) {
        not_known(key)
    } else {
        corrupt(key, what)
    }
}

impl Durable {
    /// Whether `bytes`, the value of the row `key`, hold a command this binary does not
    /// know, which a later one wrote. A value that is malformed is not: a scan leaves it
    /// for the reader to refuse. It reads the value as any reader does, which costs what
    /// the decode of each row cost before the codec was a trait.
    pub(super) fn is_unknown(&self, key: &str, bytes: &[u8]) -> bool {
        matches!(self.codec.read(key, bytes), Ok(Value::Unknown))
    }

    /// The grant a `lease/` row holds.
    pub(super) fn lease_row(&self, key: &str, row: &Versioned) -> Result<LeaseGrant, StateError> {
        match self.codec.read(key, &row.value)? {
            Value::Lease(grant) => Ok(grant),
            other => Err(wrong(key, &other, "not a lease grant")),
        }
    }

    /// The request an `op/` row holds.
    pub(super) fn request_row(&self, key: &str, row: &Versioned) -> Result<Request, StateError> {
        match self.codec.read(key, &row.value)? {
            Value::Submit(request) => Ok(request),
            other => Err(wrong(key, &other, "not a request")),
        }
    }

    /// The registration a `session/` row holds.
    pub(super) fn session_row(&self, key: &str, row: &Versioned) -> Result<WorkerUp, StateError> {
        match self.codec.read(key, &row.value)? {
            Value::Session(up) => Ok(up),
            other => Err(wrong(key, &other, "not a daemon registration")),
        }
    }
}

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
