//! What a worker's `Result` makes of an operation, and what its callers are told: the
//! scheduler's outcome for a report, the detail the report carries beyond it, and the
//! [`Finished`] a finished operation's callers see (and its `done/` row keeps).
//!
//! Both scheduling cores use these: the farm's own ([`crate::farm`]) and the one built on
//! `js-server-core` ([`crate::farm_js`]). A change here is a change in what a caller of
//! either is answered.

use std::time::{Duration, SystemTime};

use kbf_front::{Backend, Finished};
use kbf_meta::ActionRecord;
use kbf_proto::google::rpc;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker;
use kbf_storage::StorageError;
use kbf_types::{Answer, Digest, Failure, FarmRun, LeaseId, Outcome, Refusal, WorkerId};
use tonic::Code;

use crate::fault;
use crate::memory;
use crate::missing;
use crate::stamp::Stamp;

/// An OK result and its action-cache record, from the report to the answer.
#[derive(Debug)]
pub struct Pending {
    /// The result the callers get, stamped with the node and the server's times.
    pub result: ActionResult,
    /// Its action-cache record.
    pub record: ActionRecord,
}

/// What a report carries beyond its scheduler outcome, from the report to the answer.
#[derive(Debug)]
pub enum Detail {
    /// An OK result to answer with and maybe cache.
    Ran(Box<Pending>),
    /// The daemon's reason the action is invalid, passed to the callers.
    Invalid(String),
    /// The input blobs the worker could not fetch, which the callers are asked to
    /// upload again (`crate::missing`).
    Missing(Vec<Digest>),
    /// A timed-out action's partial result ([`crate::timeout`]), answered, never cached.
    TimedOut(Box<ActionResult>),
    /// The daemon's reason for an infrastructure failure (a missing image, a full
    /// disk), or why an OK result could not be accepted: the callers are told only
    /// that the farm could not run the action, and the invocation record keeps it.
    Infra(String),
}

impl Detail {
    /// The failure's reason text a lease's attempt records, if the report gave one.
    #[must_use]
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Invalid(why) | Self::Infra(why) => Some(why.clone()),
            Self::Ran(_) | Self::Missing(_) | Self::TimedOut(_) => None,
        }
    }
}

/// The scheduler's outcome for a `Result`, with the cache record of an OK one, the
/// reason for an INVALID_ARGUMENT one, the partial result of a DEADLINE_EXCEEDED one
/// ([`crate::timeout`], stamped as an OK result is), or the blobs of a missing-input
/// one ([`missing::inputs`]). An OK result whose outputs are not all stored is an
/// infrastructure failure: accepting it would answer callers with files nobody can
/// fetch. While the store cannot answer at all (every owner of an output is down) the
/// check is asked again, for up to ten minutes, before it fails ([`prepare`]). An OK result gets the node it `ran_on` and the server's times
/// ([`Stamp::apply`], received when `wall` reads) before it is checked and its cache record
/// made.
///
/// The wait for storage runs inside the report that carries the result, with the lease
/// held and no farm lock. A handover can fence the farm during it (`Stand::quiesce`
/// waits at most `QUIESCE_WAIT`, then fences): the fenced farm drops the late report, the
/// node, whose result is not acknowledged, sends it again to the successor, and the
/// successor starts its own wait. Copies of a result that reconnects send each run one
/// bounded wait, and only the first to finish is taken.
pub async fn outcome(
    cache: &Backend,
    wall: impl Fn() -> SystemTime,
    lease: LeaseId,
    ran_on: (&WorkerId, Stamp),
    result: worker::Result,
) -> (Outcome, Option<Detail>) {
    outcome_pausing(cache, &Sleep, wall, lease, ran_on, result).await
}

/// What [`prepare`] waits with between two asks of the store: tokio's timer in the
/// server, a recorder in the tests that check how long the wait is asked to be.
pub(crate) trait Pause {
    /// Returns after `for_how_long`.
    fn pause(&self, for_how_long: Duration) -> impl Future<Output = ()> + Send;
}

/// The server's pause.
struct Sleep;

impl Pause for Sleep {
    fn pause(&self, for_how_long: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(for_how_long)
    }
}

/// [`outcome`], waiting for storage with `pause`.
pub(crate) async fn outcome_pausing(
    cache: &Backend,
    pause: &impl Pause,
    wall: impl Fn() -> SystemTime,
    lease: LeaseId,
    ran_on: (&WorkerId, Stamp),
    result: worker::Result,
) -> (Outcome, Option<Detail>) {
    if let Some(why) = fault::lost(&result) {
        let node = ran_on.0;
        tracing::warn!(%lease, %node, %why, "the farm lost the run");
        return (Outcome::Failed(Failure::Lost(why)), None);
    }
    if let Some(kill) = memory::killed(&result) {
        tracing::info!(%lease, ?kill, "lease killed for memory");
        return (Outcome::Failed(kill), None);
    }
    let code = result.status.as_ref().map_or(Code::Ok as i32, |s| s.code);
    match (Code::from_i32(code), result.action_result) {
        (Code::Ok, Some(mut result)) => {
            let (node, stamp) = ran_on;
            stamp.apply(&mut result, node, wall());
            match prepare(cache, pause, lease, &result).await {
                Ok(record) => {
                    let outcome = Outcome::Completed {
                        action_result: record.result.into(),
                    };
                    (
                        outcome,
                        Some(Detail::Ran(Box::new(Pending { result, record }))),
                    )
                }
                Err(e) => {
                    tracing::warn!(%lease, error = %e, "an OK result whose outputs are not stored");
                    let why = format!("an OK result whose outputs are not stored: {e}");
                    (Outcome::Failed(Failure::Infra), Some(Detail::Infra(why)))
                }
            }
        }
        (Code::DeadlineExceeded, reported) => {
            let partial = match reported {
                Some(reported) => crate::timeout::partial(cache, reported).await,
                None => None,
            };
            let detail = partial.map(|mut partial| {
                let (node, stamp) = ran_on;
                stamp.apply(&mut partial, node, wall());
                Detail::TimedOut(Box::new(partial))
            });
            (Outcome::Failed(Failure::Timeout), detail)
        }
        (Code::InvalidArgument, _) => {
            let why = result.status.map(|s| s.message).unwrap_or_default();
            (
                Outcome::Failed(Failure::Invalid),
                Some(Detail::Invalid(why)),
            )
        }
        (code, _) => match result.status.as_ref().and_then(missing::inputs) {
            Some(blobs) => {
                tracing::warn!(%lease, blobs = blobs.len(), "input blobs missing at the worker");
                (
                    Outcome::Failed(Failure::MissingInput),
                    Some(Detail::Missing(blobs)),
                )
            }
            None => {
                // The client is told only that the farm could not run the action: the
                // daemon's reason (a missing image, a full disk) is in this line and
                // in the invocation record.
                let why = result.status.map(|s| s.message).unwrap_or_default();
                tracing::warn!(%lease, node = %ran_on.0, ?code, reason = %why, "lease failed");
                (Outcome::Failed(Failure::Infra), Some(Detail::Infra(why)))
            }
        },
    }
}

/// How long [`prepare`] waits between two asks of a store that cannot answer. Fixed, not
/// tunable: no flag carries it until an operator needs another value, because a flag is
/// one more setting to document, require and keep compatible, and a whole-farm restart
/// brings storage back well inside [`STORAGE_WAITS`] pauses of this length.
pub const STORAGE_PAUSE: Duration = Duration::from_secs(5);

/// How many times [`prepare`] waits [`STORAGE_PAUSE`] for a store that cannot answer: ten
/// minutes in all. Past it the store's error is the result's, and the operation is run
/// again as any infrastructure failure is. Fixed, not tunable, for the reason
/// [`STORAGE_PAUSE`] gives.
pub const STORAGE_WAITS: u32 = 120;

/// [`Backend::prepare_action_result`] for the OK `result` of `lease`, asked again every
/// [`STORAGE_PAUSE`], up to [`STORAGE_WAITS`] times, while the store cannot answer
/// ([`StorageError::Unavailable`], such as every owner of an output being down). The
/// action ran to completion on its node and its outputs are not at fault, so the run is
/// not charged to the operation's farm reruns for an outage that ends in minutes: the
/// result waits here, the lease held (the node's heartbeats list it until the result is
/// acknowledged), and is accepted once the store answers. Any other error (an output the
/// store lacks, a malformed result) is returned at once.
///
/// The wait runs inside the report, with no farm lock held. If a handover fences the farm
/// meanwhile (`Stand::quiesce` waits at most `QUIESCE_WAIT`, then fences), the fenced
/// farm drops the late report; the node, never acknowledged, sends the result to the
/// successor, which starts its own wait. Copies sent by reconnects each run one bounded
/// wait, and only the first to finish is taken.
async fn prepare(
    cache: &Backend,
    pause: &impl Pause,
    lease: LeaseId,
    result: &ActionResult,
) -> Result<ActionRecord, StorageError> {
    let mut waited = 0;
    loop {
        match cache.prepare_action_result(result).await {
            Err(StorageError::Unavailable(e)) if waited < STORAGE_WAITS => {
                if waited == 0 {
                    tracing::warn!(%lease, error = %e, "an OK result waits for storage to answer");
                }
                waited += 1;
                pause.pause(STORAGE_PAUSE).await;
            }
            answer => {
                if waited > 0 {
                    tracing::info!(%lease, waited, "storage answered an OK result's check");
                }
                return answer;
            }
        }
    }
}

/// What the callers of the operation `answer` finishes are told, given the report's
/// `detail` if it had one. The row the state store keeps of the result is this too, so
/// callers and followers never differ.
#[must_use]
pub fn finished_of(answer: &Answer, detail: Option<&Detail>) -> Finished {
    match (detail, answer.outcome) {
        (Some(Detail::Ran(pending)), _) => Finished::Ran(Box::new(pending.result.clone())),
        (Some(Detail::TimedOut(partial)), Outcome::Failed(Failure::Timeout)) => {
            let status = status(Code::DeadlineExceeded, TIMED_OUT);
            Finished::TimedOut {
                status,
                partial: partial.clone(),
            }
        }
        (_, Outcome::Failed(Failure::Timeout)) => failed(Code::DeadlineExceeded, TIMED_OUT),
        (Some(Detail::Invalid(why)), _) => failed(Code::InvalidArgument, why),
        (Some(Detail::Missing(blobs)), _) => Finished::Failed(kbf_front::missing_status(blobs)),
        (_, Outcome::Failed(Failure::OutOfMemory)) => memory::finished(&answer.memory_runs),
        (_, outcome) => unrun(outcome, &answer.farm_runs),
    }
}

/// What the callers of the operation `refusal` refuses are told.
#[must_use]
pub fn refused_as(refusal: &Refusal) -> Finished {
    failed(Code::FailedPrecondition, &refusal.reason)
}

/// What the callers of a timed-out action are told.
const TIMED_OUT: &str = "the action timed out";

/// `message` under `code`, as an operation's end.
#[must_use]
pub fn failed(code: Code, message: &str) -> Finished {
    Finished::Failed(status(code, message))
}

fn status(code: Code, message: &str) -> rpc::Status {
    rpc::Status {
        code: code as i32,
        message: message.to_owned(),
        details: Vec::new(),
    }
}

/// How a result that carries no detail and is no timeout, invalid input, missing blob or
/// memory kill is answered: a farm fault names its runs; anything else (a committed
/// `Completed` whose worker report this process never saw) is the fixed INTERNAL.
#[must_use]
pub fn unrun(outcome: Outcome, farm_runs: &[FarmRun]) -> Finished {
    match outcome {
        Outcome::Failed(Failure::NodeMemoryPressure | Failure::Lost(_) | Failure::Infra) => {
            fault::finished(farm_runs)
        }
        _ => failed(Code::Internal, "the farm could not run the action"),
    }
}
