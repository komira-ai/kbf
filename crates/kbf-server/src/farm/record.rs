//! What the farm tells the invocation recorder (`docs/design/invocations.md`, section
//! 3.4): each lease granted, started and given up, and each operation's end, with
//! the lease that ended it and, for a failure, the daemon's reason. Every event is
//! sent under the farm's lock in the order the scheduler made the change, and after
//! the Execute call that queued or joined the operation, which [`Dispatch::submit`]
//! sends before it queues anything.
//!
//! [`Dispatch::submit`]: kbf_front::Dispatch::submit

use kbf_front::{ExecuteCall, Finished, wall_now};
use kbf_invocation::{Event, LeaseEnd, Listing, Outcome as Answered};
use kbf_sched::Requeue;
use kbf_types::{Answer, LeaseGrant, Outcome, Refusal, StartLease};
use tonic::Code;

use super::State;

impl State {
    /// Sends the events of an Execute call answered with `name` on the scheduler's
    /// operation that a submission of `request` gets: the twin it joins, or the next
    /// new one. Call it before the request is fed, so the call comes first.
    pub(super) fn record_submit(
        &self,
        call: ExecuteCall,
        name: &str,
        request: &kbf_sched::Request,
    ) {
        let twin = self.sched.twin(request);
        let operation = twin.unwrap_or_else(|| self.sched.next_operation());
        for event in call.answered(name.to_owned(), operation, twin.is_some()) {
            self.recorder.emit(event);
        }
    }

    pub(super) fn record_granted(&self, grant: &LeaseGrant) {
        self.recorder.emit(Event::Granted {
            operation: grant.operation,
            lease: grant.lease,
            worker: grant.worker.clone(),
            at: wall_now(),
        });
    }

    pub(super) fn record_started(&self, start: &StartLease) {
        self.recorder.emit(Event::Started {
            operation: start.operation,
            lease: start.lease,
            at: wall_now(),
        });
    }

    pub(super) fn record_requeue(&self, requeue: &Requeue) {
        self.recorder.emit(Event::Requeued {
            operation: requeue.operation,
            lease: requeue.lease,
            reason: requeue.reason,
            at: wall_now(),
        });
    }

    /// Sends the end of `answer`'s operation: its callers were told `finished`; the
    /// stored result's `listing`, if it ran; and `reason`, the daemon's text, if its
    /// lease reported a failure.
    pub(super) fn record_finished(
        &self,
        answer: &Answer,
        finished: &Finished,
        listing: Option<Listing>,
        reason: Option<String>,
    ) {
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
    pub(super) fn record_refused(&self, refusal: &Refusal) {
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
