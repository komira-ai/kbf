//! What the executor tells the invocation recorder (`docs/design/invocations.md`): the Execute
//! call answered with its operation, each run of the operation as an attempt (granted when its
//! job is submitted, started when the job is read back running, requeued when an attempt after
//! it is submitted), and the operation's end. An attempt is named in the record's lease fields
//! by its operation and its number, and its worker field is its job's id.
//!
//! Only operations a recorded call waits on are told: those an Execute call with an invocation
//! id opened, and none while the recorder is off.

use kbf_front::{ExecuteCall, Finished, Recorder, listing, wall_now};
use kbf_invocation::{Event, LeaseEnd, Outcome as Answered, RequeueReason};
use kbf_types::{Failure, LeaseId, OperationId, WorkerId};
use tonic::Code;

use crate::executor::task::RunId;

/// Sends the events of an Execute call answered with `name` on `operation` (`joined` if it
/// joined one already open). True when the call is recorded: the operation's events follow.
pub(crate) fn answered(
    recorder: &Recorder,
    call: Option<ExecuteCall>,
    name: &str,
    operation: OperationId,
    joined: bool,
) -> bool {
    let Some(call) = call else {
        return false;
    };
    for event in call.answered(name.to_owned(), operation, joined) {
        recorder.emit(event);
    }
    true
}

/// The record's name of `run`.
fn lease(run: RunId) -> LeaseId {
    LeaseId::new(run.operation.0, u64::from(run.attempt))
}

/// `run`'s job `job` was submitted.
pub(crate) fn granted(recorder: &Recorder, run: RunId, job: &str) {
    recorder.emit(Event::Granted {
        operation: run.operation,
        lease: lease(run),
        worker: WorkerId::new(job),
        at: wall_now(),
    });
}

/// `run`'s job was read back running.
pub(crate) fn started(recorder: &Recorder, run: RunId) {
    recorder.emit(Event::Started {
        operation: run.operation,
        lease: lease(run),
        at: wall_now(),
    });
}

/// `run` ended for a reason of the farm's, and the operation's next attempt is submitted.
pub(crate) fn requeued(recorder: &Recorder, run: RunId) {
    recorder.emit(Event::Requeued {
        operation: run.operation,
        lease: lease(run),
        reason: RequeueReason::Infra,
        at: wall_now(),
    });
}

/// How a run that answered its operation ended, for the record: accepted when it ran, else
/// the failure its answer's code says, with the answer's message.
fn end_of(finished: &Finished) -> LeaseEnd {
    let failed = |status: &kbf_proto::google::rpc::Status| {
        let failure = match Code::from_i32(status.code) {
            Code::DeadlineExceeded => Failure::Timeout,
            Code::InvalidArgument => Failure::Invalid,
            Code::FailedPrecondition => Failure::OutOfMemory,
            _ => Failure::Infra,
        };
        LeaseEnd::Failed(failure, status.message.clone())
    };
    match finished {
        Finished::Ran(_) => LeaseEnd::Accepted,
        Finished::Failed(status) | Finished::TimedOut { status, .. } => failed(status),
    }
}

/// What callers told `finished` were answered, as a row records it.
fn outcome_of(finished: &Finished) -> Answered {
    match finished {
        Finished::Ran(result) => Answered::Ok {
            exit_code: result.exit_code,
        },
        Finished::TimedOut { .. } => Answered::TimedOut,
        Finished::Failed(status) if status.code == Code::DeadlineExceeded as i32 => {
            Answered::TimedOut
        }
        Finished::Failed(status) => Answered::Failed {
            code: status.code,
            message: status.message.clone(),
        },
    }
}

/// The operation of `run` ended, its callers answered `finished`; `run` is `None` when no run
/// of it answered (another frontend's did, or none was submitted).
pub(crate) fn finished(
    recorder: &Recorder,
    operation: OperationId,
    run: Option<RunId>,
    finished: &Finished,
) {
    let result = match finished {
        Finished::Ran(result) => Some(listing(None, result)),
        Finished::TimedOut { partial, .. } => Some(listing(None, partial)),
        Finished::Failed(_) => None,
    };
    recorder.emit(Event::Finished(kbf_invocation::Finished {
        operation,
        lease: run.map(|run| (lease(run), end_of(finished))),
        outcome: outcome_of(finished),
        result,
        at: wall_now(),
    }));
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
