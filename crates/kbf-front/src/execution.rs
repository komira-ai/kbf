//! `Execution`: Execute and WaitExecution, over the cache and a [`Dispatch`].
//!
//! Execute does steps 1 to 3 of `ARCHITECTURE.md#executing-an-action`, then streams the
//! operation:
//! - the action cache is checked first (with the closure check, unless the client set
//!   `skip_cache_lookup`), while the `Action` is read; a hit answers at once,
//!   `cached_result` set, and nothing runs;
//! - the `Action`, its `Command` and its whole input tree must be in the CAS (the Command is
//!   read while the tree is walked, `execution_inputs.rs`); every blob that is not is listed
//!   in one FAILED_PRECONDITION, a `MISSING` violation each. Once the walk has read every
//!   Directory and the Command is decoded, the [`Dispatch`] is asked to start the action
//!   tree's write ([`Dispatch::start_tree`]) while the walk asks whether the files are held;
//! - the request goes to the [`Dispatch`], which joins a running twin (in-flight dedup) or
//!   starts a new operation;
//! - the call streams the operation: `QUEUED`, `EXECUTING` once its job has started, then
//!   the done operation with its `ExecuteResponse`.
//!
//! Execute is authorized by the [`Authorizers::execute`] authorizer against the
//! request's instance name, before anything else is read; WaitExecution by the same
//! authorizer against the instance name of the operation it names, once that is found
//! (an unknown name is NOT_FOUND whoever asks). The reads Execute makes for itself (the
//! action cache, the action, its inputs) are not authorized again. Each Execute runs
//! in an `execute` trace span carrying the instance name, the caller's public
//! authentication metadata (`caller`, `-` when there is none) and the call's invocation
//! id (`invocation`, the `RequestMetadata` header's `tool_invocation_id` as
//! [`crate::RequestMetadataLayer`] read it, `-` when there is none).
//!
//! WaitExecution streams the same updates for an operation name Execute returned, until
//! it is done. A finished operation is kept for a short retention the [`Dispatch`]
//! sets, in which waiting on it streams the done operation; after that it is
//! forgotten, waiting on it is NOT_FOUND, and the client's next Execute is answered
//! from the action cache (or runs the action again, if its result was not cached).
//!
//! **Draining** ([`Closer::drain`], on SIGTERM): a new Execute is refused UNAVAILABLE,
//! which Buck2 retries when it opens the call (up to 5 attempts, so the next try can
//! land on another frontend), and every open Execute and WaitExecution stream stays
//! open until its action is done. Buck2 retries only the opening of the call: it does
//! not recover a stream that breaks after it started (it never calls WaitExecution and
//! never sends the Execute again), so an action whose stream is cut fails as
//! `remote_call_error`. Ending a stream UNAVAILABLE is therefore a last resort, not a
//! retry (kbf#791).
//!
//! **Progress** ([`progress`]): every open stream is sent its operation again, same
//! name, `done` false, its current stage, once per progress interval, so a load
//! balancer or proxy that counts idle time in data frames never cuts a stream whose
//! action is long.
//!
//! When the drain runs out of time ([`Closer::close`]), every open Execute and
//! WaitExecution stream that is not done ends UNAVAILABLE (a client that retries
//! sees it; Buck2 fails the action); a WaitExecution opened after that ends UNAVAILABLE
//! after its first operation. A WaitExecution on an operation kept finished is answered even
//! then: its first operation is the done one, and the stream ends there.
//!
//! What the front reads of the platform: the `kbf-lease` value, `action` when absent, in the
//! [`Submission`] as `whole_machine` (the job needs a whole host) when it is `whole_machine`;
//! any other value than `action` or `whole_machine` is INVALID_ARGUMENT. The name is read
//! without regard to case (`Kbf-Lease` is `kbf-lease`), and one given twice, also in two
//! spellings, is INVALID_ARGUMENT. No other platform property is read or refused here.
//!
//! **When the platform is refused.** Before anything about the inputs is answered: an Action
//! that carries a platform is checked right after it is decoded, before its Command or any
//! input blob is read (so a bad platform beside missing blobs is INVALID_ARGUMENT for the
//! platform, not MISSING); a Command's platform as soon as the Command is read, which is while
//! the input tree is walked, and its refusal is the answer over whatever the walk found (a
//! MISSING input or a store that failed). An Execute the action cache answers is checked too:
//! when the CAS holds the action (and its Command), a refused platform is an error instead of
//! the cached result. An action the CAS does not hold, or that does not decode, is answered
//! from the cache.

use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::future::Either;
use futures::{Stream, stream};
use kbf_auth::{Authorizers, authorize};
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::google::rpc::{self, PreconditionFailure, precondition_failure::Violation};
use kbf_proto::reapi::execution_server::Execution;
use kbf_proto::reapi::{
    self, Action, Command, ExecuteOperationMetadata, ExecuteRequest, ExecuteResponse,
    WaitExecutionRequest, execution_stage,
};
use kbf_types::Digest;
use prost::Message;
use prost_types::Any;
use tokio::sync::watch;
use tokio::time::Instant;
use tonic::{Code, Request as GrpcRequest, Response, Status};
use tracing::Instrument as _;

use kbf_auth::AuthenticationMetadata;
use kbf_invocation::{CallKind, Evidence, ExecuteAnswer};

use crate::backend::Backend;
use crate::described::Described;
use crate::early_tree::HeldFinding;
use crate::metrics::{CacheMetrics, OpenExecute};
use crate::recorder::{ActionLabel, ExecuteCall, Recorder, listing};
use crate::request_metadata::CallMetadata;
use crate::wire;

/// The reserved platform key that names the lease kind
/// (`docs/platform-properties.md#kbf-lease`).
pub const LEASE_KIND_KEY: &str = "kbf-lease";

/// The lease kinds a request may name. The default is the first.
pub const LEASE_KINDS: [&str; 2] = ["action", "whole_machine"];

/// The `ErrorInfo.domain` of kbf's errors.
pub const ERROR_DOMAIN: &str = "kbf";

/// One execution the front hands to the dispatcher.
#[derive(Clone, Debug, PartialEq)]
pub struct Submission {
    /// The instance and action: the key identical requests are joined by.
    pub key: kbf_types::ActionKey,
    /// The platform's `kbf-lease` is `whole_machine`: the job needs a whole host.
    pub whole_machine: bool,
    /// The call to record, with a recorder on and an invocation id: the dispatcher
    /// sends its [`ExecuteCall::answered`] events before it starts the job.
    pub call: Option<ExecuteCall>,
    /// The action's argv, output paths and client label, for the server's `action
    /// usage:` line only ([`Described`]).
    pub described: Described,
    /// What the front knew at submit: the size of the input tree. For the run record only.
    pub evidence: Evidence,
    /// The Action and the Command of [`Submission::key`]'s action as this Execute decoded them,
    /// so that its run reads neither from the CAS again. `None` when the submitter has not
    /// decoded them; the dispatcher then reads them itself.
    pub decoded: Option<Arc<DecodedAction>>,
    /// When the Execute reached the front, before it was authorized or anything of its action
    /// read: the start of `kbf_execute_presubmit_seconds` (which ends later, at the call's first
    /// stream item). For the server's per-job line only.
    pub arrival: Arrival,
}

/// The Action an Execute read and decoded, and its Command: the two messages a run's facts and
/// its input closure are read from. Named by the action's digest ([`Submission::key`]), so what
/// they say is what the CAS holds under it.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedAction {
    /// The Action.
    pub action: Action,
    /// The Command its `command_digest` names.
    pub command: Command,
}

/// When an Execute reached the front, on the monotonic clock and on the wall clock, taken
/// together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arrival {
    /// The monotonic time (tokio's clock: paused in tests).
    pub at: Instant,
    /// The wall-clock time.
    pub unix: SystemTime,
}

impl Arrival {
    /// Now, on both clocks.
    #[must_use]
    pub fn now() -> Self {
        Self {
            at: Instant::now(),
            unix: SystemTime::now(),
        }
    }
}

/// Where an operation is, as its callers see it.
#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    /// Submitted, not yet read back running.
    Queued,
    /// Its job has started.
    Executing,
    /// Finished.
    Done(Finished),
}

/// How an operation finished.
#[derive(Clone, Debug, PartialEq)]
pub enum Finished {
    /// The action ran (whatever its exit code): its result.
    Ran(Box<reapi::ActionResult>),
    /// The farm could not run it: `INTERNAL` for an infrastructure failure,
    /// `DEADLINE_EXCEEDED` for a timeout, [`missing_status`] for an input the worker
    /// could not fetch.
    Failed(rpc::Status),
    /// It ran past its timeout and was stopped: `status` is `DEADLINE_EXCEEDED`, and
    /// `partial` holds what it wrote to stdout and stderr before then (REAPI's partial
    /// result; no outputs, never cached).
    TimedOut {
        status: rpc::Status,
        partial: Box<reapi::ActionResult>,
    },
}

/// A caller's handle on an operation: its name and its stage as it changes.
#[derive(Debug)]
pub struct Ticket {
    /// The operation name the caller may pass to WaitExecution. It must never name
    /// another operation, including one of a later server process: a client can still
    /// hold it after a restart (issue #154).
    pub name: String,
    /// The instance name the operation was submitted under, which WaitExecution is
    /// authorized against.
    pub instance: String,
    /// The action digest, for the operation's metadata.
    pub action: Digest,
    /// The stage, updated until [`Stage::Done`]. A sender dropped before then means the
    /// server is going away; the stream ends UNAVAILABLE.
    pub stage: watch::Receiver<Stage>,
}

/// The job service as the front reaches it.
pub trait Dispatch: Send + Sync + 'static {
    /// Starts the job of `submission` (or joins its running twin) and returns the caller's
    /// ticket.
    ///
    /// # Errors
    /// The dispatcher cannot take work (UNAVAILABLE).
    fn submit(&self, submission: Submission) -> Result<Ticket, Status>;

    /// A new ticket on the operation called `name`, if it is unfinished or finished
    /// within the dispatcher's retention (its ticket's stage is then already done). A
    /// dispatcher over a state store shared by several frontends answers a name any of them
    /// gave: a finished operation is kept as long as the store keeps its result. Also what
    /// GetOperation reads the stage of.
    fn wait(&self, name: &str) -> Option<Ticket>;

    /// Asks the job of the unfinished operation called `name` to stop; its callers are then
    /// answered CANCELLED. False when no unfinished operation has that name.
    fn cancel(&self, name: &str) -> bool;

    /// Starts writing the action tree of `action`, whose Action and Command are `decoded`,
    /// before its Execute has finished its input check: the Execute has read every Directory of
    /// the tree and is still asking the store whether the files are held. No operation is opened
    /// and nothing is recorded: the run that a later [`Dispatch::submit`] of the action starts
    /// takes the write instead of writing the tree itself. `finding` says whether the Execute
    /// found every input held; until it says so, nothing the write did may be trusted by a later
    /// run without a check. Called on the Execute's own task, so it must not block. By default
    /// nothing is started, and each run writes its own tree.
    fn start_tree(&self, action: Digest, decoded: Arc<DecodedAction>, finding: HeldFinding) {
        let _ = (action, decoded, finding);
    }
}

/// `f` of `dispatch`, on the runtime's blocking pool: a dispatcher may read or write the state
/// store it keeps its operations in, and those calls block.
///
/// # Errors
/// INTERNAL when the task that ran `f` broke.
pub(crate) async fn on_dispatch<D: Dispatch, T: Send + 'static>(
    dispatch: &Arc<D>,
    f: impl FnOnce(&D) -> T + Send + 'static,
) -> Result<T, Status> {
    let dispatch = Arc::clone(dispatch);
    tokio::task::spawn_blocking(move || f(&dispatch))
        .await
        .map_err(|e| Status::internal(format!("the dispatcher's call stopped: {e}")))
}

/// Where the server is on its way out, as the `Execution` service sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Serving.
    Open,
    /// No new Execute; open streams run to their end.
    Draining,
    /// Open streams that are not done end UNAVAILABLE.
    Closed,
}

/// Tells the `Execution` service the server is going away: see [`closing`].
#[derive(Debug)]
pub struct Closer(watch::Sender<Phase>);

impl Closer {
    /// Refuses every new Execute UNAVAILABLE from now on. Open streams are not
    /// touched: they run until their action is done (or [`Closer::close`]).
    pub fn drain(&self) {
        self.0.send_if_modified(|phase| {
            let open = *phase == Phase::Open;
            if open {
                *phase = Phase::Draining;
            }
            open
        });
    }

    /// Takes new Execute calls again after [`Closer::drain`]. Does nothing once the
    /// closer has closed. Returns whether it undid a drain.
    pub fn undrain(&self) -> bool {
        self.0.send_if_modified(|phase| {
            let draining = *phase == Phase::Draining;
            if draining {
                *phase = Phase::Open;
            }
            draining
        })
    }

    /// Ends every open Execute and WaitExecution stream that is not done UNAVAILABLE,
    /// and every one opened from now on after its first operation. Also drains.
    pub fn close(&self) {
        self.0.send_replace(Phase::Closed);
    }
}

/// The `Execution` service's side of a [`Closer`]. A stream ends when its closer
/// closes; if the closer is dropped without closing, it never does.
#[derive(Clone, Debug)]
pub struct Closing(watch::Receiver<Phase>);

impl Closing {
    /// Completes once the closer has closed; never, if it is dropped first.
    pub(crate) async fn closed(&mut self) {
        let dropped = self
            .0
            .wait_for(|phase| *phase == Phase::Closed)
            .await
            .is_err();
        if dropped {
            std::future::pending::<()>().await;
        }
    }

    /// Whether the closer has begun to drain or has closed.
    #[must_use]
    pub fn draining(&self) -> bool {
        *self.0.borrow() != Phase::Open
    }
}

/// A [`Closer`] and the [`Closing`] to hand the `Execution` service.
#[must_use]
pub fn closing() -> (Closer, Closing) {
    let (closer, closing) = watch::channel(Phase::Open);
    (Closer(closer), Closing(closing))
}

/// Drives the shared progress tick: see [`progress`].
#[derive(Debug)]
pub struct ProgressTicker {
    every: Duration,
    tick: watch::Sender<u64>,
}

impl ProgressTicker {
    /// Ticks every interval, for as long as it is polled; never, for a zero interval.
    /// One timer serves every open stream, however many: a stream holds a receiver and
    /// wakes once per tick.
    pub async fn run(self) {
        if self.every.is_zero() {
            std::future::pending::<()>().await;
        }
        let mut interval = tokio::time::interval_at(Instant::now() + self.every, self.every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            self.tick.send_modify(|n| *n = n.wrapping_add(1));
        }
    }
}

/// The `Execution` service's side of a [`ProgressTicker`]: each tick makes every open
/// stream send its operation again.
#[derive(Clone, Debug)]
pub struct Progress(watch::Receiver<u64>);

impl Progress {
    /// A `Progress` that never ticks (streams send only the stage changes).
    #[must_use]
    pub fn off() -> Self {
        Self(watch::channel(0).1)
    }

    /// Completes at the next tick; never, if there is no ticker (or it is dropped).
    pub(crate) async fn tick(&mut self) {
        if self.0.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// A ticker that fires every `every` once run, and the [`Progress`] to hand the
/// `Execution` service ([`ExecutionService::with_progress`]). A zero `every` is no
/// progress: the ticker never fires.
#[must_use]
pub fn progress(every: Duration) -> (ProgressTicker, Progress) {
    let (tick, rx) = watch::channel(0);
    (ProgressTicker { every, tick }, Progress(rx))
}

/// An Execute that spends this long before its first stream item is logged at `WARN`.
pub const PRESUBMIT_WARN: Duration = Duration::from_secs(10);

/// One Execute's time before its first stream item, from the handler's start: observed
/// in `kbf_execute_presubmit_seconds` when dropped, whether the call answered, failed
/// or was dropped by its caller (a stalled call the client gave up on is the one the
/// operator needs to see), and logged at `WARN` when it took [`PRESUBMIT_WARN`] or more. Its
/// start is also the [`Submission`]'s [`Arrival`].
struct Presubmit<'a> {
    metrics: &'a CacheMetrics,
    /// The handler's start, which the [`Submission`] carries as its [`Arrival`].
    arrival: Arrival,
    /// What the call is about, once its request is read; empty before (a call denied or
    /// dropped while authorizing).
    action: String,
    instance: String,
    invocation: String,
    walk: Duration,
    submit: Duration,
    dir_reads: u64,
    /// Of a walk over a store: Directories skipped as verified, the time reading
    /// Directories and the time asking about files, the FindMissing calls and the files.
    dirs_skipped: u64,
    /// With the content cache: Directories taken without a read, and the cached ones
    /// put to FindMissing.
    dir_cache_hits: u64,
    dirs_present_checked: u64,
    dirs: Duration,
    files: Duration,
    file_asks: u64,
    files_checked: u64,
    _in_flight: crate::metrics::Raised<'a>,
}

impl<'a> Presubmit<'a> {
    fn begin(metrics: &'a CacheMetrics) -> Self {
        Self {
            metrics,
            arrival: Arrival::now(),
            action: String::new(),
            instance: String::new(),
            invocation: String::new(),
            walk: Duration::ZERO,
            submit: Duration::ZERO,
            dir_reads: 0,
            dirs_skipped: 0,
            dir_cache_hits: 0,
            dirs_present_checked: 0,
            dirs: Duration::ZERO,
            files: Duration::ZERO,
            file_asks: 0,
            files_checked: 0,
            _in_flight: metrics.execute_presubmit_in_flight.raise(),
        }
    }
}

impl Drop for Presubmit<'_> {
    fn drop(&mut self) {
        let elapsed = self.arrival.at.elapsed();
        self.metrics.execute_presubmit.observe_duration(elapsed);
        if elapsed < PRESUBMIT_WARN {
            return;
        }
        tracing::warn!(
            action = %self.action,
            instance = %self.instance,
            invocation = %self.invocation,
            presubmit_ms = elapsed.as_millis(),
            walk_ms = self.walk.as_millis(),
            submit_ms = self.submit.as_millis(),
            dir_reads = self.dir_reads,
            dirs_skipped = self.dirs_skipped,
            dir_cache_hits = self.dir_cache_hits,
            dirs_present_checked = self.dirs_present_checked,
            dirs_ms = self.dirs.as_millis(),
            files_ms = self.files.as_millis(),
            file_asks = self.file_asks,
            files_checked = self.files_checked,
            "Execute spent {PRESUBMIT_WARN:?} or more before its first operation"
        );
    }
}

/// The `Execution` service over a [`Backend`] (a cache, or a storage node) and a
/// [`Dispatch`].
#[derive(Debug)]
pub struct ExecutionService<D> {
    cache: Backend,
    dispatch: Arc<D>,
    closing: Closing,
    progress: Progress,
    authorizers: Arc<Authorizers>,
    recorder: Recorder,
}

/// The gRPC path of Execute, as calls are logged.
const EXECUTE: &str = "/build.bazel.remote.execution.v2.Execution/Execute";

/// The gRPC path of WaitExecution, as calls are logged.
const WAIT_EXECUTION: &str = "/build.bazel.remote.execution.v2.Execution/WaitExecution";

impl<D> ExecutionService<D> {
    /// The service over `cache` (a cache or a [`Backend`]) and `dispatch`, whose
    /// streams end when `closing` does, each call authorized by `authorizers`.
    #[must_use]
    pub fn with_authorizers(
        cache: impl Into<Backend>,
        dispatch: Arc<D>,
        closing: Closing,
        authorizers: Arc<Authorizers>,
    ) -> Self {
        Self {
            cache: cache.into(),
            dispatch,
            closing,
            progress: Progress::off(),
            authorizers,
            recorder: Recorder::off(),
        }
    }

    /// This service, each open stream sent its operation again at every tick of
    /// `progress`.
    #[must_use]
    pub fn with_progress(mut self, progress: Progress) -> Self {
        self.progress = progress;
        self
    }

    /// This service, each Execute that carries an invocation id sent to `recorder`:
    /// an answer from the action cache here, a queued or joined one through the
    /// [`Submission`].
    #[must_use]
    pub fn recording(mut self, recorder: Recorder) -> Self {
        self.recorder = recorder;
        self
    }
}

/// The stream of operations an Execute or WaitExecution call returns.
pub type OperationStream = Pin<Box<dyn Stream<Item = Result<Operation, Status>> + Send>>;

#[tonic::async_trait]
impl<D> Execution for ExecutionService<D>
where
    D: Dispatch,
{
    type ExecuteStream = OperationStream;
    type WaitExecutionStream = OperationStream;

    async fn execute(
        &self,
        request: GrpcRequest<ExecuteRequest>,
    ) -> Result<Response<OperationStream>, Status> {
        if self.closing.draining() {
            return Err(Status::unavailable(
                "the server is draining and takes no new Execute; try again (Buck2 retries \
                 this, and the next attempt can reach another frontend)",
            ));
        }
        let mut window = Presubmit::begin(self.cache.metrics());
        let mut open = self.cache.metrics().execute.execute();
        open.connection(kbf_tls::liveness(&request));
        self.cache.metrics().execute.started.add(1);
        let caller = kbf_auth::metadata(&request);
        let call = crate::request_metadata::call_metadata(&request);
        open.build(call.invocation_id.clone());
        let request = request.into_inner();
        let instance = request.instance_name.as_str();
        authorize(&*self.authorizers.execute, &caller, EXECUTE, instance)
            .await
            .inspect_err(|_| open.fail())?;
        let span = tracing::info_span!(
            "execute",
            instance,
            caller = %caller.public_display(),
            invocation = call.invocation_display()
        );
        let answered = self
            .execute_authorized(request, &call, &caller, &mut window, &mut open)
            .instrument(span)
            .await;
        match answered {
            Ok(Either::Left(ticket)) => Ok(Response::new(operations(
                ticket,
                self.closing.clone(),
                self.progress.clone(),
                open,
            ))),
            Ok(Either::Right(cached)) => Ok(cached),
            Err(status) => {
                open.fail();
                Err(status)
            }
        }
    }

    async fn wait_execution(
        &self,
        request: GrpcRequest<WaitExecutionRequest>,
    ) -> Result<Response<OperationStream>, Status> {
        let caller = kbf_auth::metadata(&request);
        let call = crate::request_metadata::call_metadata(&request);
        let name = request.into_inner().name;
        let asked = name.clone();
        let Some(ticket) = on_dispatch(&self.dispatch, move |d| d.wait(&asked)).await? else {
            return Err(Status::not_found(format!(
                "no operation {name:?}: finished operations are kept only briefly; \
                 Execute answers a finished one from the action cache"
            )));
        };
        let execute = &*self.authorizers.execute;
        authorize(execute, &caller, WAIT_EXECUTION, &ticket.instance).await?;
        let mut open = self.cache.metrics().execute.wait();
        open.build(call.invocation_id.clone());
        Ok(Response::new(operations(
            ticket,
            self.closing.clone(),
            self.progress.clone(),
            open,
        )))
    }
}

impl<D> ExecutionService<D>
where
    D: Dispatch,
{
    /// Execute, once authorized: an action-cache hit (`Right`), or the ticket of the
    /// submitted operation (`Left`), which the caller makes a stream. `open` is the call's
    /// metrics guard (started before authorization); a cache hit marks it answered.
    async fn execute_authorized(
        &self,
        request: ExecuteRequest,
        metadata: &CallMetadata,
        caller: &AuthenticationMetadata,
        window: &mut Presubmit<'_>,
        open: &mut OpenExecute,
    ) -> Result<Either<Ticket, Response<OperationStream>>, Status> {
        wire::check_digest_function(request.digest_function)?;
        let action = wire::digest(request.action_digest.as_ref())?;
        window.action = action.to_string();
        window.instance.clone_from(&request.instance_name);
        metadata
            .invocation_display()
            .clone_into(&mut window.invocation);
        // The Action is read while the action cache is looked up: a miss reads its inputs
        // from it next, and a hit checks its platform with it. A failed lookup is the
        // answer, whatever the read found.
        let lookup = async {
            match request.skip_cache_lookup {
                true => Ok(None),
                false => self.cache.action_result(&action).await,
            }
        };
        let (looked, read) = futures::future::join(lookup, self.read_input(&action)).await;
        if let Some((stored, result)) = looked? {
            let name = format!(
                "operations/cached/{}-{}",
                action.hash_hex(),
                action.size_bytes
            );
            // A result is not an excuse for a platform kbf refuses.
            self.refuse_platform_of(read?).await?;
            tracing::debug!(%action, "answered from the action cache");
            let instance = &request.instance_name;
            self.recorder
                .call(metadata, caller, instance, &|| CallKind::Execute {
                    action,
                    answer: ExecuteAnswer::Cached {
                        name: name.clone(),
                        result: Box::new(listing(Some(stored), &result)),
                    },
                });
            let done = operation(
                &name,
                &action,
                &Stage::Done(Finished::Ran(Box::new(result))),
                true,
            );
            open.ok();
            return Ok(Either::Right(Response::new(Box::pin(stream::iter([Ok(
                done,
            )])))));
        }
        let instance = request.instance_name;
        let (mut submission, command) = self
            .submission(instance.clone(), action, read?, window)
            .await?;
        submission.call =
            ExecuteCall::new(&self.recorder, metadata, caller, &instance, action, command);
        submission.described.label = ActionLabel::sent(metadata);
        let submitting = Instant::now();
        open.dispatching();
        let submitted = on_dispatch(&self.dispatch, move |d| d.submit(submission))
            .await
            .and_then(|submitted| submitted);
        open.dispatched(submitted.is_ok());
        window.submit = submitting.elapsed();
        self.cache
            .metrics()
            .execute_submit
            .observe_duration(window.submit);
        let ticket = submitted?;
        tracing::debug!(%action, operation = %ticket.name, "submitted");
        Ok(Either::Left(ticket))
    }
}

/// The action's platform. REAPI 2.2 moved it into the `Action`; older clients set it
/// on the `Command` only, which REAPI still asks servers to read.
#[allow(deprecated)]
fn platform<'a>(action: &'a Action, command: Option<&'a Command>) -> Option<&'a reapi::Platform> {
    action
        .platform
        .as_ref()
        .filter(|p| !p.properties.is_empty())
        .or_else(|| command.and_then(|c| c.platform.as_ref()))
}

fn required(d: Option<&reapi::Digest>, field: &str) -> Result<Digest, Status> {
    match d {
        Some(d) => wire::digest(Some(d)),
        None => Err(Status::invalid_argument(format!(
            "the action has no {field}"
        ))),
    }
}

/// Whether the platform of an action (the Action's, else the Command's) asks for a whole
/// machine: its `kbf-lease` value is `whole_machine` (`action` or none is not). The name is
/// read without regard to case. INVALID_ARGUMENT for a value not in [`LEASE_KINDS`], and for
/// the name given twice, also in two spellings. Reads nothing else of the platform, and no
/// input of the action: it runs before anything about the inputs is answered, and for a
/// cached result too (`ExecutionService::refuse_platform_of`).
fn wants_whole_machine(action: &Action, command: Option<&Command>) -> Result<bool, Status> {
    let properties = platform(action, command).map_or(&[][..], |p| p.properties.as_slice());
    let mut named = properties
        .iter()
        .filter(|p| p.name.eq_ignore_ascii_case(LEASE_KIND_KEY));
    let Some(lease) = named.next() else {
        return Ok(false);
    };
    if let Some(again) = named.next() {
        return Err(Status::invalid_argument(format!(
            "platform property {LEASE_KIND_KEY:?} is given twice (as {:?} and as {:?})",
            lease.name, again.name
        )));
    }
    if !LEASE_KINDS.contains(&lease.value.as_str()) {
        return Err(Status::invalid_argument(format!(
            "platform property {LEASE_KIND_KEY}={:?} is not one of {LEASE_KINDS:?}",
            lease.value
        )));
    }
    Ok(lease.value == "whole_machine")
}

/// How many of the digests an Execute refused for missing inputs a log line names.
const REFUSED_LOGGED: usize = 5;

/// FAILED_PRECONDITION with one `MISSING` violation per blob, as REAPI asks.
fn missing(digests: &[Digest]) -> Status {
    let status = missing_status(digests);
    Status::with_details(
        Code::FailedPrecondition,
        status.message.clone(),
        Bytes::from(status.encode_to_vec()),
    )
}

/// The status REAPI asks for when inputs are not in the CAS: FAILED_PRECONDITION with
/// exactly one detail, a `PreconditionFailure` with one `MISSING` violation per blob
/// (subject `blobs/<hash>/<size>`). Bazel re-uploads the blobs and executes again only
/// on this shape: any other detail makes the error permanent. The front answers an
/// Execute with it before queueing, and the server answers an operation with it when
/// the worker could not fetch an input.
#[must_use]
pub fn missing_status(digests: &[Digest]) -> rpc::Status {
    let failure = PreconditionFailure {
        violations: digests
            .iter()
            .map(|d| Violation {
                r#type: "MISSING".to_owned(),
                subject: format!("blobs/{}/{}", d.hash_hex(), d.size_bytes),
                description: String::new(),
            })
            .collect(),
    };
    rpc::Status {
        code: Code::FailedPrecondition as i32,
        message: format!("{} input blob(s) are not in the CAS", digests.len()),
        details: vec![any(
            "google.rpc.PreconditionFailure",
            failure.encode_to_vec(),
        )],
    }
}

fn any(type_name: &str, value: Vec<u8>) -> Any {
    Any {
        type_url: format!("type.googleapis.com/{type_name}"),
        value,
    }
}

/// The operation `name` at `stage`.
pub(crate) fn operation(name: &str, action: &Digest, stage: &Stage, cached: bool) -> Operation {
    let (value, result) = match stage {
        Stage::Queued => (execution_stage::Value::Queued, None),
        Stage::Executing => (execution_stage::Value::Executing, None),
        Stage::Done(finished) => {
            let response = match finished {
                Finished::Ran(result) => ExecuteResponse {
                    result: Some((**result).clone()),
                    cached_result: cached,
                    status: Some(wire::rpc_ok()),
                    ..ExecuteResponse::default()
                },
                Finished::Failed(status) => ExecuteResponse {
                    status: Some(status.clone()),
                    ..ExecuteResponse::default()
                },
                Finished::TimedOut { status, partial } => ExecuteResponse {
                    result: Some((**partial).clone()),
                    status: Some(status.clone()),
                    ..ExecuteResponse::default()
                },
            };
            let response = any(
                "build.bazel.remote.execution.v2.ExecuteResponse",
                response.encode_to_vec(),
            );
            (
                execution_stage::Value::Completed,
                Some(operation::Result::Response(response)),
            )
        }
    };
    let metadata = ExecuteOperationMetadata {
        stage: value as i32,
        action_digest: Some(wire::digest_to_proto(action)),
        digest_function: reapi::digest_function::Value::Sha256 as i32,
        ..ExecuteOperationMetadata::default()
    };
    Operation {
        name: name.to_owned(),
        metadata: Some(any(
            "build.bazel.remote.execution.v2.ExecuteOperationMetadata",
            metadata.encode_to_vec(),
        )),
        done: result.is_some(),
        result,
    }
}

/// The operation as its stage changes: the stage now, then each change, ending after
/// the done operation. A stage that stops changing before it is done (the operation was
/// abandoned), or `closing` closing first (the server is going away), ends the stream
/// UNAVAILABLE. A change already there is sent before a close is
/// noticed. At every tick of `progress` the operation is sent again at its current stage.
/// `open` (the call's metrics guard) is dropped with the stream, and told of each stage
/// read.
fn operations(
    ticket: Ticket,
    closing: Closing,
    progress: Progress,
    mut open: OpenExecute,
) -> OperationStream {
    // Before the first item is read, so a stream the client has not yet polled is open.
    open.streaming();
    let Ticket {
        name,
        action,
        stage,
        ..
    } = ticket;
    let state = Some((stage, closing, progress, open, true));
    Box::pin(stream::unfold(state, move |state| {
        let name = name.clone();
        async move {
            let (mut stage, mut closing, mut progress, mut open, first) = state?;
            if !first {
                let gone = tokio::select! {
                    biased;
                    changed = stage.changed() => changed
                        .is_err()
                        .then(|| format!("operation {name} was abandoned")),
                    () = closing.closed() => Some(format!(
                        "the server is shutting down; operation {name} ends here, Execute it again"
                    )),
                    () = progress.tick() => None,
                };
                if let Some(why) = gone {
                    open.fail();
                    return Some((Err(Status::unavailable(why)), None));
                }
            }
            let now = stage.borrow_and_update().clone();
            let done = matches!(now, Stage::Done(_));
            open.update(matches!(now, Stage::Queued), done);
            let next = (!done).then_some((stage, closing, progress, open, false));
            Some((Ok(operation(&name, &action, &now, false)), next))
        }
    }))
}

#[path = "execution_inputs.rs"]
mod inputs;

#[cfg(test)]
#[path = "execution_progress_tests.rs"]
mod progress_tests;
