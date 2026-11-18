//! The executor: every REAPI Execute is one job of the job service.
//!
//! # How it sits in the server
//!
//! [`Executor`] is the dispatcher the REAPI front hands each Execute to
//! ([`kbf_front::Dispatch`]). An Execute that names an action another Execute is already running
//! joins that operation; any other is a new operation, `operations/<id>`, and its run is
//! submitted to the job service at once as a job of the kbf image, whose entrypoint is the task
//! tool ([`submit`]). Nothing in kbf queues it, places it or counts it against a capacity: how
//! many jobs run, and where, is the job service's. The only bound is a guard on the memory the
//! open operations hold ([`MAX_OPEN`]).
//!
//! Each operation is driven by a task of its own ([`run`]): the job is read back every
//! [`Config::poll`] until it ends; an ending of the farm's (the host lost, the job service
//! restarted, the job refused at its start) submits the operation again as its next attempt, a
//! new job with a new key, at most [`outcome::RESUBMITS`] times; anything else, and the last
//! attempt, answers the operation's callers ([`outcome`]). A job that ended well has its pack
//! read by the frontend ([`Ingest`]), which writes the action cache. `CancelOperation` cancels
//! the job and answers the callers `CANCELLED` ([`kbf_front::Dispatch::cancel`]).
//!
//! # Over a state store
//!
//! With `--state-store`, the operation ids come from the store's shared counter and each
//! operation keeps a row from before its first submit until it finishes ([`rows`]): any frontend
//! of the store answers `WaitExecution` and `GetOperation` of any operation from the rows, and a
//! frontend takes up the unfinished operations of a process whose liveness row has expired,
//! reading their jobs back where they are ([`Running`]). A restart is such a process: the new
//! one takes up the old one's jobs. Nothing is cancelled when a frontend ends.
//!
//! The frontend builds one only for `--executor js-agent` (`frontend/executor_flags.rs`), over
//! [`Front`], which implements the two seams ([`Ingest`], [`Inputs`]) with the
//! frontend's own packs, CAS, key mint and run preparation. A frontend with no executor answers
//! every Execute that is not a cache hit `FAILED_PRECONDITION`.

mod api;
mod config;
mod driver;
mod front;
mod front_cache;
mod histogram;
mod mac;
mod metrics;
mod ops;
mod out;
mod outcome;
mod output;
mod phases;
mod probe;
mod record;
pub mod rows;
mod run;
mod size_class;
mod steps;
mod submit;
mod task;
mod traits;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_front::{Dispatch, Recorder, Stage, Submission, Ticket};
use kbf_job_client::Client;
use kbf_types::OperationId;
use tokio::sync::watch;
use tokio::time::Instant;
use tonic::Status;

use ops::{Joined, Ops};
use rows::Rows;
use run::{Op, Start};

pub use config::{Config, ConfigError, FINISHED_RETENTION, InputTree, LIVE};
pub use driver::Running;
pub use front::{
    Front, FrontParts, MintedDir, PrepareError, PreparedPutDir, PutDirParts, Republish, RunPrepare,
    RunSource,
};
pub use mac::{
    DEFAULT_MAC_OS_VERSION, MAC_OS, MAC_OUT_TARGET, MacPool, MacPoolError, System, TREE_TARGET,
};
pub use metrics::Metrics;
pub use out::{MAX_ROOT_BYTES, OUT_TARGET, PutDirRoots, RootError, TASK_IN_TREE};
pub use outcome::OUT_OF_MEMORY_REASON;
pub use size_class::SizeClass;
pub use submit::job_id;
pub use task::{PackKey, RunId, build_id};
pub use traits::{
    ActionFacts, Ingest, Ingested, Inputs, InputsError, LostObject, MintError, RejectReason,
    UntrustedLimits,
};

/// The most operations the executor holds open at once: a guard on the memory they hold (a
/// prepared job's spec, a stage channel and a task each), not a bound on how many jobs run.
/// An Execute past it is refused UNAVAILABLE, which a client retries.
pub const MAX_OPEN: usize = 65_536;

/// The REAPI name of `operation`: `operations/<id>`.
#[must_use]
pub fn stored_name(operation: OperationId) -> String {
    format!("operations/{}", operation.0)
}

/// The operation a name of the form [`stored_name`] writes names: `None` for any other
/// spelling, a cached result's name (`operations/cached/...`) among them, and for a spelling
/// [`stored_name`] does not write (signs, leading zeros, white space).
#[must_use]
pub fn parse_stored_name(name: &str) -> Option<OperationId> {
    let digits = name.strip_prefix("operations/")?;
    let canonical = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'));
    canonical
        .then(|| digits.parse().ok())
        .flatten()
        .map(OperationId)
}

/// What the executor is built over.
#[derive(Clone)]
pub struct Deps {
    /// The job service.
    pub client: Client,
    /// The frontend's read of a task's pack.
    pub ingest: Arc<dyn Ingest>,
    /// The CAS read of an action, and the action tree of a run.
    pub inputs: Arc<dyn Inputs>,
    /// What the executor counts.
    pub metrics: Arc<Metrics>,
    /// The state store the operations' rows are kept in, if the frontend has one.
    pub store: Option<Arc<dyn kbf_state::Store>>,
    /// The invocation recorder the operations' events go to.
    pub recorder: Recorder,
}

/// The executor's state, shared by the dispatcher and the tasks of its operations.
pub(crate) struct Inner {
    pub(crate) cfg: Config,
    pub(crate) deps: Deps,
    pub(crate) rows: Option<Rows>,
    pub(crate) ops: Mutex<Ops>,
}

impl Inner {
    /// The job service, each call limited to [`Config::call_timeout`].
    pub(crate) fn api(&self) -> api::Api<'_> {
        api::Api {
            client: &self.deps.client,
            limit: self.cfg.call_timeout,
        }
    }

    pub(crate) fn ops(&self) -> std::sync::MutexGuard<'_, Ops> {
        self.ops.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The dispatcher of Executes to the job service.
#[derive(Clone)]
pub struct Executor {
    inner: Arc<Inner>,
}

impl Executor {
    /// An executor of `cfg` over `deps`. Nothing is asked of the job service until an Execute.
    ///
    /// # Errors
    /// `cfg` breaks a rule of [`Config::validate`].
    pub fn new(cfg: Config, deps: Deps) -> Result<Self, ConfigError> {
        cfg.validate()?;
        let rows = deps
            .store
            .as_ref()
            .map(|store| Rows::new(Arc::clone(store), &cfg.instance));
        let ops = Ops::new(rows.is_none().then(process_base));
        Ok(Self {
            inner: Arc::new(Inner {
                cfg,
                deps,
                rows,
                ops: Mutex::new(ops),
            }),
        })
    }

    /// The job service as a readiness check asks about it: one read of a job no one made, which
    /// the service answers whether it has it or not, bounded by `limit`.
    #[must_use]
    pub fn probe(&self, limit: Duration) -> probe::ServiceProbe {
        probe::ServiceProbe::new(self.inner.deps.client.clone(), limit)
    }

    /// How many operations are open: submitted and not yet answered.
    #[must_use]
    pub fn open(&self) -> usize {
        self.inner.ops().open_count()
    }

    pub(crate) fn inner(&self) -> &Arc<Inner> {
        &self.inner
    }

    /// A ticket on the open operation `operation`, or on one finished within the retention.
    fn held(&self, operation: OperationId) -> Option<Ticket> {
        let mut ops = self.inner.ops();
        ops.prune(Instant::now(), self.inner.cfg.finished_retention);
        ops.ticket(operation)
    }

    /// A ticket on `operation` from the state store: done if its answer is there, else a
    /// follower that shows the answer once another frontend (or this one, having taken it up)
    /// writes it.
    fn stored(&self, operation: OperationId) -> Option<Ticket> {
        let rows = self.inner.rows.as_ref()?;
        let looked = rows.look(operation).and_then(|look| match look {
            rows::Look::Done(key, finished) => Ok(Some((key, Some(finished)))),
            rows::Look::Open(row) => row
                .action_key()
                .map(|key| Some((key, None)))
                .map_err(|why| rows::RowsError::Corrupt {
                    key: rows::op_key(operation),
                    why,
                }),
            rows::Look::Gone => Ok(None),
        });
        let (key, finished) = match looked {
            Ok(found) => found?,
            Err(error) => {
                tracing::warn!(%error, %operation, "the state store could not say where an operation is");
                return None;
            }
        };
        let (instance, action) = (key.instance.clone(), key.action);
        let stage = match finished {
            Some(finished) => watch::channel(Stage::Done(finished)).1,
            None => run::follow(&self.inner, operation, key.clone()),
        };
        Some(Ticket {
            name: stored_name(operation),
            instance,
            action,
            stage,
        })
    }
}

impl Dispatch for Executor {
    fn submit(&self, submission: Submission) -> Result<Ticket, Status> {
        let Submission {
            key,
            whole_machine,
            call,
            ..
        } = submission;
        // An operation id of the state store is reserved with no lock held: once a block of
        // ids, that is a read and a write of the store.
        let mut reserved = None;
        let (name, stage) = loop {
            let mut ops = self.inner.ops();
            ops.prune(Instant::now(), self.inner.cfg.finished_retention);
            if let Some(Joined { operation, stage }) = ops.join(&key) {
                let name = stored_name(operation);
                record::answered(&self.inner.deps.recorder, call, &name, operation, true);
                break (name, stage);
            }
            if ops.open_count() >= MAX_OPEN {
                return Err(Status::unavailable(format!(
                    "the executor holds {MAX_OPEN} open operations, the most its memory guard \
                     allows: try again"
                )));
            }
            let operation = match (&self.inner.rows, reserved) {
                (None, _) => ops.next_id(),
                (Some(_), Some(operation)) => operation,
                (Some(rows), None) => {
                    drop(ops);
                    let operation = rows.next_id().map_err(|e| {
                        Status::unavailable(format!("no operation id from the state store: {e}"))
                    })?;
                    // Another Execute of the action may have opened it meanwhile: look again.
                    reserved = Some(operation);
                    continue;
                }
            };
            let name = stored_name(operation);
            let recorded =
                record::answered(&self.inner.deps.recorder, call, &name, operation, false);
            let (stage, cancel) = ops.open(operation, key.clone());
            let op = Op::new(operation, key.clone(), whole_machine, recorded);
            run::spawn(Arc::clone(&self.inner), op, Start::Fresh, cancel);
            break (name, stage);
        };
        Ok(Ticket {
            name,
            instance: key.instance,
            action: key.action,
            stage,
        })
    }

    fn wait(&self, name: &str) -> Option<Ticket> {
        let operation = parse_stored_name(name)?;
        self.held(operation).or_else(|| self.stored(operation))
    }

    fn cancel(&self, name: &str) -> bool {
        parse_stored_name(name).is_some_and(|operation| self.inner.ops().cancel(operation))
    }
}

/// What a server hands its Executes to: its executor, or, on a server that runs none, a refusal
/// of each Execute the action cache did not answer.
pub enum Dispatcher {
    /// The executor.
    Executor(Executor),
    /// No executor: every Execute is refused FAILED_PRECONDITION.
    None,
}

impl Dispatcher {
    /// The dispatcher of a server with `executor`.
    #[must_use]
    pub fn of(executor: Option<Executor>) -> Arc<Self> {
        Arc::new(executor.map_or(Self::None, Self::Executor))
    }
}

impl Dispatch for Dispatcher {
    fn submit(&self, submission: Submission) -> Result<Ticket, Status> {
        match self {
            Self::Executor(executor) => executor.submit(submission),
            Self::None => Err(Status::failed_precondition(
                "this server runs no executor (--executor js-agent): it answers an Execute \
                 only from the action cache",
            )),
        }
    }

    fn wait(&self, name: &str) -> Option<Ticket> {
        match self {
            Self::Executor(executor) => executor.wait(name),
            Self::None => None,
        }
    }

    fn cancel(&self, name: &str) -> bool {
        match self {
            Self::Executor(executor) => executor.cancel(name),
            Self::None => false,
        }
    }
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor")
            .field("frontend", &self.inner.cfg.frontend)
            .field("instance", &self.inner.cfg.instance)
            .finish_non_exhaustive()
    }
}

/// The first operation id of a process with no state store: the wall-clock time of its start in
/// milliseconds, times 2^16, plus 16 random bits, so a name a client holds from an earlier
/// process names no operation of this one unless the clock stepped back to that very
/// millisecond and the bits match.
fn process_base() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;
    let start = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
    // A `RandomState` is keyed from the operating system's random source.
    let random = RandomState::new().hash_one(start) & 0xffff;
    (start << 16) | random
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod dispatch_tests;
#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
