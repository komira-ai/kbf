//! How a job ends, and what the operation's callers are answered.
//!
//! The job service reports a job's state, its exit code, why it ended and, for one that did
//! not start, a code; nothing else. So the task tool's own exit status carries the contract:
//! exit 0 means the action ran, whatever its own exit code, and the pack holds the rest. Any
//! other ending says why the action did not run, and the executor submits the operation's next
//! attempt or answers its callers ([`Settled::retried`], [`Settled::status`]).
//!
//! **The promotion rule.** A job's pack is read, and its outputs promoted to CAS and
//! action-cache entries, only from a clean result: the job's state is succeeded, and its
//! result's state is succeeded, its reason is exit and its exit code is 0. A result that says
//! anything else, or leaves any of it out, is not clean, however complete the pack the job
//! already wrote: the pack is never read (its key is left to the janitor) and the row's class
//! decides the rest. Nothing written to the store is promoted for having been written.
//!
//! Each row's class is what the code below decides, and a test reads this table and holds the
//! code to it:
//!
//! | the job | class | what follows |
//! |---|---|---|
//! | succeeded, its result succeeded, reason exit, exit code 0 | `ingest` | whatever the frontend's ingest of the pack says |
//! | succeeded with a nonzero exit code | `infra` | `UNAVAILABLE`: infrastructure, rerun |
//! | succeeded with no exit code | `infra` | `UNAVAILABLE`: not a clean result, rerun |
//! | succeeded for a reason other than exit | `infra` | `UNAVAILABLE`: not a clean result, rerun |
//! | succeeded, its result in another state | `infra` | `UNAVAILABLE`: not a clean result, rerun |
//! | failed, reason wall limit | `timeout` | `DEADLINE_EXCEEDED`: the action's own timeout, since nothing cuts a job's wall limit |
//! | failed, reason memory | `out-of-memory` | killed at its own limit: rerun, sized again from its class's history; at the last attempt `FAILED_PRECONDITION` with reason `ACTION_OUT_OF_MEMORY` |
//! | failed, reason exit | `infra` | `UNAVAILABLE`: the task tool exited nonzero, rerun |
//! | failed, any other reason | `infra` | `UNAVAILABLE`: rerun |
//! | cancelled by anyone else | `infra` | `UNAVAILABLE`: rerun |
//! | lost | `lost` | the container is gone with no result, the host's fault: rerun |
//! | not found after it was seen | `lost` | the service no longer knows the id: rerun |
//! | failed to start quota-unavailable | `lost` | the host's scratch quota is out of order: rerun |
//! | failed to start files-unsupported | `infra` | `UNAVAILABLE`: the host cannot write the files of a job, rerun |
//! | failed to start invalid-file | `invalid` | `INVALID_ARGUMENT`: a file of the job is ours to fix |
//! | failed to start no-capacity | `infra` | `UNAVAILABLE`: rerun |
//! | failed to start start-timeout | `infra` | `UNAVAILABLE`: rerun |
//! | failed to start exclusive-not-ready | `infra` | `UNAVAILABLE`: rerun |
//! | failed to start input-unavailable | `infra` | `UNAVAILABLE`: a host path the job named could not be read on its host, rerun |
//! | failed to start host-unsafe | `infra` | `UNAVAILABLE`: rerun |
//! | failed to start egress-unchecked | `infra` | `UNAVAILABLE`: the host's agent could not check the limits of the job's network, rerun |
//! | failed to start, any other code or detail | `infra` | `UNAVAILABLE`: rerun |
//! | in a state this server does not know | `infra` | `UNAVAILABLE`: rerun |
//! | refused at submit no-capable-host | `invalid` | `INVALID_ARGUMENT`: nothing could ever run it |
//! | refused at submit wall-time-not-positive | `invalid` | `INVALID_ARGUMENT`: nothing could ever run it |
//! | refused at submit wall-time-above-ceiling | `invalid` | `INVALID_ARGUMENT`: nothing could ever run it |
//! | refused at submit job-id-conflict | `infra` | `UNAVAILABLE`: another spec holds the id, rerun under the next attempt's id |
//! | refused at submit as written (`INVALID_ARGUMENT`) | `invalid` | `INVALID_ARGUMENT`: nothing could ever run it |
//! | refused at submit with a code this server does not know | `infra` | `UNAVAILABLE`: rerun |
//! | refused at submit otherwise | `infra` | `UNAVAILABLE`: rerun |
//!
//! Every `infra` and `lost` row is run again, as the operation's next attempt, at most
//! [`RESUBMITS`] times; then the callers are answered `UNAVAILABLE`. So is `out-of-memory`, and
//! the callers are answered as the table says. A status names the code, never the job: a job
//! carries the arguments and the mounts. The agent's detail of a start it refused is a
//! code (`quota-unavailable`, ...) or text that can quote those: only the code is
//! read, from the start of the detail, and only the detail's length is logged.
//!
//! What a job that did not end well wrote goes with the status as a detail, cut to a cap
//! ([`output`](super::output)); the code and the message above are unchanged by it.
//!
//! A job this executor cancelled for its callers is not judged here: they are answered
//! `CANCELLED`.

use js_proto::v1::{Job, JobResult, JobState, RefusalCode, ResultReason, StartFailureCode};
use kbf_front::ERROR_DOMAIN;
use kbf_job_client::Error;
use kbf_proto::google::rpc::{self, ErrorInfo};
use prost::Message;
use tonic::Code;

/// How many times an operation is submitted again after a run that ended for a reason of the
/// farm's (`infra`, `lost`, `out-of-memory`): each is a new attempt, a new job and a new key.
pub(crate) const RESUBMITS: u32 = 2;

/// The `ErrorInfo.reason` of an action killed at its memory limit at its last attempt.
pub const OUT_OF_MEMORY_REASON: &str = "ACTION_OUT_OF_MEMORY";

/// What an ended job comes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// The action ran and its pack is to be ingested.
    Ingest,
    /// The job did not produce a result.
    Settled(Settled),
}

/// A job that did not produce a pack, by what follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    /// Infrastructure: the action runs again.
    Infra(String),
    /// The action's timeout: the caller's.
    Timeout,
    /// Killed at its own memory limit.
    OutOfMemory,
    /// Lost to the host or the job service: the action runs again.
    Lost(&'static str),
    /// The action cannot run here: the caller's to fix.
    Invalid(String),
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
            Verdict::Settled(Settled::Lost("the job was lost with its host"))
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
                Verdict::Settled(Settled::Lost("the host's scratch quota is out of order"))
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
    Settled::Lost("the job service no longer knows the job")
}

/// What a submit that failed with `error` comes to; `None` when nothing is decided, because
/// nothing was sent or the answer was lost, and the same submit is sent again.
pub(super) fn refused(error: &Error) -> Option<Settled> {
    match error {
        Error::Unreachable(_) | Error::Transport(_) | Error::Decode(_) => None,
        Error::Refused(refusal) => Some(match refusal.code() {
            RefusalCode::JobIdConflict => Settled::Infra(
                "the job service holds another job under this job's id (job-id-conflict)"
                    .to_owned(),
            ),
            code @ (RefusalCode::NoCapableHost
            | RefusalCode::WallTimeNotPositive
            | RefusalCode::WallTimeAboveCeiling) => Settled::Invalid(format!(
                "the job service refused the job: {}",
                code.as_str()
            )),
            RefusalCode::Unspecified => Settled::Infra(
                "the job service refused the job for a reason this server does not know".to_owned(),
            ),
        }),
        Error::Status {
            code: Code::InvalidArgument,
            ..
        } => Some(Settled::Invalid(
            "the job service refused the job as written (INVALID_ARGUMENT)".to_owned(),
        )),
        Error::Status { code, .. } => Some(Settled::Infra(format!(
            "the job service did not take the job ({code:?})"
        ))),
        Error::TooMany(_) | Error::Stuck(_) => Some(Settled::Infra(
            "the job service gave an answer a submit does not have".to_owned(),
        )),
    }
}

impl Settled {
    /// Whether the operation is submitted again, if it has an attempt left ([`RESUBMITS`]):
    /// every ending of the farm's. The action's own timeout and an action that cannot run here
    /// are answered at once.
    pub(super) const fn retried(&self) -> bool {
        matches!(self, Self::Infra(_) | Self::Lost(_) | Self::OutOfMemory)
    }

    /// The status the callers are answered with when this is the operation's last run.
    pub(super) fn status(&self) -> rpc::Status {
        let status = |code: Code, message: &str| rpc::Status {
            code: code as i32,
            message: message.to_owned(),
            details: Vec::new(),
        };
        match self {
            Self::Infra(why) => status(Code::Unavailable, why),
            Self::Lost(why) => status(Code::Unavailable, why),
            Self::Timeout => status(Code::DeadlineExceeded, "the action ran out of time"),
            Self::OutOfMemory => {
                let mut answer = status(
                    Code::FailedPrecondition,
                    "the action passed its memory limit at every attempt",
                );
                let info = ErrorInfo {
                    reason: OUT_OF_MEMORY_REASON.to_owned(),
                    domain: ERROR_DOMAIN.to_owned(),
                    metadata: Default::default(),
                };
                answer.details.push(prost_types::Any {
                    type_url: super::output::ERROR_INFO.to_owned(),
                    value: info.encode_to_vec(),
                });
                answer
            }
            Self::Invalid(why) => status(Code::InvalidArgument, why),
        }
    }
}

#[cfg(test)]
#[path = "outcome_tests.rs"]
mod tests;
