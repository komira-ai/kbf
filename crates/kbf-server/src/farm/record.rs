//! What the farm tells the invocation recorder (`docs/design/invocations.md`, section
//! 3.4): each lease granted, started and given up, and each operation's end, with
//! the lease that ended it and, for a failure, the daemon's reason. Every event is
//! sent under the farm's lock in the order the scheduler made the change, and after
//! the Execute call that queued or joined the operation, which [`Dispatch::submit`]
//! sends before it queues anything.
//!
//! Only operations a recorded call waits on are told: those [`State::record_submit`]
//! noted, which a call without an invocation id, or any call with the recorder off,
//! never is. For any other operation each seam costs a set lookup, and no event or
//! listing is built.
//!
//! One thing is told of every operation that ran, recorded or not: the `action usage:`
//! log line ([`usage_line`]) of a result that carries a `ResourceUsage`, keyed by the
//! action's mnemonic and target when a recorded call named them.
//!
//! [`Dispatch::submit`]: kbf_front::Dispatch::submit

use kbf_front::{ActionLabel, ExecuteCall, Finished, wall_now};
use kbf_invocation::{Event, LeaseEnd, Outcome as Answered};
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::ResourceUsage;
use kbf_sched::Requeue;
use kbf_types::{Answer, Digest, LeaseGrant, OperationId, Outcome, Refusal, StartLease};
use tonic::Code;

use super::State;

impl State {
    /// Sends the events of an Execute call answered with `name` on the scheduler's
    /// operation that a submission of `request` gets: the twin it joins, or the next
    /// new one, and notes the operation as recorded. Call it before the request is
    /// fed, so the call comes first.
    pub(super) fn record_submit(
        &mut self,
        call: ExecuteCall,
        name: &str,
        request: &kbf_sched::Request,
    ) {
        let twin = self.sched.twin(request);
        let operation = twin.unwrap_or_else(|| self.sched.next_operation());
        // A twin keeps the label of the call that queued it.
        let label = call.label();
        self.recorded.entry(operation).or_insert(label);
        for event in call.answered(name.to_owned(), operation, twin.is_some()) {
            self.recorder.emit(event);
        }
    }

    /// Whether a recorded call waits on `operation`.
    fn is_recorded(&self, operation: OperationId) -> bool {
        self.recorded.contains_key(&operation)
    }

    pub(super) fn record_granted(&self, grant: &LeaseGrant) {
        if !self.is_recorded(grant.operation) {
            return;
        }
        self.recorder.emit(Event::Granted {
            operation: grant.operation,
            lease: grant.lease,
            worker: grant.worker.clone(),
            at: wall_now(),
        });
    }

    pub(super) fn record_started(&self, start: &StartLease) {
        if !self.is_recorded(start.operation) {
            return;
        }
        self.recorder.emit(Event::Started {
            operation: start.operation,
            lease: start.lease,
            at: wall_now(),
        });
    }

    pub(super) fn record_requeue(&self, requeue: &Requeue) {
        if !self.is_recorded(requeue.operation) {
            return;
        }
        self.recorder.emit(Event::Requeued {
            operation: requeue.operation,
            lease: requeue.lease,
            reason: requeue.reason,
            at: wall_now(),
        });
    }

    /// Sends the end of `answer`'s operation: its callers were told `finished`; the
    /// listing of its result, stored as `stored`, if it ran; and `reason`, the
    /// daemon's text, if its lease reported a failure.
    pub(super) fn record_finished(
        &mut self,
        answer: &Answer,
        finished: &Finished,
        stored: Option<Digest>,
        reason: Option<String>,
    ) {
        let label = self.recorded.remove(&answer.operation);
        // One line per run that reported what it used, for any operation: a call
        // without an invocation id is not recorded, and has no label.
        if let Finished::Ran(result) = finished
            && let Some(line) = usage_line(label.as_ref(), result)
        {
            tracing::info!(operation = %answer.operation, "action usage: {line}");
        }
        if label.is_none() {
            return;
        }
        let listing = match finished {
            Finished::Ran(result) => stored.map(|digest| kbf_front::listing(digest, result)),
            _ => None,
        };
        let end = match answer.outcome {
            Outcome::Completed { .. } => LeaseEnd::Accepted,
            Outcome::Failed(failure) => LeaseEnd::Failed(failure, reason.unwrap_or_default()),
        };
        self.recorder
            .emit(Event::Finished(kbf_invocation::Finished {
                operation: answer.operation,
                lease: Some((answer.lease, end)),
                outcome: answered(finished),
                result: listing,
                at: wall_now(),
            }));
    }

    /// Sends the end of an operation the scheduler refused: no lease ended it.
    pub(super) fn record_refused(&mut self, refusal: &Refusal) {
        if self.recorded.remove(&refusal.operation).is_none() {
            return;
        }
        self.recorder
            .emit(Event::Finished(kbf_invocation::Finished {
                operation: refusal.operation,
                lease: None,
                outcome: Answered::Refused(refusal.reason.clone()),
                result: None,
                at: wall_now(),
            }));
    }
}

/// The fields of the `action usage:` line of an action that ran: its exit code, its
/// `mnemonic` and `target` as the client sent them (`-` when it sent none, or the
/// operation was not recorded), the run's wall time and what the lease's cgroup
/// counted (`kbf_proto::worker::CgroupUsage::log_fields`, the keys of the daemon's
/// `lease phases:` line). `None` for a result that carries no usage: nothing was
/// measured, so there is nothing to say.
fn usage_line(label: Option<&ActionLabel>, result: &ActionResult) -> Option<String> {
    let usage = ResourceUsage::in_result(result)?;
    let text = |t: Option<&String>| t.map_or_else(|| "-".to_owned(), |t| format!("{t:?}"));
    let cgroup = usage
        .cgroup
        .as_ref()
        .map_or_else(kbf_proto::no_cgroup_fields, |c| {
            c.log_fields(usage.wall_micros)
        });
    Some(format!(
        "exit_code={} mnemonic={} target={} wall_us={} {cgroup}",
        result.exit_code,
        text(label.and_then(|l| l.mnemonic.as_ref())),
        text(label.and_then(|l| l.target.as_ref())),
        usage.wall_micros,
    ))
}

/// What callers told `finished` were answered, as a row records it (section 3.2): an
/// OK status with the exit code, a timeout (DEADLINE_EXCEEDED, with or without a
/// partial result), or another status with its code and message.
fn answered(finished: &Finished) -> Answered {
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
