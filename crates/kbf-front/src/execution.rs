//! `Execution`: Execute and WaitExecution, over the cache and a [`Dispatch`].
//!
//! Execute does steps 1 to 3 of `ARCHITECTURE.md#executing-an-action`, then streams the
//! operation:
//! - the action cache is checked first (with the closure check, unless the client set
//!   `skip_cache_lookup`); a hit answers at once, `cached_result` set, and nothing runs;
//! - the `Action`, its `Command` and its whole input tree must be in the CAS; every blob
//!   that is not is listed in one FAILED_PRECONDITION, a `MISSING` violation each;
//! - the request goes to the [`Dispatch`] (the scheduler), which joins a running twin
//!   (in-flight dedup) or queues a new operation;
//! - the call streams the operation: `QUEUED`, `EXECUTING` once a lease for it has
//!   started, then the done operation with its `ExecuteResponse`. While no live worker
//!   can run it, the `QUEUED` operation's metadata says why, as a `google.rpc.ErrorInfo`
//!   (reason [`NO_WORKER_REASON`], domain [`ERROR_DOMAIN`], the explanation under
//!   metadata key `why`) in `partial_execution_metadata.auxiliary_metadata`.
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
//! The one early end: a stream whose operation has never been started and cannot start
//! (stage [`Stage::Waiting`]: no worker on this frontend can run it, for example every
//! capable worker is cordoned, and no other frontend of the state store has one that
//! could take it) ends UNAVAILABLE when the drain begins, which is the end of the load
//! balancer wait. That action would fail at the drain deadline anyway, so ending it costs
//! nothing and lets the stop finish. A `Queued` operation a worker can take, an operation
//! another frontend could take, and any operation that ever had a lease (running, or
//! queued again after a lost one: [`Stage::WaitingKept`]), keep their streams. The
//! operation is not cancelled by the stream ending.
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
//! What v0 sends to the scheduler: QoS `ci`, or the level the `x-kbf-qos` header names
//! where [`QosAllow`] grants it ([`crate::qos_header`]), and every action is hermetic, so it may be joined (a `networked`
//! property does not exist yet). The lease kind is the platform's `kbf-lease` value,
//! `action` when absent; any other value than `action` or `whole_machine` is
//! INVALID_ARGUMENT. The booking of an `action` is one core and 4 GiB by default
//! ([`DEFAULT_RESOURCES`], the first rung of the scheduler's memory ladder, which the
//! server sets with [`ExecutionService::with_default_memory`]; learned sizes come with
//! the estimator), or the whole cores of `kbf-book-cpus` and the GiB of
//! `kbf-book-mem-gib` where the platform names them: a value that is not a whole number
//! of at least 1, or that overflows the booking, is INVALID_ARGUMENT, and so is either
//! key on a `whole_machine` lease (the scheduler books the whole worker for one). The
//! platform's `gpu` value is the number of whole GPUs to book on top (0 when absent); a
//! value that is not a whole number is INVALID_ARGUMENT. On macOS (a platform whose
//! `OSFamily` or `os` names it) a Mac has one GPU, a slot it reports as `gpu` 1: an
//! `action` lease books it with `gpu=1`, so one GPU lease runs on a Mac at a time while
//! its CPU is shared, and `gpu` of 2 or more is INVALID_ARGUMENT. A `whole_machine`
//! lease books no GPU on top (it has the whole Mac, its GPU included).
//!
//! The rest of the platform says which workers may run the action
//! (`kbf_caps::Request::from_platform`: `OSFamily`, `ISA`, `Arch` and kbf's capability
//! keys). A malformed one is INVALID_ARGUMENT: among others, an `ios.device` other than
//! `1`, an `ios.device.<attribute>` without `ios.device=1` or that kbf does not know,
//! and `ios.device` with an OS other than macOS or on a `whole_machine` lease. One no
//! kbf daemon can ever
//! run (an OS other than Linux or macOS, an architecture other than x86-64 or arm64) is
//! FAILED_PRECONDITION at once, with no `PreconditionFailure` detail: there is nothing
//! for the client to upload, and Bazel and Buck2 do not retry it.
//!
//! Every property name kbf reads is read without regard to case
//! (`kbf_caps::property_name`): `GPU`, `Kbf-Lease` and `osfamily` are `gpu`,
//! `kbf-lease` and `OSFamily`, never ignored. One name sent in two spellings is
//! INVALID_ARGUMENT.
//!
//! **When the platform is refused.** As early as it is known, before any input blob is
//! read: an Action that carries a platform is checked right after it is decoded (so a bad
//! platform beside missing blobs is INVALID_ARGUMENT or FAILED_PRECONDITION for the
//! platform, not MISSING), and a Command's platform as soon as the Command is read. An
//! Execute the action cache answers is checked too: when the CAS holds the action (and its
//! Command), a refused platform is an error instead of the cached result. An action the
//! CAS does not hold, or that does not decode, is answered from the cache.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::future::Either;
use futures::{Stream, stream};
use kbf_auth::{Authorizers, authorize};
use kbf_caps::FromPlatformError;
use kbf_proto::google::longrunning::{Operation, operation};
use kbf_proto::google::rpc::{
    self, ErrorInfo, PreconditionFailure, precondition_failure::Violation,
};
use kbf_proto::reapi::execution_server::Execution;
use kbf_proto::reapi::{
    self, Action, Command, ExecuteOperationMetadata, ExecuteRequest, ExecuteResponse,
    ExecutedActionMetadata, WaitExecutionRequest, execution_stage,
};
use kbf_sched::Request;
use kbf_storage::StorageError;
use kbf_types::{ActionKey, Digest, LeaseKind, Platform, Qos, Resources};
use prost::Message;
use prost_types::Any;
use tokio::sync::watch;
use tokio::time::Instant;
use tonic::{Code, Request as GrpcRequest, Response, Status};
use tracing::Instrument as _;

use kbf_auth::AuthenticationMetadata;
use kbf_invocation::{BookSource, CallKind, Evidence, ExecuteAnswer};

use crate::backend::Backend;
use crate::default_platform::{DefaultPlatform, Quiet};
use crate::described::Described;
use crate::metrics::{CacheMetrics, OpenExecute};
use crate::qos_header::{Chosen, QosAllow};
use crate::recorder::{ActionLabel, ExecuteCall, Recorder, listing};
use crate::request_metadata::CallMetadata;
use crate::wire;

/// The reserved platform key that names the lease kind
/// (`docs/platform-properties.md#kbf-lease`).
pub const LEASE_KIND_KEY: &str = "kbf-lease";

/// The lease kinds a request may name. The default is the first.
pub const LEASE_KINDS: [&str; 2] = ["action", "whole_machine"];

/// The platform key that asks for whole GPUs, each booked for the lease alone.
pub const GPU_KEY: &str = "gpu";

/// The reserved platform key that books this many whole cores instead of one.
pub const BOOK_CPUS_KEY: &str = "kbf-book-cpus";

/// The reserved platform key that books this many GiB of memory instead of one.
pub const BOOK_MEM_GIB_KEY: &str = "kbf-book-mem-gib";

/// The `ErrorInfo.reason` of a queued operation no live worker can run.
pub const NO_WORKER_REASON: &str = "NO_WORKER_CAN_RUN";

/// The `ErrorInfo.domain` of kbf's errors.
pub const ERROR_DOMAIN: &str = "kbf";

/// What v0 books for an action that names no size: one core and 4 GiB, the first rung
/// of the scheduler's default memory ladder, plus the GPUs its `gpu` property asks
/// for. `kbf-book-cpus` and `kbf-book-mem-gib` replace either, and the server's first
/// rung ([`ExecutionService::with_default_memory`]) replaces the memory.
pub const DEFAULT_RESOURCES: Resources = Resources::new(1_000, 4 << 30);

/// The least worker a lease of a kind other than `action` may run on: one core and
/// 1 GiB. A `whole_machine` lease books the whole worker whatever this says.
const LEAST_WORKER: Resources = Resources::new(1_000, 1 << 30);

/// One execution the front hands to the scheduler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    /// What to run, keyed for dedup, with the lease kind from the platform's
    /// `kbf-lease`.
    pub request: Request,
    /// The call to record, with a recorder on and an invocation id: the dispatcher
    /// sends its [`ExecuteCall::answered`] events before it queues the request.
    pub call: Option<ExecuteCall>,
    /// The action's argv, output paths and client label, for the server's `action
    /// usage:` line only ([`Described`]).
    pub described: Described,
    /// What the front knew at submit that the booking does not say: which axes the
    /// client named, and the size of the input tree. For the run record only; it is
    /// not part of the scheduler's [`Request`], which the log codec writes.
    pub evidence: Evidence,
    /// The longest the action may run: its `timeout`, `None` when it names none or names
    /// zero (the daemon then applies its own default, which the front does not know). The
    /// scheduler reads it for the drain order and the deadlines of whole-machine requests
    /// when those are on (`Scheduler::set_max_wall`); it is not part of the [`Request`].
    pub max_wall: Option<Duration>,
}

/// Where an operation is, as its callers see it.
#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    /// Waiting for room.
    Queued,
    /// Queued, never started, and no live worker of this frontend can run it now: why. A
    /// drain ends the streams of an operation in this stage, since it can never start: no
    /// other frontend of the state store has a worker that could take it either.
    Waiting(String),
    /// As [`Stage::Waiting`], but a drain keeps its streams: a lease for the operation did
    /// start once (and was lost, so it was queued again), or another frontend of the
    /// state store has a worker that could run it, which that frontend's intake takes
    /// from the store's queue.
    WaitingKept(String),
    /// A lease for it has started on a worker.
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

/// The scheduler as the front reaches it.
pub trait Dispatch: Send + Sync + 'static {
    /// Queues `submission` (or joins its running twin) and returns the caller's ticket.
    ///
    /// # Errors
    /// The scheduler cannot take work (UNAVAILABLE).
    fn submit(&self, submission: Submission) -> Result<Ticket, Status>;

    /// A new ticket on the operation called `name`, if it is unfinished or finished
    /// within the dispatcher's retention (its ticket's stage is then already done). A
    /// dispatcher over a state store shared by several frontends answers a name any of them
    /// gave: a finished operation is kept as long as the store keeps its result. Also what
    /// GetOperation reads the stage of.
    fn wait(&self, name: &str) -> Option<Ticket>;
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

    /// Completes once the closer has begun to drain or has closed; never, if it is
    /// dropped first.
    pub(crate) async fn drained(&mut self) {
        let dropped = self
            .0
            .wait_for(|phase| *phase != Phase::Open)
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
/// operator needs to see), and logged at `WARN` when it took [`PRESUBMIT_WARN`] or more.
struct Presubmit<'a> {
    metrics: &'a CacheMetrics,
    started: Instant,
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
            started: Instant::now(),
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
        let elapsed = self.started.elapsed();
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
    /// The memory an `action` that names no size books, in bytes.
    default_memory: u64,
    /// The OS and architecture of an `action` that names neither.
    default_platform: DefaultPlatform,
    /// Holds the log lines of [`Self::default_platform`] to a few a minute.
    defaulted_log: Quiet,
    /// The levels the `x-kbf-qos` header may name.
    qos_allow: QosAllow,
    /// Every `action` lease owns its whole worker ([`Self::with_exclusive_actions`]).
    exclusive_actions: bool,
    /// Holds the log lines of a header that asked for a level it was not granted to a
    /// few a minute.
    qos_log: Quiet,
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
            default_memory: DEFAULT_RESOURCES.memory_bytes,
            default_platform: DefaultPlatform::NONE,
            defaulted_log: Quiet::default(),
            qos_allow: QosAllow::DEFAULT,
            exclusive_actions: false,
            qos_log: Quiet::default(),
        }
    }

    /// This service, granting the levels in `allow` to a call whose `x-kbf-qos` header
    /// names one (instead of [`QosAllow::DEFAULT`]).
    #[must_use]
    pub const fn with_qos_allow(mut self, allow: QosAllow) -> Self {
        self.qos_allow = allow;
        self
    }

    /// This service, asking the scheduler that every `action` lease own the whole worker it
    /// lands on, all its CPUs and all its memory, so that one action runs per worker (instead
    /// of booking what the platform names or the default). Placement only: the action digest
    /// is the client's, and `kbf-book-cpus` and `kbf-book-mem-gib` are still read, checked
    /// and recorded as hints but do not change what is booked. A `whole_machine` or `vm`
    /// lease is as it was.
    #[must_use]
    pub const fn with_exclusive_actions(mut self, on: bool) -> Self {
        self.exclusive_actions = on;
        self
    }

    /// This service, giving an `action` that names no OS, no architecture and no ISA
    /// level (and asks for nothing only a Mac reports) the OS and architecture of
    /// `platform` (instead of none): placement only, the digest is the client's.
    #[must_use]
    pub const fn with_default_platform(mut self, platform: DefaultPlatform) -> Self {
        self.default_platform = platform;
        self
    }

    /// This service, booking `bytes` of memory for an `action` that names no size
    /// (instead of [`DEFAULT_RESOURCES`]): the first rung of the scheduler's ladder.
    #[must_use]
    pub const fn with_default_memory(mut self, bytes: u64) -> Self {
        self.default_memory = bytes;
        self
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
        let chosen = self
            .qos_allow
            .choose(&request.metadata().clone().into_headers());
        self.log_ignored_qos(&chosen);
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
            .execute_authorized(request, &call, &caller, chosen.qos, &mut window, &mut open)
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
        let name = request.into_inner().name;
        let Some(ticket) = self.dispatch.wait(&name) else {
            return Err(Status::not_found(format!(
                "no operation {name:?}: finished operations are kept only briefly; \
                 Execute answers a finished one from the action cache"
            )));
        };
        let execute = &*self.authorizers.execute;
        authorize(execute, &caller, WAIT_EXECUTION, &ticket.instance).await?;
        let open = self.cache.metrics().execute.wait();
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
        qos: Qos,
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
        if !request.skip_cache_lookup
            && let Some((stored, result)) = self.cache.action_result(&action).await?
        {
            let name = format!(
                "operations/cached/{}-{}",
                action.hash_hex(),
                action.size_bytes
            );
            // A result is not an excuse for a platform kbf refuses.
            self.refuse_platform_of(&action).await?;
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
            .submission(instance.clone(), action, qos, window)
            .await?;
        submission.call =
            ExecuteCall::new(&self.recorder, metadata, caller, &instance, action, command);
        submission.described.label = ActionLabel::sent(metadata);
        let submitting = Instant::now();
        open.dispatching();
        let submitted = self.dispatch.submit(submission);
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

impl<D> ExecutionService<D> {
    /// Reads the action and checks its inputs are all held; the scheduler's request,
    /// with no call to record yet, and the Command digest the action names.
    async fn submission(
        &self,
        instance: String,
        action: Digest,
        qos: Qos,
        window: &mut Presubmit<'_>,
    ) -> Result<(Submission, Digest), Status> {
        let Some(bytes) = self.read_input(&action).await? else {
            return Err(missing(&[action]));
        };
        let decoded = Action::decode(bytes).map_err(|e| {
            Status::invalid_argument(format!("action {action} does not decode: {e}"))
        })?;
        // A platform the Action carries is refused before anything else is read.
        let own = if platform(&decoded, None).is_some() {
            Some(self.place(&decoded, None)?)
        } else {
            None
        };
        let command_digest = required(decoded.command_digest.as_ref(), "command_digest")?;
        let root = required(decoded.input_root_digest.as_ref(), "input_root_digest")?;

        let mut absent = Vec::new();
        let command = match self.read_input(&command_digest).await? {
            Some(bytes) => Some(Command::decode(bytes).map_err(|e| {
                Status::invalid_argument(format!("command {command_digest} does not decode: {e}"))
            })?),
            None => {
                absent.push(command_digest);
                None
            }
        };
        // And a Command's, as soon as the Command is read: before the input tree is walked.
        let placed = match own {
            Some(placed) => placed,
            None => self.place(&decoded, command.as_ref())?,
        };
        let walking = Instant::now();
        let walked = self.cache.walk_inputs(root).await;
        window.walk = walking.elapsed();
        self.cache
            .metrics()
            .execute_walk
            .observe_duration(window.walk);
        let walked = walked?;
        let (input_bytes, input_files) = (walked.input_bytes, walked.input_files);
        window.dir_reads += walked.dirs_read;
        window.dirs_skipped += walked.dirs_skipped;
        window.dir_cache_hits += walked.dir_cache_hits;
        window.dirs_present_checked += walked.dirs_present_checked;
        window.dirs += walked.dirs_time;
        window.files += walked.files_time;
        window.file_asks += walked.file_asks;
        window.files_checked += walked.files_checked;
        self.observe_phases(&walked);
        absent.extend(walked.absent);
        if !absent.is_empty() {
            tracing::warn!(
                %action,
                count = absent.len(),
                first = %absent
                    .iter()
                    .take(REFUSED_LOGGED)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                "execute refused: input blob(s) are not in the CAS"
            );
            return Err(missing(&absent));
        }

        let Placed {
            kind,
            booked,
            sources,
            needs,
            defaulted,
        } = placed;
        if defaulted {
            self.log_defaulted(&action);
        }
        let submission = Submission {
            request: Request {
                key: ActionKey { instance, action }.into(),
                qos,
                kind,
                resources: booked,
                hermetic: true,
                do_not_cache: decoded.do_not_cache,
                whole_worker: self.exclusive_actions && kind == LeaseKind::Action,
                needs,
            },
            call: None,
            // The Command is here: an absent one returned MISSING above.
            described: command.map(Described::of).unwrap_or_default(),
            evidence: Evidence {
                cpu_source: sources.cpu,
                memory_source: sources.memory,
                input_bytes,
                input_files,
            },
            max_wall: wall_of(&decoded),
        };
        Ok((submission, command_digest))
    }

    /// What the platform of an action asks of the scheduler, or the status that refuses
    /// it: INVALID_ARGUMENT for a malformed platform, FAILED_PRECONDITION for one no
    /// daemon can run. Reads nothing: it runs before any input of the action is read,
    /// and for a cached result too ([`Self::refuse_platform_of`]).
    fn place(&self, action: &Action, command: Option<&Command>) -> Result<Placed, Status> {
        let platform = properties(platform(action, command))?;
        let kind = lease_kind(&platform)?;
        let (booked, sources) = booking(&platform, kind, self.default_memory)?;
        let asked = needs(&platform)?;
        let (needs, defaulted) = match self.default_platform.apply(&asked) {
            Some(placed) => (placed, true),
            None => (asked, false),
        };
        let gpus = mac_gpus(kind, gpus(&platform)?, &needs)?;
        device_kind(kind, &needs)?;
        Ok(Placed {
            kind,
            booked: booked.with_gpus(gpus),
            sources,
            needs,
            defaulted,
        })
    }

    /// Logs, at most a few a minute, that the default platform placed `action`
    /// ([`DefaultPlatform::apply`]). Placement only: the action's digest is the client's.
    fn log_defaulted(&self, action: &Digest) {
        if let Some(held_back) = self.defaulted_log.turn() {
            tracing::info!(
                %action,
                default_platform = %self.default_platform,
                held_back,
                "platform defaulted: the action named no OS and no architecture"
            );
        }
    }

    /// Logs, at most a few a minute, a call whose `x-kbf-qos` header asked for a level
    /// it was not granted (or named none): it runs at `ci`, and is not refused.
    fn log_ignored_qos(&self, chosen: &Chosen) {
        let Some(asked) = &chosen.ignored else {
            return;
        };
        if let Some(held_back) = self.qos_log.turn() {
            tracing::warn!(
                asked = ?asked,
                granted = %chosen.qos,
                held_back,
                "x-kbf-qos ignored: not a level this front end grants (--qos-allowed); running at ci"
            );
        }
    }

    /// Refuses, for an Execute the action cache answers, the platform of the action that
    /// cached result is for, if the action and its Command are in the CAS: the Action's
    /// platform, else the Command's, as an Execute that ran would have read it. An action
    /// or Command the CAS does not hold, or that does not decode, is not refused here (an
    /// Execute that runs says so); a store that fails is the call's error.
    async fn refuse_platform_of(&self, action: &Digest) -> Result<(), Status> {
        let Some(decoded) = self.held::<Action>(action).await? else {
            return Ok(());
        };
        let command = match decoded
            .command_digest
            .as_ref()
            .map(|d| wire::digest(Some(d)))
        {
            Some(Ok(digest)) if platform(&decoded, None).is_none() => {
                self.held::<Command>(&digest).await?
            }
            _ => None,
        };
        self.place(&decoded, command.as_ref()).map(drop)
    }

    /// The message `digest` names, if the CAS holds it and it decodes.
    async fn held<T: prost::Message + Default>(
        &self,
        digest: &Digest,
    ) -> Result<Option<T>, Status> {
        Ok(self
            .read_input(digest)
            .await?
            .and_then(|bytes| T::decode(bytes).ok()))
    }

    /// The phases of a walk over a store in the metrics (a walk by the in-process cache
    /// has none to report: its figures are zero and it counts its own).
    fn observe_phases(&self, walked: &crate::input_tree::Walked) {
        let m = self.cache.metrics();
        m.execute_walk_dirs_read.add(walked.dirs_read);
        m.execute_walk_dirs_skipped.add(walked.dirs_skipped);
        m.execute_walk_file_asks.add(walked.file_asks);
        m.execute_walk_files_checked.add(walked.files_checked);
        for took in &walked.read_times {
            m.execute_walk_dir_read.observe_duration(*took);
        }
        m.execute_dir_cache_hits.add(walked.dir_cache_hits);
        m.execute_dir_cache_misses.add(walked.dir_cache_misses);
        m.execute_dir_cache_merged.add(walked.dir_cache_merged);
        m.execute_dir_cache_evictions.add(walked.dir_cache_evicted);
        m.execute_dir_cache_present_checked
            .add(walked.dirs_present_checked);
        if walked.phased {
            m.execute_dir_cache_bytes.set(walked.dir_cache_bytes);
        }
        if walked.phased {
            m.execute_walk_dirs.observe_duration(walked.dirs_time);
            m.execute_walk_files.observe_duration(walked.files_time);
        }
    }

    /// A blob's bytes, or `None` if it is not held. Anything else (unreachable, store
    /// down) fails the call: the input exists, so it is not MISSING.
    async fn read_input(&self, digest: &Digest) -> Result<Option<Bytes>, Status> {
        match self.cache.read_blob(digest).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(StorageError::NotFound(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// Where each axis of a booking came from: a size the platform named, or the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Sources {
    cpu: BookSource,
    memory: BookSource,
}

/// What an action's platform asks of the scheduler ([`ExecutionService::place`]).
struct Placed {
    kind: LeaseKind,
    /// The resources to book, GPUs included.
    booked: Resources,
    /// Which axes of `booked` the client named.
    sources: Sources,
    needs: kbf_caps::Request,
    /// Whether the default platform completed `needs`.
    defaulted: bool,
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

/// The platform's properties that kbf reads, each under the name kbf reads it by
/// (`kbf_caps::property_name`), or INVALID_ARGUMENT for a repeated one, also when
/// repeated in another spelling (`gpu` and `GPU`).
fn properties(platform: Option<&reapi::Platform>) -> Result<Platform, Status> {
    let properties = platform.map_or(&[][..], |p| p.properties.as_slice());
    let sent = Platform::from_properties(properties.iter().map(|p| (&*p.name, &*p.value)))
        .map_err(|e| Status::invalid_argument(e.to_string()))?;
    let mut read = Platform::new();
    for (name, value) in sent.canonical() {
        let Some(key) = kbf_caps::property_name(name) else {
            continue;
        };
        read.insert(&*key, value).map_err(|_| {
            Status::invalid_argument(format!(
                "platform property {key:?} is given twice, in two spellings (one is {name:?})"
            ))
        })?;
    }
    Ok(read)
}

/// The number of GPUs a platform asks for, or INVALID_ARGUMENT.
fn gpus(platform: &Platform) -> Result<u64, Status> {
    let Some(n) = platform.get(GPU_KEY) else {
        return Ok(0);
    };
    n.parse().map_err(|_| {
        Status::invalid_argument(format!(
            "platform property {GPU_KEY}={n:?} is not a whole number of GPUs"
        ))
    })
}

/// The GPUs to book for a request that asks for `gpus`. A Mac has one GPU, a slot it
/// reports as `gpu` 1 (`docs/design/macos-vms.md` section 8): an `action` lease on
/// macOS that asks for it books it, so one GPU lease runs on a Mac at a time while the
/// Mac's other work shares the rest; a `whole_machine` lease on macOS books none on top,
/// because it has the whole Mac, its GPU included; a `vm` lease is refused. Any other
/// request books what it asks.
/// A request that names no OS (an `arch` and a `gpu`, say) is not a request for a Mac:
/// it books what it asks, and a Mac, which reports a GPU, can serve it.
///
/// # Errors
/// INVALID_ARGUMENT for more than one GPU on macOS, of any lease kind: a Mac has one, so no
/// node could serve it; and for any GPU on a macOS `vm` lease, which has no host GPU.
fn mac_gpus(kind: LeaseKind, gpus: u64, needs: &kbf_caps::Request) -> Result<u64, Status> {
    if gpus == 0 || needs.exact("os") != Some("macos") {
        return Ok(gpus);
    }
    if gpus > 1 {
        return Err(Status::invalid_argument(format!(
            "platform property {GPU_KEY}={gpus} on macOS asks for more than a Mac has: a Mac \
             has one GPU, booked by one lease at a time"
        )));
    }
    match kind {
        LeaseKind::Action => Ok(gpus),
        LeaseKind::WholeMachine => Ok(0),
        // A guest's GPU is not the host's: the host's slot is not the guest's to book.
        LeaseKind::Vm => Err(Status::invalid_argument(format!(
            "platform property {GPU_KEY}={gpus} on a macOS {:?} lease: the Mac's GPU is not \
             passed to a guest",
            kind.name()
        ))),
    }
}

/// What the lease books: one core and `default_memory` bytes for an `action` (the least
/// worker, [`LEAST_WORKER`], for another kind), with `kbf-book-cpus` whole cores and
/// `kbf-book-mem-gib` GiB in place of the default where the platform names them, or
/// INVALID_ARGUMENT for a value that is not a whole number of at least 1 in plain
/// digits ([`starts_plain`]), one too large to book, or either key on a `whole_machine`
/// lease. The sources say which axes the platform named, so a named size equal to the
/// default stays a hint.
fn booking(
    platform: &Platform,
    kind: LeaseKind,
    default_memory: u64,
) -> Result<(Resources, Sources), Status> {
    let mut booked = if kind == LeaseKind::Action {
        Resources::new(DEFAULT_RESOURCES.cpu_millis, default_memory)
    } else {
        LEAST_WORKER
    };
    let mut sources = Sources::default();
    for (key, unit, slot, source) in [
        (
            BOOK_CPUS_KEY,
            1_000,
            &mut booked.cpu_millis,
            &mut sources.cpu,
        ),
        (
            BOOK_MEM_GIB_KEY,
            1 << 30,
            &mut booked.memory_bytes,
            &mut sources.memory,
        ),
    ] {
        let Some(value) = platform.get(key) else {
            continue;
        };
        if kind != LeaseKind::Action {
            return Err(Status::invalid_argument(format!(
                "platform property {key} sizes an {:?} lease only, not a {:?} one",
                LeaseKind::Action.name(),
                kind.name()
            )));
        }
        *slot = Some(value)
            .filter(|v| starts_plain(v))
            .and_then(|v| v.parse::<u64>().ok())
            .and_then(|n| n.checked_mul(unit))
            .ok_or_else(|| {
                Status::invalid_argument(format!(
                    "platform property {key}={value:?} is not a whole number from 1 to {} \
                     in plain digits",
                    u64::MAX / unit
                ))
            })?;
        *source = BookSource::Hint;
    }
    Ok((booked, sources))
}

/// Whether `value` starts with a digit from 1 to 9. A size must also parse as a `u64`,
/// which takes nothing but ASCII digits after an optional `+`; together that leaves
/// one spelling of each size, with no sign and no leading zero. Parsing alone also
/// takes `+4` and `04`, which book the same as `4` under a different action digest,
/// and so a different cache entry.
fn starts_plain(value: &str) -> bool {
    value
        .bytes()
        .next()
        .is_some_and(|b| (b'1'..=b'9').contains(&b))
}

/// What a worker must offer to run an action with `platform`: INVALID_ARGUMENT for a
/// malformed platform, FAILED_PRECONDITION for one no kbf daemon can ever run.
fn needs(platform: &Platform) -> Result<kbf_caps::Request, Status> {
    kbf_caps::Request::from_platform(platform.canonical()).map_err(|e| match e {
        FromPlatformError::Invalid(e) => {
            Status::invalid_argument(format!("platform properties: {e}"))
        }
        FromPlatformError::NeverServed(why) => Status::failed_precondition(why),
    })
}

/// INVALID_ARGUMENT for an iOS device (`ios.device=1`) on a lease kind other than
/// `action`: a device lease runs beside other work on its Mac
/// (`docs/design/ios-devices.md`, section 5.3), never as the whole machine.
fn device_kind(kind: LeaseKind, needs: &kbf_caps::Request) -> Result<(), Status> {
    if needs.wants_device() && kind != LeaseKind::Action {
        return Err(Status::invalid_argument(format!(
            "platform property ios.device books a device for an {:?} lease only, not a \
             {:?} one",
            LeaseKind::Action.name(),
            kind.name()
        )));
    }
    Ok(())
}

/// How long `action` may run, from its `timeout`: `None` when it has none, when it is
/// zero, and when it is negative (the daemon refuses such an action when it runs it).
fn wall_of(action: &Action) -> Option<Duration> {
    let timeout = action.timeout.as_ref()?;
    let wall = Duration::new(
        u64::try_from(timeout.seconds).ok()?,
        u32::try_from(timeout.nanos).ok()?,
    );
    (!wall.is_zero()).then_some(wall)
}

/// The lease kind a platform names, or INVALID_ARGUMENT for one not in
/// [`LEASE_KINDS`] (`vm` among them, until the front books it).
fn lease_kind(platform: &Platform) -> Result<LeaseKind, Status> {
    let Some(kind) = platform.get(LEASE_KIND_KEY) else {
        return Ok(LeaseKind::Action);
    };
    LeaseKind::from_name(kind)
        .filter(|k| LEASE_KINDS.contains(&k.name()))
        .ok_or_else(|| {
            Status::invalid_argument(format!(
                "platform property {LEASE_KIND_KEY}={kind:?} is not one of {LEASE_KINDS:?}"
            ))
        })
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
    let mut partial = None;
    let (value, result) = match stage {
        Stage::Queued => (execution_stage::Value::Queued, None),
        Stage::Waiting(why) | Stage::WaitingKept(why) => {
            let info = ErrorInfo {
                reason: NO_WORKER_REASON.to_owned(),
                domain: ERROR_DOMAIN.to_owned(),
                metadata: [("why".to_owned(), why.clone())].into(),
            };
            partial = Some(ExecutedActionMetadata {
                auxiliary_metadata: vec![any("google.rpc.ErrorInfo", info.encode_to_vec())],
                ..ExecutedActionMetadata::default()
            });
            (execution_stage::Value::Queued, None)
        }
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
        partial_execution_metadata: partial,
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

/// Completes once the server is draining and the operation is [`Stage::Waiting`]: it has
/// never been started and can never start, so it could only fail at the drain deadline,
/// and ending it when the drain begins frees the stop. The call fails (Buck2 does not recover
/// an ended stream), as it would at the deadline. Never for a plain `Queued` operation
/// (a worker may take it), nor for one that ever had a lease ([`Stage::WaitingKept`]).
async fn unstartable_while_draining(mut closing: Closing, mut stage: watch::Receiver<Stage>) {
    closing.drained().await;
    let queued = stage
        .wait_for(|stage| matches!(stage, Stage::Waiting(_)))
        .await
        .is_ok();
    if !queued {
        // The operation was abandoned: the branch that reads the stage reports that.
        std::future::pending::<()>().await;
    }
}

/// The operation as its stage changes: the stage now, then each change, ending after
/// the done operation. A stage that stops changing before it is done (the operation was
/// abandoned), or `closing` closing first (the server is going away), ends the stream
/// UNAVAILABLE; so does a drain while the operation can never start
/// ([`unstartable_while_draining`]). A change already there is sent before a close is
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
                let (watching, peeking) = (closing.clone(), stage.clone());
                let gone = tokio::select! {
                    biased;
                    changed = stage.changed() => changed
                        .is_err()
                        .then(|| format!("operation {name} was abandoned")),
                    () = closing.closed() => Some(format!(
                        "the server is shutting down; operation {name} ends here, Execute it again"
                    )),
                    () = unstartable_while_draining(watching, peeking) => Some(format!(
                        "the server is draining and operation {name} was never started and no \
                         worker can run it; it ends here (the operation is not cancelled)"
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
            open.update(
                matches!(
                    now,
                    Stage::Queued | Stage::Waiting(_) | Stage::WaitingKept(_)
                ),
                done,
            );
            let next = (!done).then_some((stage, closing, progress, open, false));
            Some((Ok(operation(&name, &action, &now, false)), next))
        }
    }))
}

#[cfg(test)]
#[path = "execution_gpu_tests.rs"]
mod gpu_tests;

#[cfg(test)]
#[path = "execution_progress_tests.rs"]
mod progress_tests;
