//! How a run ends, as the farm is told.
//!
//! The lease server reports a run's outcome and exit code and nothing else, so the task
//! tool's own exit status carries the contract: exit 0 means the action ran, whatever its own
//! exit code, and the pack holds the rest. Any other ending says why the action did not run,
//! and the farm runs it again or answers the caller:
//!
//! | the run | the farm is told |
//! |---|---|
//! | succeeded, exit 0 | whatever the frontend's ingest of the pack says |
//! | out of memory | `RESOURCE_EXHAUSTED`, killed at its own limit: a farm fault, rerun with more memory |
//! | timed out at the wall the executor asked for | `DEADLINE_EXCEEDED`: the caller's timeout |
//! | timed out at a wall the lease cut short | lost (fenced): the farm's fault, rerun |
//! | failed to start because the host's scratch quota is out of order (`quota-unavailable`) | lost (fenced): the host's fault, rerun elsewhere |
//! | failed (the tool exited nonzero), killed unasked, failed to start, unknown | `UNAVAILABLE`: infrastructure, rerun |
//! | the server forgot the run (it is not listed any more) or the host never listed it | lost (daemon restarted): rerun |

use kbf_lease_client::{Outcome, Run, RunState};
use kbf_proto::google::rpc;
use kbf_proto::reapi::Digest;
use kbf_proto::worker::{self, Lost, MemoryKill};
use kbf_types::LeaseId;
use tonic::Code;

/// What a finished run comes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The action ran and its pack is to be ingested.
    Ingest,
    /// The run did not produce a result.
    Settled(Settled),
}

/// A run that did not produce a pack, by what the farm does about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Settled {
    /// Infrastructure: the farm runs the action again.
    Infra(String),
    /// The action's timeout: the caller's.
    Timeout,
    /// Killed at its own memory limit.
    OutOfMemory,
    /// Lost to the host or the lease server: the farm runs the action again elsewhere.
    Lost(Lost),
    /// The action cannot run here: the caller's to fix.
    Invalid(String),
}

/// The detail a js-agent gives a start it refuses because its scratch disk quota is out of
/// order (`js-agent-wire`'s `QUOTA_UNAVAILABLE`). Nothing was created, the host is clean, and
/// it recovers by itself.
const QUOTA_UNAVAILABLE: &str = "quota-unavailable";

/// Whether `run` never started because the host's scratch quota is out of order: the host's
/// fault, and no taint.
pub(super) fn refused_for_quota(run: &Run) -> bool {
    run.outcome == Some(Outcome::FailedToStart) && run.detail.as_deref() == Some(QUOTA_UNAVAILABLE)
}

/// Whether `run` never started because its host cannot deliver the files of a run.
#[allow(dead_code)]
pub(super) fn refused_for_files(run: &Run) -> bool {
    let _ = run;
    false
}

/// The verdict on `run`. `truncated` says the lease cut the run's wall time short of what
/// was asked, so a timeout was the lease's.
pub(super) fn judge(run: &Run, truncated: bool) -> Verdict {
    // The host's detail is a driver's text, and a driver's error can quote the arguments, which
    // hold the pack url and the read token: neither the log nor the caller gets it, only its
    // length. The caller is told the fixed phrase and the exit code.
    let infra = |what: &str| {
        let detail_bytes = run.detail.as_deref().map_or(0, str::len);
        tracing::warn!(run = %run.run_id, detail_bytes, exit_code = ?run.exit_code, "{what}");
        Verdict::Settled(Settled::Infra(format!(
            "{what} (exit code {:?})",
            run.exit_code
        )))
    };
    match (run.state, run.outcome) {
        (RunState::NeverStarted, _) => Verdict::Settled(Settled::Lost(Lost::DaemonRestarted)),
        (_, Some(Outcome::Succeeded)) if run.exit_code.unwrap_or(0) == 0 => Verdict::Ingest,
        (_, Some(Outcome::Succeeded)) => infra("the run succeeded with a nonzero exit code"),
        (_, Some(Outcome::Failed)) => infra("the task tool failed"),
        (_, Some(Outcome::Killed)) => infra("the run was killed unasked"),
        (_, Some(Outcome::TimedOut)) if truncated => Verdict::Settled(Settled::Lost(Lost::Fenced)),
        (_, Some(Outcome::TimedOut)) => Verdict::Settled(Settled::Timeout),
        (_, Some(Outcome::OutOfMemory)) => Verdict::Settled(Settled::OutOfMemory),
        (_, Some(Outcome::FailedToStart)) if refused_for_quota(run) => {
            tracing::warn!(run = %run.run_id, "the host refused the start: its scratch quota is out of order");
            Verdict::Settled(Settled::Lost(Lost::Fenced))
        }
        (_, Some(Outcome::FailedToStart)) => infra("the run could not start"),
        (_, Some(Outcome::Unknown)) => infra("the run ended in a way this server does not know"),
        (_, None) => infra("the run ended with no outcome"),
    }
}

fn status(code: Code, message: &str) -> Option<rpc::Status> {
    Some(rpc::Status {
        code: code as i32,
        message: message.to_owned(),
        details: Vec::new(),
    })
}

/// The `Result` the farm is sent for `lease`, which ran `action`, ending as `settled`.
pub(super) fn result_of(lease: LeaseId, action: &Digest, settled: &Settled) -> worker::Result {
    let mut result = worker::Result {
        lease_id: Some(worker::LeaseId {
            term: lease.term,
            seq: lease.seq,
        }),
        action_digest: Some(action.clone()),
        ..worker::Result::default()
    };
    match settled {
        Settled::Infra(why) => result.status = status(Code::Unavailable, why),
        Settled::Timeout => {
            result.status = status(Code::DeadlineExceeded, "the action ran out of time");
        }
        Settled::OutOfMemory => {
            result.status = status(
                Code::ResourceExhausted,
                "the action passed its memory limit",
            );
            result.memory_kill = MemoryKill::OwnLimit as i32;
        }
        Settled::Lost(why) => {
            result.status = status(Code::Unavailable, "the run was lost");
            result.lost = *why as i32;
        }
        Settled::Invalid(why) => result.status = status(Code::InvalidArgument, why),
    }
    result
}

/// The `Result` of a run that ended well: the action ran and the frontend rebuilt its result.
pub(super) fn ran(
    lease: LeaseId,
    action: &Digest,
    action_result: kbf_proto::reapi::ActionResult,
) -> worker::Result {
    worker::Result {
        lease_id: Some(worker::LeaseId {
            term: lease.term,
            seq: lease.seq,
        }),
        action_digest: Some(action.clone()),
        action_result: Some(action_result),
        ..worker::Result::default()
    }
}

#[cfg(test)]
#[path = "outcome_tests.rs"]
mod tests;
