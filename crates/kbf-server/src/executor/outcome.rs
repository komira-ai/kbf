//! How a job ends, as the farm is told.
//!
//! The job service reports a job's state, its exit code, why it ended and, for one that did
//! not start, a code; nothing else. So the task tool's own exit status carries the contract:
//! exit 0 means the action ran, whatever its own exit code, and the pack holds the rest. Any
//! other ending says why the action did not run, and the farm runs it again or answers the
//! caller.
//!
//! **The promotion rule.** A job's pack is read, and its outputs promoted to CAS and
//! action-cache entries, only from a clean result: the job's state is succeeded, and its
//! result's state is succeeded, its reason is exit and its exit code is 0. A result that says
//! anything else, or leaves any of it out, is not clean, however complete the pack the job
//! already wrote: the pack is never read (its key is left to the janitor) and the farm is told
//! the row's class. Nothing written to the store is promoted for having been written.
//!
//! Each row's class is what the code below decides, and a test reads this table and holds the
//! code to it:
//!
//! | the job | class | the farm is told |
//! |---|---|---|
//! | succeeded, its result succeeded, reason exit, exit code 0 | `ingest` | whatever the frontend's ingest of the pack says |
//! | succeeded with a nonzero exit code | `infra` | `UNAVAILABLE`: infrastructure, rerun |
//! | succeeded with no exit code | `infra` | `UNAVAILABLE`: not a clean result, rerun |
//! | succeeded for a reason other than exit | `infra` | `UNAVAILABLE`: not a clean result, rerun |
//! | succeeded, its result in another state | `infra` | `UNAVAILABLE`: not a clean result, rerun |
//! | failed, reason wall limit | `timeout` | `DEADLINE_EXCEEDED`: the action's own timeout, since nothing cuts a job's wall limit |
//! | failed, reason memory | `out-of-memory` | `RESOURCE_EXHAUSTED`, killed at its own limit (`MemoryKill::OwnLimit`): rerun with more memory |
//! | failed, reason exit | `infra` | `UNAVAILABLE`: the task tool exited nonzero, rerun |
//! | failed, any other reason | `infra` | `UNAVAILABLE`: rerun |
//! | cancelled by anyone else | `infra` | `UNAVAILABLE`: rerun |
//! | lost | `fenced` | lost (fenced): the container is gone with no result, the host's fault, rerun elsewhere |
//! | not found after it was seen | `restarted` | lost (daemon restarted): the service no longer knows the id, rerun |
//! | failed to start quota-unavailable | `fenced` | lost (fenced): the host's scratch quota is out of order, rerun elsewhere |
//! | failed to start files-unsupported | `infra` | `UNAVAILABLE`: the host cannot write the files of a job, rerun |
//! | failed to start invalid-file | `invalid` | `INVALID_ARGUMENT`: a file of the job is ours to fix |
//! | failed to start no-capacity | `infra` | `UNAVAILABLE`: rerun, as the farm's own retry policy says |
//! | failed to start start-timeout | `infra` | `UNAVAILABLE`: rerun, as the farm's own retry policy says |
//! | failed to start exclusive-not-ready | `infra` | `UNAVAILABLE`: rerun, as the farm's own retry policy says |
//! | failed to start input-unavailable | `infra` | `UNAVAILABLE`: a host path the job named could not be read on its host, rerun |
//! | failed to start host-unsafe | `infra` | `UNAVAILABLE`: rerun, as the farm's own retry policy says |
//! | failed to start egress-unchecked | `infra` | `UNAVAILABLE`: the host's agent could not check the limits of the job's network (`--executor-network`), rerun |
//! | failed to start, any other code or detail | `infra` | `UNAVAILABLE`: rerun |
//! | in a state this server does not know | `infra` | `UNAVAILABLE`: rerun |
//! | refused at submit no-capable-host | `invalid` | `INVALID_ARGUMENT`: nothing could ever run it |
//! | refused at submit wall-time-not-positive | `invalid` | `INVALID_ARGUMENT`: nothing could ever run it |
//! | refused at submit wall-time-above-ceiling | `invalid` | `INVALID_ARGUMENT`: nothing could ever run it |
//! | refused at submit job-id-conflict | `twin` | `UNAVAILABLE`, and the executor stops: another process wrote the id |
//! | refused at submit as written (`INVALID_ARGUMENT`) | `invalid` | `INVALID_ARGUMENT`: nothing could ever run it |
//! | refused at submit with a code this server does not know | `infra` | `UNAVAILABLE`: rerun |
//! | refused at submit otherwise | `infra` | `UNAVAILABLE`: rerun |
//!
//! A message to the farm names the code, never the job: a job carries the arguments, which
//! may hold the pack url and the read token. The agent's detail of a start it refused is a
//! code (`quota-unavailable`, ...) or text that can quote those arguments: only the code is
//! read, from the start of the detail, and only the detail's length is logged.
//!
//! What a job that did not end well wrote goes with its `Result` as a detail of the status,
//! redacted of the job's grants and cut to a cap ([`output`](super::output)); the code and the
//! message above are unchanged by it.
//!
//! A job this executor cancelled is not judged here: the farm cancelled its lease and owes
//! itself no result, so the task is dropped once the service has answered the cancel.

use js_proto::v1::{Job, JobResult, JobState, RefusalCode, ResultReason, StartFailureCode};
use kbf_job_client::Error;
use kbf_proto::google::rpc;
use kbf_proto::reapi::Digest;
use kbf_proto::worker::{self, Lost, MemoryKill};
use kbf_types::LeaseId;
use tonic::Code;

/// What an ended job comes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The action ran and its pack is to be ingested.
    Ingest,
    /// The job did not produce a result.
    Settled(Settled),
}

/// A job that did not produce a pack, by what the farm does about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Settled {
    /// Infrastructure: the farm runs the action again.
    Infra(String),
    /// The action's timeout: the caller's.
    Timeout,
    /// Killed at its own memory limit.
    OutOfMemory,
    /// Lost to the host or the job service: the farm runs the action again elsewhere.
    Lost(Lost),
    /// The action cannot run here: the caller's to fix.
    Invalid(String),
}

/// A submit the service refused: what the farm is told, and whether another process holds
/// this frontend's id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Refused {
    pub(super) settled: Settled,
    pub(super) twin: bool,
}

/// The codes an agent gives a start it refuses, as `js-agent-wire` spells them.
const QUOTA_UNAVAILABLE: &str = "quota-unavailable";
const FILES_UNSUPPORTED: &str = "files-unsupported";
const INVALID_FILE: &str = "invalid-file";
/// The agent could not check that the network the job would get is limited as it was told
/// (its firewall or its job network is missing or changed), and created nothing. The proto has
/// no start-failure code of its own for it: it is a detail of `AGENT_REFUSED`.
const EGRESS_UNCHECKED: &str = "egress-unchecked";

/// The code an agent's detail starts with: its leading run of lower-case letters, digits and
/// dashes. The rest is never read.
fn agent_code(detail: &str) -> &str {
    let end = detail
        .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
        .unwrap_or(detail.len());
    &detail[..end]
}

/// An infrastructure ending, logged with the job id and the detail's length only.
fn infra(job: &Job, what: &str) -> Verdict {
    let detail_bytes = job.start_failure_detail.len();
    tracing::warn!(job = %job.id, detail_bytes, "{what}");
    Verdict::Settled(Settled::Infra(what.to_owned()))
}

/// The verdict on `job`, which has ended, with its `result`.
pub(super) fn judge(job: &Job, result: &JobResult) -> Verdict {
    match job.state() {
        JobState::Succeeded => promote(job, result),
        JobState::Failed => match result.reason() {
            ResultReason::WallLimit => Verdict::Settled(Settled::Timeout),
            ResultReason::Memory => Verdict::Settled(Settled::OutOfMemory),
            ResultReason::Exit => infra(
                job,
                &format!(
                    "the task tool exited nonzero (exit code {:?})",
                    result.exit_code
                ),
            ),
            _ => infra(job, "the job failed for a reason this server does not know"),
        },
        JobState::Cancelled => infra(job, "the job was cancelled by another client"),
        JobState::Lost => {
            tracing::warn!(job = %job.id, "the job was lost with its host");
            Verdict::Settled(Settled::Lost(Lost::Fenced))
        }
        JobState::FailedToStart => failed_to_start(job),
        _ => infra(job, "the job ended in a state this server does not know"),
    }
}

/// The verdict on `job`, which succeeded, by the promotion rule: its pack is ingested only
/// when `result` also says succeeded, for its exit, with exit code 0.
fn promote(job: &Job, result: &JobResult) -> Verdict {
    match (result.state(), result.reason(), result.exit_code) {
        (JobState::Succeeded, ResultReason::Exit, Some(0)) => Verdict::Ingest,
        (JobState::Succeeded, ResultReason::Exit, Some(code)) => {
            infra(job, &format!("the job succeeded with exit code {code}"))
        }
        (JobState::Succeeded, ResultReason::Exit, None) => {
            infra(job, "the job succeeded with no exit code")
        }
        (JobState::Succeeded, reason, _) => infra(
            job,
            &format!(
                "the job succeeded for a reason other than its exit ({})",
                reason.as_str_name()
            ),
        ),
        (state, _, _) => infra(
            job,
            &format!(
                "the job succeeded but its result says {}",
                state.as_str_name()
            ),
        ),
    }
}

/// The verdict on a job that did not start.
fn failed_to_start(job: &Job) -> Verdict {
    match job.start_failure() {
        StartFailureCode::NoCapacity => infra(job, "the job failed to start: no-capacity"),
        StartFailureCode::StartTimeout => infra(job, "the job failed to start: start-timeout"),
        StartFailureCode::ExclusiveNotReady => {
            infra(job, "the job failed to start: exclusive-not-ready")
        }
        StartFailureCode::InputUnavailable => {
            infra(job, "the job failed to start: input-unavailable")
        }
        StartFailureCode::AgentRefused => match agent_code(&job.start_failure_detail) {
            QUOTA_UNAVAILABLE => {
                tracing::warn!(job = %job.id, "the host refused the start: its scratch quota is out of order");
                Verdict::Settled(Settled::Lost(Lost::Fenced))
            }
            FILES_UNSUPPORTED => infra(job, "the host cannot write the files of a job"),
            INVALID_FILE => Verdict::Settled(Settled::Invalid(
                "the host refused a file of the job".to_owned(),
            )),
            "host-unsafe" => infra(job, "the job failed to start: host-unsafe"),
            EGRESS_UNCHECKED => infra(
                job,
                "the host refused to start the job: its agent could not check the limits of \
                 the job's network (egress-unchecked)",
            ),
            _ => infra(job, "the host refused to start the job"),
        },
        StartFailureCode::Unspecified => infra(job, "the job failed to start"),
    }
}

/// The ending of a job the service no longer knows, after it was seen.
pub(super) fn forgotten() -> Settled {
    Settled::Lost(Lost::DaemonRestarted)
}

/// A job withdrawn at a drain while it was still queued on the job service: nothing ran.
pub(super) fn drained() -> Settled {
    Settled::Infra("the frontend is draining and the job had not been placed on a host".to_owned())
}

/// What a submit that failed with `error` comes to; `None` when nothing is decided, because
/// nothing was sent or the answer was lost, and the same submit is sent again.
pub(super) fn refused(error: &Error) -> Option<Refused> {
    let settled = |settled| {
        Some(Refused {
            settled,
            twin: false,
        })
    };
    match error {
        Error::Unreachable(_) | Error::Transport(_) | Error::Decode(_) => None,
        Error::Refused(refusal) => match refusal.code() {
            RefusalCode::JobIdConflict => Some(Refused {
                settled: Settled::Infra(
                    "the job service holds another job under this job's id (job-id-conflict)"
                        .to_owned(),
                ),
                twin: true,
            }),
            code @ (RefusalCode::NoCapableHost
            | RefusalCode::WallTimeNotPositive
            | RefusalCode::WallTimeAboveCeiling) => settled(Settled::Invalid(format!(
                "the job service refused the job: {}",
                code.as_str()
            ))),
            RefusalCode::Unspecified => settled(Settled::Infra(
                "the job service refused the job for a reason this server does not know".to_owned(),
            )),
        },
        Error::Status {
            code: Code::InvalidArgument,
            ..
        } => settled(Settled::Invalid(
            "the job service refused the job as written (INVALID_ARGUMENT)".to_owned(),
        )),
        Error::Status { code, .. } => settled(Settled::Infra(format!(
            "the job service did not take the job ({code:?})"
        ))),
        Error::TooMany(_) | Error::Stuck(_) => settled(Settled::Infra(
            "the job service gave an answer a submit does not have".to_owned(),
        )),
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
            result.status = status(Code::Unavailable, "the job was lost");
            result.lost = *why as i32;
        }
        Settled::Invalid(why) => result.status = status(Code::InvalidArgument, why),
    }
    result
}

/// The `Result` of a job that ended well: the action ran and the frontend rebuilt its result.
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
