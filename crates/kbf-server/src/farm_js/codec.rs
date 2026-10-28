//! The values of the `op/`, `lease/` and `session/` rows ([`RowCodec`]) in the `kbf-log`
//! encoding: a row value is a one-input batch of the replicated log's own format, so the codec
//! the log already runs decides what a row may hold, and the bytes are the ones the server's
//! store layer has always written.
//!
//! A lease row also carries the cost admission charged the lease (field 4 of the log's lease
//! record, [`LeaseCost`]), so that a scheduler recovered from the rows charges the lease the
//! same. A cost that is the default is left out, and a row without one reads as
//! [`Value::LegacyLease`]: a row an older binary wrote, or a lease built by hand. Such a lease
//! is recovered with no cost, and the scheduler charges it its request. Rows written before
//! the cost existed also read `retry = false`.

use std::time::UNIX_EPOCH;

use js_store::{RowCodec, StateError, Value, WorkerUp};
use kbf_log::{FarmBatch, FarmInput, Format, LogCommand};
use kbf_sched::Request;
use kbf_types::sched::usage::LeaseCost;
use kbf_types::{ControlRecord, FarmTime, LeaseGrant};

/// The `kbf-log` encoding: a row value is a `FarmInput` of the replicated log's own format.
#[derive(Debug, Clone, Copy, Default)]
pub struct KbfRowCodec;

/// `input` as the `kbf-log` encoding of a one-input batch: the bytes of a row value.
fn encode(input: FarmInput) -> Result<Vec<u8>, StateError> {
    let batch = FarmBatch {
        now: FarmTime::default(),
        wall: UNIX_EPOCH,
        inputs: vec![input],
    };
    kbf_log::encode(Format::V1, &LogCommand::Farm(batch))
        .map_err(|error| StateError::Codec(error.to_string()))
}

fn corrupt(key: &str, why: &str) -> StateError {
    StateError::Corrupt {
        key: key.to_owned(),
        why: why.to_owned(),
    }
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
        Err(error) => Err(corrupt(key, &error.to_string())),
    }
}

impl RowCodec for KbfRowCodec {
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
            Some(FarmInput::Committed(ControlRecord::Lease(grant))) => {
                if grant.cost == LeaseCost::default() {
                    Value::LegacyLease(grant)
                } else {
                    Value::Lease(grant)
                }
            }
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
