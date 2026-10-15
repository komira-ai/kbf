//! The farm core of one server: the scheduler, the operations callers wait on, the
//! worker streams, and the action-cache write for accepted results.
//!
//! Everything that changes scheduler state happens under one lock, in the order inputs
//! arrive; the scheduler's effects are carried out under the same lock (`Commit`,
//! `Start`, the offer that precedes a `Start`) or right after it (`Answer`, which reads
//! and writes the cache).
//!
//! **Single node.** The control log is this process: a record the scheduler asks to
//! commit is committed as soon as it is appended, and fed straight back (see
//! [`State::feed`]). The replicated log replaces exactly that step. Since the leases
//! die with the process, each process grants them under its own term, so a lease id
//! names one lease ever and an earlier process's `Result` is refused (issue #137).
//! Operations die with it too, so the term is part of every operation name (issue
//! #154). The term, and the lease epoch `Welcome` names, come from the metadata log
//! ([`MetaLog::leadership`]):
//!
//! - Under `--meta-log=memory` the log is this process's, so the term is a new
//!   [`process_term`] and the epoch is the same number: every start names another
//!   epoch, and a daemon kills the runs of the leases it held before.
//! - Under `--meta-log=raft` the term is the Raft term the log elected this server in,
//!   greater at every start, and the epoch is the log id, the same at every start over
//!   one `--raft-dir`. A daemon that reconnects after a restart therefore keeps the
//!   runs of its old leases, which this process never granted: until the scheduler's
//!   state is in the log they are orphans, whose results are refused and which no
//!   heartbeat cancels (docs/design/worker-protocol.md#server-restarts-and-the-lease-epoch
//!   bounds the work booked beside them).
//!
//! **Placement in the log** (issue #166): each grant at debug level, and each lease
//! the scheduler gives up at info, with the operation, the lease, the node and why.
//! An accepted result carries the node that ran it and when it was queued
//! ([`crate::stamp`]).

use std::collections::hash_map::RandomState;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::hash::BuildHasher;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kbf_front::{Cache, Dispatch, Finished, MetaLog, Recorder, Stage, Submission, Ticket};
use kbf_meta::{ActionRecord, Role};
use kbf_objstore::ObjectStore;
use kbf_proto::google::rpc;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{
    self, LeaseOffer, NodeStatus, ResultAck, ServerMessage, Start, server_message,
};
use kbf_sched::fence::START_VALIDITY;
use kbf_sched::{Cordon, DaemonInstance, Event, Input, OpState, Requeue, Scheduler};
use kbf_types::{
    Answer, ControlRecord, Digest, Effect, Failure, FarmTime, LeaseGrant, LeaseId, OperationId,
    Outcome, Refusal, StartLease, StateMachine, WaiterId, Waiting, WorkerId,
};
use tokio::sync::{mpsc, watch};
use tonic::{Code, Status};

mod beat;
mod phase;
mod record;

pub use phase::Reported;

use crate::fault;
use crate::fleet::{
    NodeView, NodesView, PhaseView, PlacementView, SoftwareView, attention_changes, needs_attention,
};
use crate::machine::{FarmMachine, Sent, Waiter};
use crate::memory;
use crate::missing;
use crate::stamp::Stamp;

/// The scheduler term of a new single-node server process: the wall-clock time of its
/// start in milliseconds since the Unix epoch, times 2^16, plus 16 random bits.
///
/// Every lease id a process grants carries its term, and a single node's leases live
/// in its process alone, so a term two processes share would let a lease id name two
/// leases: an old run's `Result` could be taken for another operation's (issue #137).
/// While the wall clock does not step back across a restart, each term is greater
/// than every earlier process's, as the scheduler's lease order expects of a newer
/// leader's. If it does step back, the new term still differs from an earlier one
/// unless the restart lands on that one's very millisecond and the random bits match
/// (one chance in 65 536). Under `--meta-log=raft` the Raft term replaces it.
#[must_use]
pub fn process_term() -> u64 {
    let start = unix_ms();
    // A `RandomState` is keyed from the operating system's random source.
    let random = RandomState::new().hash_one(start) & 0xffff;
    (start << 16) | random
}

/// Where the server sends a worker's messages: the outbound half of its stream.
pub type Outbound = mpsc::UnboundedSender<Result<ServerMessage, Status>>;

/// Names one worker stream, so messages from a replaced stream can be told apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StreamId(u64);

/// The farm core. Cheap to share: every method takes `&self`.
#[derive(Debug)]
pub struct Farm<M, O> {
    cache: Arc<Cache<M, O>>,
    /// This process's scheduler term: every lease it grants carries it, and every
    /// operation name it gives out names it.
    term: u64,
    /// The lease epoch `Welcome` names: the term itself, or the Raft log's id.
    lease_epoch: u64,
    epoch: Instant,
    /// The wall-clock time of `epoch`, in milliseconds since the Unix epoch: how farm
    /// times are shown to operators.
    epoch_unix_ms: u64,
    state: Mutex<State>,
}

/// An operator action named a node that never registered.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no node {0} has registered")]
pub struct UnknownNode(pub String);

/// What an operator does to a node's placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeAction {
    /// Offer it no new lease; its leases run on.
    Cordon,
    /// Cordon it and wait this long for its leases to end; then the drain pauses.
    Drain(Duration),
    /// Return it to placement, ending any drain.
    Uncordon,
}

/// A worker's newest stream.
#[derive(Debug)]
struct Link {
    stream: StreamId,
    /// The daemon process that opened it.
    instance: DaemonInstance,
    outbound: Outbound,
    /// The seq of the newest heartbeat taken on the stream; 0 before the first. Each
    /// `Start` names it, and the daemon acts on the `Start` only within
    /// [`START_VALIDITY`] of having sent that heartbeat (or, for 0, its `Hello`).
    newest_beat: u64,
}

/// An OK result and its action-cache record, from the report to the answer.
#[derive(Debug)]
struct Pending {
    result: ActionResult,
    record: ActionRecord,
}

/// What a report carries beyond its scheduler outcome, from the report to the answer.
#[derive(Debug)]
enum Detail {
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
    fn reason(&self) -> Option<String> {
        match self {
            Self::Invalid(why) | Self::Infra(why) => Some(why.clone()),
            Self::Ran(_) | Self::Missing(_) | Self::TimedOut(_) => None,
        }
    }
}

#[derive(Debug)]
struct State {
    sched: Scheduler,
    /// The callers' records, the finished operations whose callers are kept, the
    /// leases whose `Start` was sent, and the node registry.
    machine: FarmMachine,
    /// The channel of each caller the machine keeps, forgotten with its record.
    stages: BTreeMap<WaiterId, watch::Sender<Stage>>,
    next_stream: u64,
    /// The newest stream of each node the machine has registered.
    links: BTreeMap<WorkerId, Link>,
    /// Nodes whose drain has paused, once logged.
    paused: BTreeSet<WorkerId>,
    /// Where the invocation record's lease and finish events go ([`record`]).
    recorder: Recorder,
    /// The operations a recorded call waits on: the only ones [`record`] tells the
    /// recorder of. Each leaves at its end.
    recorded: BTreeSet<OperationId>,
}

/// What a finished operation tells its callers, and the action-cache entry to write
/// first, if any.
struct Settled {
    finished: Finished,
    write: Option<(Digest, ActionRecord)>,
    stages: Vec<watch::Sender<Stage>>,
}

impl<M: MetaLog, O: ObjectStore> Farm<M, O> {
    /// A farm over `cache`, with an empty scheduler of a new term that refuses queued work once no live worker has been able to run it for
    /// `unservable_wait`, and keeps a finished operation, which WaitExecution still
    /// answers, for `finished_retention`.
    ///
    /// The term and the lease epoch are those of the cache's metadata log
    /// ([`MetaLog::leadership`]: its leader's term, and its id) if it has them, and a
    /// new [`process_term`] for both otherwise (see the module docs).
    #[must_use]
    pub fn new(
        cache: Arc<Cache<M, O>>,
        unservable_wait: Duration,
        finished_retention: Duration,
    ) -> Self {
        let (term, lease_epoch) = cache.meta().leadership().map_or_else(
            || {
                let term = process_term();
                (term, term)
            },
            |log| (log.term, log.log_id.get()),
        );
        let sched = Scheduler::new(term)
            .with_unservable_wait(unservable_wait)
            .with_finished_retention(finished_retention)
            .recording_requeues();
        Self {
            cache,
            term,
            lease_epoch,
            epoch: Instant::now(),
            epoch_unix_ms: unix_ms(),
            state: Mutex::new(State {
                sched,
                machine: FarmMachine::new(term),
                stages: BTreeMap::new(),
                next_stream: 0,
                links: BTreeMap::new(),
                paused: BTreeSet::new(),
                recorder: Recorder::off(),
                recorded: BTreeSet::new(),
            }),
        }
    }

    /// This farm, each Execute's queued or joined answer, each lease granted, started
    /// and given up, and each operation's end sent to `recorder`
    /// (`docs/design/invocations.md`, section 3.4).
    #[must_use]
    pub fn recording(mut self, recorder: Recorder) -> Self {
        let state = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        state.recorder = recorder;
        self
    }

    /// This process's scheduler term, which every lease it grants carries.
    #[must_use]
    pub const fn term(&self) -> u64 {
        self.term
    }

    /// The lease epoch `Welcome` names (see the module docs).
    #[must_use]
    pub const fn lease_epoch(&self) -> u64 {
        self.lease_epoch
    }

    /// Farm time: milliseconds since this farm was built.
    fn now(&self) -> FarmTime {
        let millis = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        FarmTime::from_millis(millis)
    }

    /// Sets the scheduler gauges of `/metrics` from `state`: called where the lock is
    /// already held after a submit and on each tick, so a scrape sees the queue and the
    /// leases at most a tick old.
    fn publish(&self, state: &State) {
        let metrics = self.cache.metrics();
        metrics.sched_queued.set(state.sched.queued_len() as u64);
        metrics.sched_leases.set(state.sched.leases_held() as u64);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Nothing under the lock panics midway through a change.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The first `Hello` of a stream, from the daemon process `instance`: queues
    /// `welcome` on `outbound`, makes this the worker's stream for every `Start` from now
    /// on, and registers the worker (a new session) with what it `reported`. Its phase
    /// is taken before any placement, and may send a `Start` again or a `Cancel` on this
    /// stream ([`Self::resize`] says when). Messages still arriving on its earlier
    /// stream are ignored from now on. A stream of another process than the earlier
    /// stream's is logged: leases that process ran are kept until it has fenced (issue
    /// #140), unless it is proven stopped. It is when the new process names it as its
    /// `predecessor` (whose runs the new daemon's drivers killed before its `Hello`) and
    /// the earlier stream has closed: a process still connected may be another
    /// machine's, with a copy of this one's disk (issue #22).
    pub fn register(
        &self,
        worker: &WorkerId,
        instance: DaemonInstance,
        predecessor: DaemonInstance,
        reported: Reported,
        outbound: Outbound,
        welcome: ServerMessage,
    ) -> StreamId {
        let now = self.now();
        let mut state = self.lock();
        let stream = StreamId(state.next_stream);
        state.next_stream += 1;
        // The receiver is the stream's own response, alive until the stream ends.
        let _ = outbound.send(Ok(welcome));
        let link = Link {
            stream,
            instance: instance.clone(),
            outbound,
            newest_beat: 0,
        };
        let replaced = state.links.insert(worker.clone(), link);
        state.machine.register(worker);
        let stopped = replaced
            .as_ref()
            .is_some_and(|old| old.instance.same_as(&predecessor) && old.outbound.is_closed());
        if stopped {
            // Its runs were killed: its leases go at once, as farm faults.
            let predecessor = predecessor.as_str();
            tracing::info!(%worker, predecessor, "the daemon restarted and names its predecessor stopped");
        } else if replaced.is_some_and(|old| !old.instance.same_as(&instance)) {
            // The earlier process's leases are kept until it has fenced.
            let cause = "the daemon restarted, or two daemons hold this node's certificate";
            tracing::warn!(%worker, cause, "another daemon process registered as this node");
        }
        let event = Event::WorkerUp {
            worker: worker.clone(),
            instance,
            predecessor_stopped: stopped,
            capacity: reported.capacity,
            caps: reported.caps,
        };
        // No placement between the registration and the phase: a quarantined node
        // must not be offered a lease in between.
        let none = state.sched.apply(Input::new(now, event));
        debug_assert!(none.is_empty(), "a registration has no effect: {none:?}");
        state.phase(now, worker, reported.phase);
        stream
    }

    /// A `Hello` resent on `stream`: the node report or the node's phase changed.
    /// Changes the worker's capacity, capabilities and phase, and opens no session;
    /// ignored if `stream` was replaced.
    ///
    /// A `Preparing` or `Restoring` phase naming a lease the scheduler holds on the
    /// worker, whose `Start` went to an earlier session, sends that `Start` again (the
    /// daemon restarted for the session switch); one naming a lease of this process it
    /// does not hold there is sent a `Cancel`, so the daemon cleans up after it. A
    /// change into or out of quarantine is logged as an attention item.
    pub fn resize(&self, worker: &WorkerId, stream: StreamId, reported: Reported) {
        let now = self.now();
        let mut state = self.lock();
        if state.is_current(worker, stream) {
            let event = Event::Capacity {
                worker: worker.clone(),
                capacity: reported.capacity,
                caps: reported.caps,
            };
            let none = state.sched.apply(Input::new(now, event));
            debug_assert!(none.is_empty(), "a capacity change has no effect: {none:?}");
            state.phase(now, worker, reported.phase);
        }
    }

    /// A `NodeStatus` on `stream`: kept as the worker's newest, unless `stream` was
    /// replaced (a newer stream sends its own after its `Welcome`). Each attention item
    /// it raises or clears against the node's previous status, from any stream, is
    /// logged once; an Xcode it lists not surveyed yet keeps its previous item
    /// (`crate::fleet`). Returns those lines, `true` for each raised.
    pub fn node_status(
        &self,
        worker: &WorkerId,
        stream: StreamId,
        status: NodeStatus,
    ) -> Vec<(bool, String)> {
        let received = unix_ms();
        let mut state = self.lock();
        if !state.is_current(worker, stream) {
            return Vec::new();
        }
        let mut view = SoftwareView::new(status, received);
        let node = state.machine.node(worker);
        let before = node
            .and_then(|n| n.status.as_ref())
            .map_or(&[][..], |s| &s.xcodes);
        // An Xcode not surveyed yet (a restarted daemon's first status) keeps the item
        // it had: neither cleared now nor raised again by the survey (`crate::fleet`).
        view.hold_unsurveyed(before);
        let changes = attention_changes(worker.as_str(), before, &view.xcodes);
        for (raise, line) in &changes {
            if *raise {
                tracing::warn!(target: "kbf_server::attention", "{line}");
            } else {
                tracing::info!(target: "kbf_server::attention", "{line}");
            }
        }
        state.machine.report_status(worker, view);
        changes
    }

    /// This build, and every node registered since this farm started, in node-id order.
    pub fn nodes(&self) -> NodesView {
        let mut state = self.lock();
        state.note_closed_streams();
        let nodes = state
            .machine
            .nodes()
            .map(|worker| self.node(&state, worker))
            .collect();
        NodesView::of_this_build(nodes)
    }

    /// `worker` as `GET /v1/nodes` lists it, if it has registered.
    pub fn node_view(&self, worker: &WorkerId) -> Option<NodeView> {
        let mut state = self.lock();
        state.note_closed_streams();
        state
            .machine
            .node(worker)
            .is_some()
            .then(|| self.node(&state, worker))
    }

    /// Cordons, drains or uncordons `worker` (see `kbf_sched::Cordon`), and returns
    /// the node as it is now. A drain's deadline counts from now.
    ///
    /// # Errors
    /// `worker` has never registered.
    pub fn place(&self, worker: &WorkerId, action: NodeAction) -> Result<NodeView, UnknownNode> {
        let now = self.now();
        let mut state = self.lock();
        state.note_closed_streams();
        if state.machine.node(worker).is_none() {
            return Err(UnknownNode(worker.as_str().to_owned()));
        }
        let name = worker.clone();
        let event = match action {
            NodeAction::Cordon => Event::Cordon { worker: name },
            NodeAction::Drain(within) => Event::Drain {
                worker: name,
                deadline: now.saturating_add(within),
            },
            NodeAction::Uncordon => Event::Uncordon { worker: name },
        };
        tracing::info!(%worker, ?action, "operator action");
        state.feed_quiet(now, event);
        Ok(self.node(&state, worker))
    }

    /// One node's view; `worker` is registered, and every closed stream noted
    /// ([`State::note_closed_streams`]).
    fn node(&self, state: &State, worker: &WorkerId) -> NodeView {
        let wall = |at: FarmTime| self.epoch_unix_ms.saturating_add(at.as_millis());
        let leases = || {
            let held = state.sched.leases_on(worker);
            held.iter().map(ToString::to_string).collect()
        };
        let placement = match state.sched.cordon(worker) {
            None => PlacementView::Serving,
            Some(Cordon::Cordoned) => PlacementView::Cordoned,
            Some(Cordon::Draining { deadline }) => PlacementView::Draining {
                deadline_unix_ms: wall(*deadline),
                leases: leases(),
            },
            Some(Cordon::Drained) => PlacementView::Drained,
            Some(Cordon::Paused { deadline }) => PlacementView::DrainPaused {
                deadline_unix_ms: wall(*deadline),
                leases: leases(),
            },
        };
        let node = state.machine.node(worker);
        let software = node.and_then(|n| n.status.clone());
        let phase = state.sched.phase(worker).cloned().unwrap_or_default();
        NodeView {
            node_id: worker.as_str().to_owned(),
            connected: node.is_some_and(|n| n.connected),
            needs_attention: needs_attention(software.as_ref(), &phase),
            software,
            placement,
            phase: PhaseView::new(&phase),
        }
    }

    /// Expires leases of silent workers and places queued work. Logs each drain that
    /// has paused at its deadline, once.
    pub fn tick(&self) {
        let now = self.now();
        let mut state = self.lock();
        state.feed_quiet(now, Event::Tick);
        self.publish(&state);
        let paused: BTreeSet<WorkerId> = state
            .machine
            .nodes()
            .filter(|w| matches!(state.sched.cordon(w), Some(Cordon::Paused { .. })))
            .cloned()
            .collect();
        for worker in paused.difference(&state.paused) {
            let leases = state.sched.leases_on(worker).len();
            tracing::warn!(%worker, leases, "drain paused at its deadline; leases run on");
        }
        state.paused = paused;
    }

    /// A `Result` from `worker`. Accepted only if `worker` holds the operation's
    /// current lease, which this process granted, and the `Result` names no action but
    /// the one that lease runs; an accepted OK result is written to the action cache
    /// (unless the action is `do_not_cache` or exited non-zero) before its callers are
    /// answered. Before it is checked, its `ActionResult` gets the node id and the
    /// server's times ([`Stamp::apply`]), so callers and the cache see them. Returns
    /// the acknowledgement, or `None` for a `Result` without a lease id.
    pub async fn report(&self, worker: &WorkerId, result: worker::Result) -> Option<ResultAck> {
        let wire_lease = result.lease_id?;
        let lease = LeaseId::new(wire_lease.term, wire_lease.seq);
        let refused = ResultAck {
            lease_id: Some(wire_lease),
            accepted: false,
        };
        let holding = self.lock().holder(lease, worker);
        let Some((operation, action, stamp)) = holding else {
            tracing::warn!(%worker, %lease, "result refused: not the holder of the current lease");
            return Some(refused);
        };
        // The lease id is this process's, so the run it reports was started for this
        // operation; a daemon that says it ran another action is refused all the same
        // (issue #137).
        if let Some(named) = &result.action_digest
            && *named != kbf_front::digest_to_proto(&action)
        {
            tracing::warn!(%worker, %lease, %action, "result refused: it names another action");
            return Some(refused);
        }
        let ran_on = (worker, stamp);
        let (outcome, mut detail) = self.outcome(lease, ran_on, result).await;

        let now = self.now();
        let (answers, accepted) = {
            let mut state = self.lock();
            let (answers, accepted) = state.take_report(now, worker, operation, lease, outcome);
            let settled: Vec<Settled> = answers
                .iter()
                .map(|a| state.settle(a, detail.take()))
                .collect();
            (settled, accepted)
        };
        for settled in answers {
            self.deliver(settled).await;
        }
        Some(ResultAck {
            lease_id: Some(wire_lease),
            accepted,
        })
    }

    /// The scheduler's outcome for a `Result`, with the cache record of an OK one, the
    /// reason for an INVALID_ARGUMENT one, the partial result of a DEADLINE_EXCEEDED one
    /// ([`crate::timeout`], stamped as an OK result is), or the blobs of a missing-input
    /// one ([`missing::inputs`]). An OK result whose outputs are not all stored is an
    /// infrastructure failure: accepting it would answer callers with files nobody can
    /// fetch. An OK result gets the node it `ran_on` and the server's times
    /// ([`Stamp::apply`]) before it is checked and its cache record made.
    async fn outcome(
        &self,
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
                stamp.apply(&mut result, node, SystemTime::now());
                match self.cache.prepare_action_result(&result).await {
                    Ok(record) => {
                        let outcome = Outcome::Completed {
                            action_result: record.result,
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
                    Some(reported) => crate::timeout::partial(&self.cache, reported).await,
                    None => None,
                };
                let detail = partial.map(|mut partial| {
                    let (node, stamp) = ran_on;
                    stamp.apply(&mut partial, node, SystemTime::now());
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

    /// Writes the action-cache entry, if any, then answers the callers.
    async fn deliver(&self, settled: Settled) {
        if let Some((action, record)) = settled.write
            && let Err(e) = self
                .cache
                .commit_action_record(Role::Daemon, action, record)
                .await
        {
            tracing::error!(%action, error = %e, "action-cache write of an accepted result failed");
        }
        for stage in settled.stages {
            stage.send_replace(Stage::Done(settled.finished.clone()));
        }
    }
}

impl<M: MetaLog, O: ObjectStore + 'static> Dispatch for Farm<M, O> {
    fn submit(&self, mut submission: Submission) -> Result<Ticket, Status> {
        let now = self.now();
        let waiting = Instant::now();
        let mut state = self.lock();
        self.cache.metrics().sched_lock_wait.since(waiting);
        let action = submission.request.key.action;
        let instance = submission.request.key.instance.clone();
        let waiter = state.machine.submit(Waiter {
            key: submission.request.key.clone(),
            kind: submission.request.kind,
            do_not_cache: submission.request.do_not_cache,
            queued: SystemTime::now(),
        });
        let name = state.machine.name(waiter);
        if let Some(call) = submission.call.take() {
            state.record_submit(call, &name, &submission.request);
        }
        let (stage, receiver) = watch::channel(Stage::Queued);
        state.stages.insert(waiter, stage);
        state.feed_quiet(
            now,
            Event::Submit {
                waiter,
                request: submission.request,
            },
        );
        // A caller that joined a twin already started sees it executing.
        let joined_started = state.machine.started_operations().any(|operation| {
            state
                .sched
                .waiters(operation)
                .is_some_and(|w| w.contains(&waiter))
        });
        if let Some(stage) = state.stages.get(&waiter).filter(|_| joined_started) {
            stage.send_replace(Stage::Executing);
        }
        self.publish(&state);
        Ok(Ticket {
            name,
            instance,
            action,
            stage: receiver,
        })
    }

    fn wait(&self, name: &str) -> Option<Ticket> {
        let state = self.lock();
        let (id, waiter) = state.machine.named(name)?;
        let stage = state.stages.get(&id)?;
        Some(Ticket {
            name: name.to_owned(),
            instance: waiter.key.instance.clone(),
            action: waiter.key.action,
            stage: stage.subscribe(),
        })
    }
}

impl State {
    /// How many runs of `operation` the scheduler has recorded as killed for memory or
    /// as the farm's fault.
    fn runs_recorded(&self, operation: OperationId) -> usize {
        let memory = self.sched.memory_runs(operation).map_or(0, <[_]>::len);
        let farm = self.sched.farm_runs(operation).map_or(0, <[_]>::len);
        memory + farm
    }

    /// Feeds `worker`'s report that `lease` of `operation` ended with `outcome`, and
    /// returns the answers it led to and whether the scheduler took it. A busy node's
    /// memory kill the scheduler took is logged for operators (`crate::memory`).
    ///
    /// The in-process log commits its record at once: an accepted result is answered
    /// here, and a refused one (its lease given up while its outputs were checked)
    /// never is. A memory kill or lost run the scheduler takes is answered, or records
    /// a run and runs the operation again. The replicated log will answer once the
    /// record commits, carrying the result with it.
    ///
    /// Not generic, unlike `Farm::report`, so every test binary's runs count toward the
    /// same function's coverage.
    fn take_report(
        &mut self,
        now: FarmTime,
        worker: &WorkerId,
        operation: OperationId,
        lease: LeaseId,
        outcome: Outcome,
    ) -> (Vec<Answer>, bool) {
        let recorded_before = self.runs_recorded(operation);
        let event = Event::Report {
            operation,
            lease,
            outcome,
        };
        let answers = self.feed(now, event);
        let rerun = self.runs_recorded(operation) > recorded_before;
        let accepted = rerun || answers.iter().any(|a| a.lease == lease);
        if accepted && outcome == Outcome::Failed(Failure::NodeMemoryPressure) {
            memory::pressure(worker, self.sched.memory_pressure(worker));
        }
        (answers, accepted)
    }

    fn is_current(&self, worker: &WorkerId, stream: StreamId) -> bool {
        self.links.get(worker).is_some_and(|l| l.stream == stream)
    }

    /// Tells the machine of each node whose newest stream has closed (its response
    /// side is gone), so its registry says the node is not connected.
    fn note_closed_streams(&mut self) {
        for (worker, link) in &self.links {
            if link.outbound.is_closed() {
                self.machine.disconnect(worker);
            }
        }
    }

    /// The operation whose current lease is `lease`, the action its `Start` named,
    /// and the server's times for the run, if `worker` holds it. Only this process's
    /// leases are in the machine's started table, so a lease of an earlier process
    /// (another term) is never one.
    fn holder(&self, lease: LeaseId, worker: &WorkerId) -> Option<(OperationId, Digest, Stamp)> {
        let sent = self.machine.sent(lease)?;
        let operation = sent.operation;
        let current = match self.sched.state(operation)? {
            OpState::Leased {
                lease: held,
                worker: holder,
                committed: true,
            }
            | OpState::Running {
                lease: held,
                worker: holder,
            } => *held == lease && holder == worker,
            _ => false,
        };
        current.then_some((operation, sent.action, sent.stamp))
    }

    /// Feeds an input that cannot lead to an answer: only a `Report` and a heartbeat
    /// (a lease lost to a proven restart) propose a result, and only a committed result
    /// answers.
    fn feed_quiet(&mut self, now: FarmTime, event: Event) {
        let answers = self.feed(now, event);
        debug_assert!(
            answers.is_empty(),
            "an answer without a report: {answers:?}"
        );
    }

    /// Feeds `event`, then a tick (so placement follows at once), and carries out the
    /// effects in order. Returns the answers, for the caller to deliver once the lock
    /// is released.
    fn feed(&mut self, now: FarmTime, event: Event) -> Vec<Answer> {
        let mut effects: VecDeque<Effect> = self.sched.apply(Input::new(now, event)).into();
        effects.extend(self.sched.apply(Input::new(now, Event::Tick)));
        self.log_requeues();
        let mut answers = Vec::new();
        while let Some(effect) = effects.pop_front() {
            match effect {
                Effect::Commit(record) => {
                    if let ControlRecord::Lease(grant) = &record {
                        self.offer(grant);
                    }
                    // Single node: appended is committed. The replicated log goes here.
                    let committed = Event::Committed(record);
                    effects.extend(self.sched.apply(Input::new(now, committed)));
                }
                Effect::Start(start) => self.start(start),
                Effect::Answer(answer) => answers.push(answer),
                Effect::Waiting(waiting) => self.waiting(&waiting),
                Effect::Refuse(refusal) => self.refuse(&refusal),
            }
        }
        self.log_requeues();
        self.forget_dropped();
        answers
    }

    /// Logs the leases the scheduler has given up since the last call, one INFO line
    /// each, naming the operation by its first caller's name. Called before any
    /// effect is carried out (an effect can finish an operation, and with a zero
    /// finished retention drop it and forget its callers) and again at the end of a
    /// feed. A requeue only names an operation the scheduler held when it was made;
    /// one dropped since is logged with an empty name, never looked up by index.
    fn log_requeues(&mut self) {
        for requeue in self.sched.take_requeues() {
            self.record_requeue(&requeue);
            let Requeue {
                operation,
                lease,
                worker: node,
                reason,
            } = requeue;
            let operation = self.operation_name(operation);
            // One line: the coverage of a logged field counts only where it is logged.
            tracing::info!(%operation, %lease, %node, %reason, "lease given up; requeued");
        }
    }

    /// Keeps the callers of `operation`, which has just finished, until the scheduler
    /// drops it.
    fn keep_finished(&mut self, operation: OperationId, waiters: Vec<WaiterId>) {
        self.machine.keep_finished(operation, waiters);
        self.forget_dropped();
    }

    /// Forgets the callers, and their channels, of every finished operation the
    /// scheduler has dropped ([`FarmMachine::forget_dropped`]): a WaitExecution on one
    /// is NOT_FOUND from now on. Every input ends here, so debug builds check here
    /// that the callers with a channel are exactly the callers the machine keeps: the
    /// same ids, not only as many.
    fn forget_dropped(&mut self) {
        let sched = &self.sched;
        let forgotten = self
            .machine
            .forget_dropped(|operation| sched.state(operation).is_some());
        for id in forgotten {
            self.stages.remove(&id);
        }
        debug_assert!(
            self.stages.keys().copied().eq(self.machine.waiter_ids()),
            "a caller's channel outlives its record, or a record has no channel"
        );
    }

    /// The REAPI name of `operation`, its first waiter's, as the log names it; empty
    /// for an operation without one.
    fn operation_name(&self, operation: OperationId) -> String {
        self.first_waiter(operation)
            .map_or_else(String::new, |(id, _)| self.machine.name(id))
    }

    /// The first waiter of `operation`: its key and lease kind are the operation's.
    fn first_waiter(&self, operation: OperationId) -> Option<(WaiterId, &Waiter)> {
        let first = *self.sched.waiters(operation)?.first()?;
        self.machine.waiter(first).map(|waiter| (first, waiter))
    }

    /// Sends `worker` the message `build` makes from `operation`'s first waiter.
    ///
    /// Both are always there, so nothing here branches on them: the scheduler places
    /// work only on a worker that registered (and a worker's link outlives its stream),
    /// and only operations a waiter submitted (waiters leave only once it is finished).
    /// A stream that has just ended drops the message; its lease is reconciled later.
    fn send_for(
        &self,
        worker: &WorkerId,
        operation: OperationId,
        build: impl FnOnce(&Waiter) -> server_message::Message,
    ) {
        let message = self
            .first_waiter(operation)
            .map(|(_, waiter)| build(waiter));
        // Whether it was sent: a stream that has just ended drops it.
        let _sent = self.links.get(worker).zip(message).map(|(link, message)| {
            link.outbound
                .send(Ok(ServerMessage {
                    message: Some(message),
                }))
                .is_ok()
        });
    }

    /// Tells the worker a lease is placed on it, before the grant commits.
    fn offer(&self, grant: &LeaseGrant) {
        let (operation, lease, node) = (
            self.operation_name(grant.operation),
            grant.lease,
            &grant.worker,
        );
        tracing::debug!(%operation, %lease, %node, "lease granted");
        self.record_granted(grant);
        self.send_for(&grant.worker, grant.operation, |w| {
            server_message::Message::LeaseOffer(LeaseOffer {
                lease_id: Some(wire_lease(grant.lease)),
                kind: w.kind.name().to_owned(),
                action_digest: Some(kbf_front::digest_to_proto(&w.key.action)),
            })
        });
    }

    /// Sends the `Start` of a committed lease and marks its callers executing. The
    /// `Start` names the newest heartbeat taken on the worker's stream and the window
    /// after it in which the daemon may still act on the `Start` (issue #23).
    fn start(&mut self, start: StartLease) {
        let heartbeat_seq = self.links.get(&start.worker).map_or(0, |l| l.newest_beat);
        self.send_for(&start.worker, start.operation, |_| {
            server_message::Message::Start(Start {
                lease_id: Some(wire_lease(start.lease)),
                kind: start.kind.name().to_owned(),
                action_digest: Some(kbf_front::digest_to_proto(&start.key.action)),
                millicpus: start.resources.cpu_millis,
                memory_bytes: start.resources.memory_bytes,
                heartbeat_seq,
                valid_for_ms: START_VALIDITY_MS,
                device_id: start.device.clone().unwrap_or_default(),
            })
        });
        let waiters = self.sched.waiters(start.operation).unwrap_or_default();
        for stage in waiters.iter().filter_map(|id| self.stages.get(id)) {
            stage.send_replace(Stage::Executing);
        }
        let now = SystemTime::now();
        // An operation with a `Start` has its first waiter (see `send_for`).
        let queued = self
            .first_waiter(start.operation)
            .map_or(now, |(_, w)| w.queued);
        let sent = Sent {
            operation: start.operation,
            action: start.key.action,
            stamp: Stamp {
                queued,
                started: now,
            },
        };
        self.machine.start(start.lease, sent);
        self.record_started(&start);
    }

    /// Tells an operation's callers why it waits, or that it no longer waits for a
    /// worker that can run it. Logged, so an operator sees it too.
    fn waiting(&self, waiting: &Waiting) {
        let operation = waiting.operation;
        let stage = match &waiting.reason {
            Some(why) => {
                tracing::warn!(%operation, reason = %why, "no live worker can run the operation");
                Stage::Waiting(why.clone())
            }
            None => {
                tracing::info!(%operation, "a live worker can run the operation again");
                Stage::Queued
            }
        };
        // A caller that joins a waiting operation is told again; the others, who
        // already know, see no repeated update.
        let waiters = self.sched.waiters(operation).unwrap_or_default();
        for channel in waiters.iter().filter_map(|id| self.stages.get(id)) {
            channel.send_if_modified(|now| {
                let changed = *now != stage;
                if changed {
                    now.clone_from(&stage);
                }
                changed
            });
        }
    }

    /// Answers the callers of an operation the scheduler refused FAILED_PRECONDITION
    /// with the reason, and keeps them for the finished retention. Nothing is cached,
    /// so nothing is awaited.
    fn refuse(&mut self, refusal: &Refusal) {
        tracing::warn!(operation = %refusal.operation, reason = %refusal.reason, "operation refused");
        self.record_refused(refusal);
        let finished = failed(Code::FailedPrecondition, &refusal.reason);
        // A lease given up before the operation was refused may still be listed.
        self.machine.forget_leases(refusal.operation);
        for stage in refusal.waiters.iter().filter_map(|id| self.stages.get(id)) {
            stage.send_replace(Stage::Done(finished.clone()));
        }
        self.keep_finished(refusal.operation, refusal.waiters.clone());
    }

    /// Works out what the callers of a finished operation get, and keeps them for the
    /// finished retention. `detail` is what the accepted report carried beyond its
    /// outcome, if anything.
    fn settle(&mut self, answer: &Answer, detail: Option<Detail>) -> Settled {
        self.machine.forget_leases(answer.operation);
        let waiters: Vec<&Waiter> = answer
            .waiters
            .iter()
            .filter_map(|id| self.machine.waiter(*id))
            .collect();
        let stages = answer
            .waiters
            .iter()
            .filter_map(|id| self.stages.get(id).cloned())
            .collect();
        let reason = detail.as_ref().and_then(Detail::reason);
        let mut stored = None;
        let (finished, write) = match (detail, answer.outcome) {
            (Some(Detail::Ran(pending)), _) => {
                let Pending { result, record } = *pending;
                stored = Some(record.result);
                let cacheable =
                    result.exit_code == 0 && waiters.first().is_some_and(|w| !w.do_not_cache);
                let write = waiters
                    .first()
                    .filter(|_| cacheable)
                    .map(|w| (w.key.action, record));
                (Finished::Ran(Box::new(result)), write)
            }
            (Some(Detail::TimedOut(partial)), Outcome::Failed(Failure::Timeout)) => {
                let status = status(Code::DeadlineExceeded, TIMED_OUT);
                (Finished::TimedOut { status, partial }, None)
            }
            (_, Outcome::Failed(Failure::Timeout)) => {
                (failed(Code::DeadlineExceeded, TIMED_OUT), None)
            }
            (Some(Detail::Invalid(why)), _) => (failed(Code::InvalidArgument, &why), None),
            (Some(Detail::Missing(blobs)), _) => {
                (Finished::Failed(kbf_front::missing_status(&blobs)), None)
            }
            (_, Outcome::Failed(Failure::OutOfMemory)) => {
                (memory::finished(&answer.memory_runs), None)
            }
            (_, Outcome::Failed(Failure::NodeMemoryPressure | Failure::Lost(_))) => {
                (fault::finished(&answer.farm_runs), None)
            }
            _ => (
                failed(Code::Internal, "the farm could not run the action"),
                None,
            ),
        };
        self.record_finished(answer, &finished, stored, reason);
        self.keep_finished(answer.operation, answer.waiters.clone());
        Settled {
            finished,
            write,
            stages,
        }
    }
}

/// Now on the wall clock, in milliseconds since the Unix epoch.
fn unix_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

/// What the callers of a timed-out action are told.
const TIMED_OUT: &str = "the action timed out";

fn failed(code: Code, message: &str) -> Finished {
    Finished::Failed(status(code, message))
}

fn status(code: Code, message: &str) -> rpc::Status {
    rpc::Status {
        code: code as i32,
        message: message.to_owned(),
        details: Vec::new(),
    }
}

/// [`START_VALIDITY`] in milliseconds, as `Start.valid_for_ms` carries it.
const START_VALIDITY_MS: u64 = START_VALIDITY.as_millis() as u64;

fn wire_lease(lease: LeaseId) -> worker::LeaseId {
    worker::LeaseId {
        term: lease.term,
        seq: lease.seq,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a server process that reuses an earlier process's term (issue #137:
    /// every process granted leases from `(1, 0)`), and one whose term does not order
    /// after that of a process started a few milliseconds before. A term is also never
    /// 1, the term every server before the fix used.
    #[test]
    fn each_process_term_is_new_and_later() {
        let first = process_term();
        std::thread::sleep(Duration::from_millis(3));
        let second = process_term();
        assert!(second > first, "{second} does not order after {first}");
        assert!(first >> 16 > 0, "{first} could be a pre-fix term");
    }
}
