//! The farm core of one server: the scheduler, the operations callers wait on, the
//! worker streams, and the action-cache write for accepted results.
//!
//! Everything that changes scheduler state happens under one lock, in the order inputs
//! arrive; the scheduler's effects are carried out under the same lock (`Commit`,
//! `Start`, the offer that precedes a `Start`) or right after it (`Answer`, which reads
//! and writes the cache).
//!
//! The state under the lock is two parts. The [`engine`] holds the scheduler, the state
//! store handle and the bookkeeping between them, and runs the feed loop; it knows nothing
//! of REAPI, caller channels or the action cache. [`side`] holds those, and is the
//! engine's [`engine::Adapter`]: the engine calls it for every effect it carries out.
//!
//! **Single node.** The control log is this process: a record the scheduler asks to
//! commit is committed as soon as it is appended, and fed straight back (see
//! [`engine::Engine::carry_out`]). The replicated log replaces exactly that step. Each
//! process grants leases under its own term (and a store continues the sequence), so a
//! lease id names one lease ever (issue #137). Under a state store the predecessor's own
//! leases are restored (`take_stored`) when their daemon says `Hello`: its `Result` ends
//! its own operation until the daemon's heartbeat omits the lease, and one naming another
//! action is a MISMATCH; an id no row holds is UNKNOWN_LEASE.
//! Operations die with it too, so the term is part of every operation name (issue
//! #154). A farm over a state store names its operations by their ids instead,
//! `operations/<id>`: the ids are the store's, so a name means one operation on every
//! frontend of the store and across restarts, and any frontend answers it (`named`
//! module). The term, and the lease epoch `Welcome` names, come from the metadata log
//! ([`MetaLog::leadership`]):
//!
//! - Under the in-memory metadata log (tests only) the log is this process's, so the term is a new
//!   [`process_term`] and the epoch is the same number: every start names another
//!   epoch, and a daemon kills the runs of the leases it held before.
//! - Under the Raft metadata log the term is the Raft term the log elected this server in,
//!   greater at every start, and the epoch is the log id, the same at every start over
//!   one `--meta-dir`. A daemon that reconnects after a restart therefore keeps the
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
use std::collections::{BTreeMap, BTreeSet};
use std::hash::BuildHasher;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_front::{Backend, Dispatch, Recorder, Stage, Submission, Ticket};
use kbf_proto::update::UpdateStatus;
use kbf_proto::worker::{self, NodeStatus, ServerMessage};
use kbf_sched::fence::START_VALIDITY;
use kbf_sched::{Cordon, DaemonInstance, Event, LOSS_REQUEUES, MemoryLadder, OpState, Scheduler};
use kbf_types::{
    ActionKey, Answer, Digest, FarmTime, LeaseId, MemoryRun, OperationId, StateMachine, WorkerId,
};
use tokio::sync::{mpsc, watch};
use tonic::Status;

#[macro_use]
mod driver;
mod ack;
mod ambient;
mod beat;
mod cordon_owner;
pub(crate) mod durable;
mod engine;
pub(crate) mod failed_actions;
mod handover;
mod input_cache;
mod js;
mod learning;
mod named;
mod node_load;
mod node_types;
mod nodes;
mod pack_reads;
mod pack_writes;
mod phase;
mod report;
pub mod sched_view;
mod side;
#[cfg(any(test, feature = "testkit"))]
mod testkit;

#[cfg(any(test, feature = "testkit"))]
pub use testkit::TestCore;

pub use durable::{
    ACK_WINDOW, DONE_FLOOR, EpochBump, EpochPolicy, ID_BLOCK, INTAKE_POLL, ORPHAN_WAIT,
    OWNER_CACHE_TTL, SESSION_RENEWAL, StateError,
};
pub use handover::FOLLOW_EVERY;
pub use node_types::{NodeAction, NodeLook, NodeUnread, PlaceError, UnknownNode};
pub use phase::Reported;

use self::ambient::Ambient;
use self::driver::Driver;
pub(crate) use self::driver::Settings;
use self::durable::Held;
use self::engine::Engine;
pub(crate) use self::node_load::NodeLoads;
use self::report::{Detail, Settled, finished_of, refused_as};
pub(crate) use self::side::identity;
use self::side::{Link, Side};
use crate::action_usage::Shapes;
use crate::fleet::{NodeView, take_node_status};
use crate::learned::LearnedSizes;
use crate::machine::{Platform, Waiter, parse_stored_name};
use crate::size_key::Keyable;
use crate::stamp::Stamp;
use crate::update_status::UpdateReport;

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
/// (one chance in 65 536). Under the Raft metadata log the Raft term replaces it.
#[must_use]
pub fn process_term() -> u64 {
    let start = unix_ms();
    // A `RandomState` is keyed from the operating system's random source.
    let random = RandomState::new().hash_one(start) & 0xffff;
    (start << 16) | random
}

/// Where the server sends a worker's messages: the outbound half of its stream.
pub type Outbound = mpsc::UnboundedSender<Result<ServerMessage, Status>>;

/// A test's check of the callers' stages at the point a `Start` is about to be sent.
#[cfg(test)]
pub(crate) type StartHook = Box<dyn Fn(Vec<Stage>)>;

#[cfg(test)]
thread_local! {
    /// Called with the stage of each caller of the operation just before a `Start` is sent,
    /// by tests that check what is true at that point.
    pub(crate) static BEFORE_START: std::cell::RefCell<Option<StartHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Names one worker stream, so messages from a replaced stream can be told apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StreamId(u64);

/// The farm core. Cheap to share: every method takes `&self`.
#[derive(Debug)]
pub struct Farm {
    cache: Backend,
    /// This process's scheduler term: every lease it grants carries it, and every
    /// operation name it gives out names it.
    term: u64,
    /// The lease epoch `Welcome` names: the term itself, or the Raft log's id.
    lease_epoch: u64,
    /// When this farm was built, on tokio's clock (so a test that pauses it moves farm
    /// time with `advance`).
    epoch: tokio::time::Instant,
    /// The wall-clock time of `epoch`, in milliseconds since the Unix epoch: how farm
    /// times are shown to operators.
    epoch_unix_ms: u64,
    /// The action shapes this process has described ([`action_usage`](crate::action_usage)).
    shapes: Shapes,
    /// `Result`s being taken now ([`Farm::report`]): a handover waits for none to be.
    reports: AtomicUsize,
    /// Where the wall clock and the process term come from.
    ambient: Ambient,
    /// What the `with_*` builders set.
    settings: Settings,
    /// The `--usage-profiles` mode, which the start line and the gauge report.
    usage_profiles: crate::UsageProfiles,
    /// The engine every method reaches the farm's state through.
    driver: Driver,
}

/// The state under the farm's lock: the [`Engine`] that runs the feed loop, and the
/// farm's own [`Side`], which it calls through [`engine::Adapter`].
#[derive(Debug)]
struct State {
    engine: Engine,
    side: Side,
}

/// The engine's fields, readable and writable through the state in tests, which look at
/// the scheduler and the store bookkeeping directly.
#[cfg(test)]
impl std::ops::Deref for State {
    type Target = Engine;

    fn deref(&self) -> &Engine {
        &self.engine
    }
}

#[cfg(test)]
impl std::ops::DerefMut for State {
    fn deref_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }
}

/// What [`Farm::assemble`] builds a farm from.
struct Parts {
    cache: Backend,
    term: u64,
    lease_epoch: u64,
    sched: Scheduler,
    finished_retention: Duration,
    ambient: Ambient,
    epoch_unix_ms: u64,
}

impl Farm {
    /// A farm over `cache`, with an empty scheduler of a new term that refuses queued work once no live worker has been able to run it for
    /// `unservable_wait`, and keeps a finished operation, which WaitExecution still
    /// answers, for `finished_retention`.
    ///
    /// The term and the lease epoch are those of the cache's metadata log
    /// ([`MetaLog::leadership`]: its leader's term, and its id) if it has them, and a
    /// new [`process_term`] for both otherwise (see the module docs).
    ///
    /// It runs on the core [`crate::SchedulerCore::under_test`] names: the farm's own, and
    /// the js core in a build with the `farm-core-js` feature (the tests of the js core).
    #[must_use]
    pub fn new(
        cache: impl Into<Backend>,
        unservable_wait: Duration,
        finished_retention: Duration,
    ) -> Self {
        Self::on_core(
            crate::SchedulerCore::under_test(),
            cache.into(),
            unservable_wait,
            finished_retention,
        )
    }

    /// [`Farm::new`] on the legacy core whatever core [`crate::SchedulerCore::under_test`] names:
    /// for the tests of the legacy engine, which reach its state (`lock`, `state_mut`).
    #[cfg(test)]
    pub(crate) fn new_legacy(
        cache: impl Into<Backend>,
        unservable_wait: Duration,
        finished_retention: Duration,
    ) -> Self {
        Self::on_core(
            crate::SchedulerCore::Legacy,
            cache.into(),
            unservable_wait,
            finished_retention,
        )
    }

    /// [`Farm::new`] on `core`.
    pub(crate) fn on_core(
        core: crate::SchedulerCore,
        cache: Backend,
        unservable_wait: Duration,
        finished_retention: Duration,
    ) -> Self {
        if core == crate::SchedulerCore::Js {
            return Self::new_js(cache, unservable_wait, finished_retention);
        }
        let (term, lease_epoch) = Self::terms_of(&cache);
        let sched = Scheduler::new(term)
            .with_load_config(crate::PlacementLoad::Off.config())
            .with_unservable_wait(unservable_wait)
            .with_finished_retention(finished_retention)
            .with_loss_requeues(LOSS_REQUEUES)
            .recording_requeues();
        let epoch = tokio::time::Instant::now();
        Self::assemble(
            Parts {
                cache,
                term,
                lease_epoch,
                sched,
                finished_retention,
                ambient: Ambient::Process,
                epoch_unix_ms: unix_ms(),
            },
            epoch,
        )
    }

    /// The term and the lease epoch of a new farm over `cache`: its metadata log's, or a new
    /// process term for both.
    fn terms_of(cache: &Backend) -> (u64, u64) {
        cache.leadership().map_or_else(
            || {
                let term = process_term();
                (term, term)
            },
            |log| (log.term, log.log_id.get()),
        )
    }

    /// The farm over `parts`, built at `epoch` on tokio's clock: the one place a farm's
    /// state is made, for [`Farm::new`] and for the farm a test builds.
    fn assemble(parts: Parts, epoch: tokio::time::Instant) -> Self {
        let Parts {
            cache,
            term,
            lease_epoch,
            sched,
            finished_retention,
            ambient,
            epoch_unix_ms,
        } = parts;
        Self {
            cache,
            term,
            lease_epoch,
            epoch,
            epoch_unix_ms,
            shapes: Shapes::default(),
            reports: AtomicUsize::new(0),
            ambient,
            settings: Settings::default(),
            usage_profiles: crate::UsageProfiles::default(),
            driver: Driver::legacy(State {
                engine: Engine::new(sched, finished_retention, ambient),
                side: Side::new(term, ambient),
            }),
        }
    }

    /// This farm, built at wall-clock time `unix_ms` (milliseconds since the Unix epoch)
    /// instead of now: the day its size summaries age by. Set before
    /// [`Farm::with_learned_sizes`].
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_wall_epoch(mut self, unix_ms: u64) -> Self {
        self.epoch_unix_ms = unix_ms;
        self
    }

    /// This farm, with learned sizes `mode` (`--learned-sizes`) over a first rung of
    /// `default_memory` bytes. Set before the farm takes a state store over: `off` reads
    /// none of the size summaries it holds.
    #[must_use]
    pub fn with_learned_sizes(mut self, mode: LearnedSizes, default_memory: u64) -> Self {
        self.settings.learned = mode;
        self.settings.default_memory = default_memory;
        self.driver.set_learned(self.epoch_unix_ms, &self.settings);
        self
    }

    /// This farm, placing as `mode` says (`--placement-load`) instead of by the booked rule
    /// ([`crate::PlacementLoad::Off`]) that [`Farm::new`] builds it with. Set before the farm takes
    /// a state store over.
    #[must_use]
    pub fn with_placement_load(mut self, mode: crate::PlacementLoad) -> Self {
        self.settings.placement_load = mode;
        self.driver.set_placement_load(self.term, &self.settings);
        self
    }

    /// This farm, whose scheduler climbs `ladder` for an action that passes its memory limit
    /// (instead of [`kbf_sched::MemoryLadder::default`]).
    #[must_use]
    pub fn with_memory_ladder(mut self, ladder: MemoryLadder) -> Self {
        self.settings.ladder = ladder;
        self.driver.set_ladder(&self.settings);
        self
    }

    /// This farm, whose scheduler finishes an action that passes its own memory limit with
    /// `Failed(OutOfMemory)` at its booking if `climb` is `false` (`--no-memory-climb`), instead
    /// of running it again one rung up. Legacy core only: the js core builds its own scheduler
    /// and ignores this.
    #[must_use]
    pub fn with_memory_climb(mut self, climb: bool) -> Self {
        self.settings.climb = climb;
        self.driver.set_climb(&self.settings);
        self
    }

    /// Whether an own-limit kill runs the action again one rung up (the default) or finishes it.
    #[must_use]
    pub(crate) const fn memory_climb(&self) -> bool {
        self.settings.climb
    }

    /// This farm, keeping at most `bound` memory floors (instead of
    /// [`kbf_sched::MEMORY_FLOORS`]); past that, the one raised longest ago is forgotten
    /// first. Set before the farm takes a state store over.
    #[must_use]
    pub fn with_memory_floors(mut self, bound: usize) -> Self {
        self.settings.floors = bound;
        self.driver.set_floors(self.term, &self.settings);
        self
    }

    /// This farm, each Execute's queued or joined answer, each lease granted, started
    /// and given up, and each operation's end sent to `recorder`
    /// (`docs/design/invocations.md`, section 3.4).
    #[must_use]
    pub fn recording(mut self, recorder: Recorder) -> Self {
        self.settings.recorder = recorder;
        self.driver.set_recorder(&self.settings);
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
    pub(crate) fn now(&self) -> FarmTime {
        let millis = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        FarmTime::from_millis(millis)
    }

    /// Sets the scheduler gauges of `/metrics` from `state`: called where the lock is
    /// already held after a submit and on each tick, so a scrape sees the queue and the
    /// leases at most a tick old.
    fn publish(&self, state: &State) {
        let metrics = self.cache.metrics();
        metrics
            .sched_queued
            .set(state.engine.sched.queued_len() as u64);
        metrics
            .sched_leases
            .set(state.engine.sched.leases_held() as u64);
        metrics
            .sched_whole
            .set(state.engine.sched.whole_summary().as_ref());
        state.side.learned.publish(&metrics.estimator_shadow);
        state.side.learned.publish_predictions(&metrics.prediction);
        metrics
            .prediction
            .set_cpu_limited_nodes(self.cpu_limited_nodes(state));
    }

    /// Writes the changed size summaries to the state store now, as the tick does once
    /// the write-behind interval has passed: for a test that would otherwise wait out
    /// the interval.
    #[doc(hidden)]
    pub fn write_behind_now(&self) {
        js_arm!(self, lifecycle::write_behind_now());
        self.lock().write_behind(true);
    }

    /// Operations waiting in the scheduler's queue for a lease, now (read under the
    /// scheduler's lock, not the gauge `/metrics` serves, which is a tick old).
    #[must_use]
    pub fn queued_operations(&self) -> u64 {
        js_arm!(self, views::queued_operations());
        self.lock().engine.sched.queued_len() as u64
    }

    /// Writes to the state store the farm gave up on for good: held effects dropped as
    /// stale or on a fence, records over the cap.
    #[must_use]
    pub fn dropped_writes(&self) -> u64 {
        js_arm!(self, views::dropped_writes());
        self.lock().engine.write_drops
    }

    /// Held-back committed records the farm keeps for a refusing state store at most.
    #[doc(hidden)]
    #[must_use]
    pub fn with_held_cap(mut self, cap: usize) -> Self {
        self.settings.held_cap = cap;
        self.driver.set_held_cap(&self.settings);
        self
    }

    /// The memory runs of every operation this farm holds that was killed for memory,
    /// oldest first ([`kbf_sched::MemoryState::runs_by_operation`]): what a successor
    /// farm over the same state store must hold too.
    #[must_use]
    pub fn memory_runs(&self) -> BTreeMap<OperationId, Vec<MemoryRun>> {
        js_arm!(self, views::memory_runs());
        self.lock().engine.sched.memory_state().runs_by_operation()
    }

    /// The workers placement skips for each operation that has any
    /// ([`kbf_sched::Scheduler::avoided`]): what a successor farm over the same state
    /// store must hold too.
    #[must_use]
    pub fn avoided(&self) -> BTreeMap<OperationId, BTreeSet<WorkerId>> {
        js_arm!(self, views::avoided());
        self.lock().engine.sched.avoided()
    }

    /// Execute and WaitExecution operation streams open now
    /// (`kbf_front_open_executions`).
    #[must_use]
    pub fn open_executions(&self) -> u64 {
        js_arm!(self, views::open_executions());
        self.cache.metrics().execute.open_executions.get()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.driver.lock()
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
        js_arm!(
            self,
            hot::register(worker, instance, predecessor, reported, outbound, welcome)
        );
        let now = self.now();
        let mut state = self.lock();
        let stream = StreamId(state.side.next_stream);
        state.side.next_stream += 1;
        let (os, arch) = identity(&reported.caps);
        // The receiver is the stream's own response, alive until the stream ends.
        let _ = outbound.send(Ok(welcome));
        let link = Link {
            stream,
            instance: instance.clone(),
            outbound,
            newest_beat: 0,
            os,
            arch,
            protocol: reported.protocol,
        };
        let replaced = state.side.links.insert(worker.clone(), link);
        state.side.machine.register(worker);
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
        // Before the registration replaces the session row it reads, and before the
        // scheduler hears of the worker, whose recovered leases it books.
        state.claim_worker(now, worker, &instance);
        state
            .engine
            .record_session(now, worker, &instance, stopped, &reported);
        let event = Event::WorkerUp {
            worker: worker.clone(),
            instance,
            predecessor_stopped: stopped,
            capacity: reported.capacity,
            caps: reported.caps,
        };
        // No placement between the registration and the phase: a quarantined node
        // must not be offered a lease in between.
        let none = state.engine.sched.apply(kbf_sched::Input::new(now, event));
        debug_assert!(none.is_empty(), "a registration has no effect: {none:?}");
        state.phase(now, worker, reported.phase);
        stream
    }

    /// A `Hello` resent on `stream`: the node report or the node's phase changed.
    /// Changes the worker's capacity, capabilities and phase, and opens no session;
    /// ignored if `stream` was replaced. The protocol version `reported` names is not
    /// taken: the stream's is the first `Hello`'s, which `Welcome` answered.
    ///
    /// A `Preparing` or `Restoring` phase naming a lease the scheduler holds on the
    /// worker, whose `Start` went to an earlier session, sends that `Start` again (the
    /// daemon restarted for the session switch); one naming a lease of this process it
    /// does not hold there is sent a `Cancel`, so the daemon cleans up after it. A
    /// change into or out of quarantine is logged as an attention item.
    pub fn resize(&self, worker: &WorkerId, stream: StreamId, reported: Reported) {
        js_arm!(self, hot::resize(worker, stream, reported));
        let now = self.now();
        let mut state = self.lock();
        let (os, arch) = identity(&reported.caps);
        // The newest stream's link takes the new identity; a replaced stream's is ignored.
        let current = state
            .side
            .links
            .get_mut(worker)
            .filter(|link| link.stream == stream)
            .map(|link| (link.os, link.arch) = (os, arch))
            .is_some();
        if current {
            let event = Event::Capacity {
                worker: worker.clone(),
                capacity: reported.capacity,
                caps: reported.caps,
            };
            let none = state.engine.sched.apply(kbf_sched::Input::new(now, event));
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
        js_arm!(self, views::node_status(worker, stream, status));
        let received = self.ambient.unix_ms();
        let mut state = self.lock();
        if !state.side.is_current(worker, stream) {
            return Vec::new();
        }
        take_node_status(&mut state.side.machine, worker, status, received)
    }

    /// An `UpdateStatus` on `stream`: kept as the worker's newest, unless `stream` was
    /// replaced (a newer stream sends its own), exactly as [`Self::node_status`] keeps a
    /// `NodeStatus`. Returns whether it was kept.
    pub fn update_status(&self, worker: &WorkerId, stream: StreamId, status: UpdateStatus) -> bool {
        js_arm!(self, views::update_status(worker, stream, status));
        let report = UpdateReport::new(status);
        let mut state = self.lock();
        if !state.side.is_current(worker, stream) {
            return false;
        }
        state.side.machine.report_update(worker, report);
        true
    }

    /// Every node registered since this farm started, as the update node step reads it, in
    /// node-id order.
    pub fn update_look(&self) -> Vec<NodeLook> {
        js_arm!(self, views::update_look());
        let mut state = self.lock();
        state.side.note_closed_streams();
        let (engine, side) = (&state.engine, &state.side);
        side.machine
            .nodes()
            .map(|worker| {
                let link = side.links.get(worker);
                let identity = (
                    link.map(|l| l.os.clone()).unwrap_or_default(),
                    link.map_or("", |l| l.arch),
                );
                nodes::look_of(&engine.sched, &side.machine, identity, worker)
            })
            .collect()
    }

    /// The wall clock as a farm time: milliseconds since the Unix epoch. The update rows
    /// store times, and a restart must not reset their origin (unlike [`Farm::now`]).
    pub fn wall_now(&self) -> FarmTime {
        FarmTime::from_millis(self.ambient.unix_ms())
    }

    /// Cordons, drains or uncordons `worker` (see `kbf_sched::Cordon`), and returns
    /// the node as it is now. A drain's deadline counts from now, and is written on the
    /// state store's clock. A node connected to another frontend of the state store is
    /// placed too: that frontend applies the row at its next intake poll ([`Farm::intake`]).
    ///
    /// # Errors
    /// `worker` has never registered with this frontend, and no frontend of the state store
    /// has claimed it; the store cannot say whether one has, and `worker` is not connected
    /// here; or the store did not take the node's `cordon/` row. In the last two the action is
    /// not applied here either: the row is written first, and the scheduler follows it.
    pub fn place(&self, worker: &WorkerId, action: NodeAction) -> Result<NodeView, PlaceError> {
        js_arm!(self, lifecycle::place(worker, action));
        let now = self.now();
        let mut state = self.lock();
        state.side.note_closed_streams();
        // A node connected to another frontend of the state store is placed too: its
        // `cordon/` row is the other frontend's to apply (the intake poll).
        if state.side.machine.node(worker).is_none() {
            match state.engine.known_elsewhere(worker) {
                Some(true) => {}
                Some(false) => return Err(UnknownNode(worker.as_str().to_owned()).into()),
                None => return Err(NodeUnread(worker.as_str().to_owned()).into()),
            }
        }
        // A drain's deadline is written on the store's clock: the frontends that apply the
        // row have clocks of their own.
        let written = state.engine.durably("a cordon", |d| {
            d.cordoned(worker, action.stored(d.store_ms()))
        });
        if !written {
            return Err(PlaceError::Unwritten(worker.as_str().to_owned()));
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

    /// Expires leases of silent workers and places queued work. A silent lease lost
    /// past its operation's farm reruns finishes the operation: its callers are
    /// answered (nothing is cached), as a heartbeat's are. Logs each drain that has
    /// paused at its deadline, once.
    pub async fn tick(&self) {
        js_arm_async!(self, hot::tick());
        let now = self.now();
        let settled = {
            let mut state = self.lock();
            let answers = state.feed(now, Event::Tick);
            state.flush_learned();
            self.publish(&state);
            let paused: BTreeSet<WorkerId> = state
                .side
                .machine
                .nodes()
                .filter(|w| matches!(state.engine.sched.cordon(w), Some(Cordon::Paused { .. })))
                .cloned()
                .collect();
            for worker in paused.difference(&state.side.paused) {
                let leases = state.engine.sched.leases_on(worker).len();
                tracing::warn!(%worker, leases, "drain paused at its deadline; leases run on");
            }
            state.side.paused = paused;
            answers
                .iter()
                .map(|a| state.settle(a))
                .collect::<Vec<Settled>>()
        };
        for settled in settled {
            self.deliver(settled).await;
        }
    }
}

impl Dispatch for Farm {
    fn submit(&self, mut submission: Submission) -> Result<Ticket, Status> {
        js_arm!(self, hot::submit(submission));
        // The front keys every request by an action; the callers and tickets of the
        // server name that action.
        let key = ActionKey::try_from(&submission.request.key)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        // An action with no program and no output names no shape: all such actions would
        // share one key.
        let shaped =
            !submission.described.arguments.is_empty() || !submission.described.outputs.is_empty();
        // Outside the lock: hashes the action's shape and drops its argv and paths.
        let usage = self
            .shapes
            .subject(std::mem::take(&mut submission.described));
        let shape = submission.call.as_ref().map(|_| usage.recorded());
        let now = self.now();
        let platform = Platform::of(&submission.request.needs);
        let waiting = std::time::Instant::now();
        let mut state = self.lock();
        self.cache.metrics().sched_lock_wait.since(waiting);
        let size_key = state
            .side
            .learned
            .keys()
            .then(|| {
                Keyable {
                    instance: &key.instance,
                    platform: &platform,
                    shape: usage.key,
                    shaped,
                    kind: submission.request.kind,
                    gpus: submission.request.resources.gpus,
                    memory: submission.evidence.memory_source,
                }
                .key()
            })
            .flatten();
        let held = state.admit(
            &submission.request,
            &usage,
            size_key,
            submission.max_wall,
            now,
        )?;
        // The operation this submit queues or joins: the twin the scheduler holds, the one
        // another frontend leases (which the scheduler does not hold), or the next it numbers.
        let twin = state.engine.sched.twin(&submission.request);
        let (operation, joined) = match held {
            Held::Elsewhere(operation) => (operation, true),
            Held::Here => (
                twin.unwrap_or_else(|| state.engine.sched.next_operation()),
                twin.is_some(),
            ),
        };
        let action = key.action;
        let instance = key.instance.clone();
        let waiter = state.side.machine.submit(Waiter {
            key,
            kind: submission.request.kind,
            do_not_cache: submission.request.do_not_cache,
            queued: self.ambient.wall(),
            usage,
            platform,
        });
        let name = state
            .side
            .name_of(state.engine.durable.is_some(), waiter, operation);
        let predicted = state.learn_submit(
            waiter,
            operation,
            &name,
            &submission.request,
            &submission.evidence,
            joined,
            size_key,
        );
        if let Some((call, shape)) = submission.call.take().zip(shape) {
            let call = call.with_shape(shape).with_evidence(submission.evidence);
            state.side.recording.submit(call, &name, operation, joined);
        }
        let (stage, receiver) = watch::channel(Stage::Queued);
        state.side.stages.insert(waiter, stage);
        // The longest the operation this submit allocates may run is given before the submit
        // is fed: its first placement round follows, and the whole-node drain order and
        // deadlines read it (nothing reads it with them off). A twin joined keeps the one
        // it was given.
        if let Some(wall) = submission.max_wall.filter(|_| !joined) {
            state.engine.sched.set_max_wall(operation, wall);
        }
        // The operation this submit allocates (none if it joins a twin, which keeps its
        // own key) is named before the submit is fed: its first placement round follows.
        if let Some(key) = size_key.filter(|_| !joined) {
            let next = state.engine.sched.next_operation();
            state.engine.sched.set_size_key(next, key);
        }
        // Its predicted cost is given before the submit is fed too (`--placement-predictions
        // on`; none for a twin joined, which keeps the one it was given).
        if let Some(prediction) = predicted {
            state.engine.sched.set_cost(operation, prediction);
        }
        match held {
            Held::Here => state.feed_quiet(
                now,
                Event::Submit {
                    waiter,
                    request: submission.request,
                },
            ),
            // The operation is another frontend's: this scheduler never holds it, and the
            // caller is shown what the store says of it at the intake polls.
            Held::Elsewhere(_) => state.engine.keep_following(operation, vec![waiter]),
        }
        // A caller that joined a twin already started sees it executing, and so does one
        // that follows an operation another frontend's daemon runs.
        let joined_started = state.side.machine.started_operations().any(|operation| {
            state
                .engine
                .sched
                .waiters(operation)
                .is_some_and(|w| w.contains(&waiter))
        });
        let running = joined_started || matches!(held, Held::Elsewhere(_));
        if let Some(stage) = state.side.stages.get(&waiter).filter(|_| running) {
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

    /// A name `operations/<id>` is answered by any frontend of the state store, from the
    /// scheduler if it holds the operation and from the store if not
    /// ([`State::stored_ticket`]); a farm with no store answers none. A name of the old form,
    /// `operations/{term}-{waiter}`, is answered only for a caller this process holds.
    fn wait(&self, name: &str) -> Option<Ticket> {
        js_arm!(self, hot::wait(name));
        let mut state = self.lock();
        if let Some(operation) = parse_stored_name(name) {
            return state.stored_ticket(operation);
        }
        let (id, waiter) = state.side.machine.named(name)?;
        let stage = state.side.stages.get(&id)?;
        Some(Ticket {
            name: name.to_owned(),
            instance: waiter.key.instance.clone(),
            action: waiter.key.action,
            stage: stage.subscribe(),
        })
    }
}

impl State {
    /// [`Engine::feed`] with the farm's own side.
    fn feed(&mut self, now: FarmTime, event: Event) -> Vec<Answer> {
        self.engine.feed(&mut self.side, now, event)
    }

    /// [`Engine::feed_quiet`] with the farm's own side.
    fn feed_quiet(&mut self, now: FarmTime, event: Event) {
        self.engine.feed_quiet(&mut self.side, now, event);
    }

    /// [`Engine::forget_dropped`] with the farm's own side.
    fn forget_dropped(&mut self) {
        self.engine.forget_dropped(&mut self.side);
    }

    /// [`Engine::admit`] with the farm's own side.
    fn admit(
        &mut self,
        request: &kbf_sched::Request,
        subject: &crate::action_usage::Subject,
        size_key: Option<u64>,
        max_wall: Option<Duration>,
        now: FarmTime,
    ) -> Result<Held, Status> {
        self.engine
            .admit(&mut self.side, request, subject, size_key, max_wall, now)
    }

    /// [`Engine::claim_worker`] with the farm's own side.
    fn claim_worker(&mut self, now: FarmTime, worker: &WorkerId, instance: &DaemonInstance) {
        self.engine
            .claim_worker(&mut self.side, now, worker, instance);
    }
}

/// The engine's operations with the farm's own side, as the tests call them.
#[cfg(test)]
impl State {
    fn take_in(&mut self, operation: OperationId, version: kbf_state::Version) {
        self.engine.take_in(&mut self.side, operation, version);
    }

    fn settle_requeues(&mut self, given_up: &[kbf_sched::Requeue]) {
        self.engine.settle_requeues(&mut self.side, given_up);
    }

    fn write_estimates(&mut self, rows: &[crate::learned::Row]) {
        self.engine.write_estimates(&mut self.side, rows);
    }

    fn refuse(&mut self, refusal: &kbf_types::Refusal) {
        self.engine.refuse(&mut self.side, refusal);
    }

    fn register_stored(&mut self, op: &durable::StoredOperation, size_key: u64) {
        self.side.register_stored(op, size_key);
    }
}

/// Now on the wall clock, in milliseconds since the Unix epoch.
fn unix_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
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
mod cordon_owner_tests;
mod from_listeners;
#[cfg(test)]
mod learned_tests;
#[cfg(test)]
mod placement_tests;
mod prediction;
#[cfg(test)]
mod prediction_tests;
#[cfg(test)]
mod storage_down_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod whole_node_recovery_tests;
#[cfg(test)]
mod whole_node_tests;
