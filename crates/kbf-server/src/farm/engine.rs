//! The feed loop of the farm core, apart from everything that is the build farm's own.
//!
//! [`Engine`] holds the scheduler, the state store handle and the bookkeeping that keeps a
//! committed record's effects behind the store's write of its rows. It decides when an
//! effect is carried out and when the rows it settles are written; it knows no REAPI
//! message, no caller channel and no action cache. Everything of that kind is an
//! [`Adapter`] call, made under the same lock as the feed: the effects of a record
//! (`Commit`, `Start`, the offer before a `Start`) are carried out inside the feed, and an
//! `Answer` is returned for the caller to deliver after the lock is released.
//!
//! `Engine` is not generic and takes `&mut dyn Adapter`, so a test binary's run of any of
//! its methods counts toward one instantiation of it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, SystemTime};

use kbf_front::{Finished, Stage};
use kbf_sched::{
    DaemonInstance, Event, FARM_RERUNS, Fate, FloorChange, Input, KeyFloor, Requeue, Scheduler,
};
use kbf_types::{
    Answer, ControlRecord, Effect, FarmTime, LeaseGrant, LeaseId, OperationId, Refusal, StartLease,
    StateMachine, WaiterId, Waiting, WorkerId,
};

use super::ambient::Ambient;
use super::durable::intake::Intake;
use super::durable::{Durable, StoredOperation};
use crate::learned::Row;

/// How many committed records the farm holds for a state store that refuses them.
pub(super) const HELD_CAP: usize = 4096;

/// What an [`Adapter`] hook may read of the engine: the scheduler as it stands, and
/// whether operations are named by the ids of a state store.
#[derive(Clone, Copy)]
pub(in crate::farm) struct View<'a> {
    /// The scheduler, which the hook may not change.
    pub sched: &'a Scheduler,
    /// The farm has a state store, so an operation is named by its id (`operations/<id>`).
    pub stored: bool,
}

/// What the learned sizes made of a lease the scheduler gave up: the summaries to write
/// once the requeues are, and the key floors the estimator's decay lowered.
pub(in crate::farm) struct LearnedRequeue {
    /// The size summaries an own-limit kill changed.
    pub rows: Vec<Row>,
    /// The key floors lowered as `(key, floor before the decay, floor asked)`; `None`
    /// when the learned sizes were not consulted for a lowering at all.
    pub lowered: Option<Vec<(u64, u64, u64)>>,
}

/// Everything the engine asks of the farm around it. Called under the farm's lock, so a
/// hook neither blocks nor calls back into the engine: it sends on a channel, changes a
/// map, writes a log line. Besides ids and the scheduler's types, the signatures still name
/// kbf types: `Stage` (which the engine also builds, in `stage_for`), `Finished`, the learned
/// `Row`, `StoredOperation` and the estimator's `Key`, and the engine holds `Durable`. Each
/// becomes a type the engine owns or the consumer supplies before the engine can be shared.
pub(in crate::farm) trait Adapter {
    /// Every node the farm has heard from, in node-id order.
    fn registered(&self) -> Vec<WorkerId>;
    /// The nodes whose newest stream is open.
    fn connected(&self) -> BTreeSet<WorkerId>;
    /// Whether `worker`'s newest stream is open.
    fn stream_open(&self, worker: &WorkerId) -> bool;
    /// Farm time `now`, at the start of every feed.
    fn clock(&mut self, now: FarmTime);
    /// The name of `operation` as the log writes it.
    fn name(&self, view: &View<'_>, operation: OperationId) -> String;

    /// Tells the node a lease is placed on it, before the grant commits.
    fn offer(&mut self, view: &View<'_>, grant: &LeaseGrant);
    /// Sends the `Start` of a committed lease and marks its callers executing.
    fn start(&mut self, view: &View<'_>, start: &StartLease);
    /// Logs an operation the scheduler finishes failed.
    fn failed(&self, view: &View<'_>, answer: &Answer);
    /// Shows `stage` to the callers of `operation`, if it changes what they see.
    fn show(&mut self, view: &View<'_>, operation: OperationId, stage: &Stage);
    /// Answers the callers of an operation the scheduler refused, and keeps them for the
    /// finished retention.
    fn refuse(&mut self, view: &View<'_>, refusal: &Refusal);
    /// Forgets the callers, and their channels, of every finished operation `held` no
    /// longer says the engine holds.
    fn forget_dropped(&mut self, held: &dyn Fn(OperationId) -> bool);
    /// Notes that the scheduler gave up `requeue`'s lease.
    fn requeued(&mut self, requeue: &Requeue);

    /// What the learned sizes make of `requeue`'s lease being given up.
    fn learned_requeue(&mut self, view: &View<'_>, requeue: &Requeue) -> LearnedRequeue;
    /// The estimator key the learned sizes name `key` by, if they do.
    fn summary_key(&self, key: u64) -> Option<kbf_estimator::Key>;
    /// Summaries the state store refused, to be written again later.
    fn estimates_refused(&mut self, refused: &[Row]);
    /// Whether the learned sizes read size keys.
    fn size_keys(&self) -> bool;

    /// How the operation of `effect` ended, if the effect ends one: what its callers are
    /// told, which the `done/` row keeps too.
    fn ending(&self, effect: &Effect) -> Option<Finished>;
    /// Drops what is kept for the report of `lease`, whose answer will never be written.
    fn forget_parcel(&mut self, lease: LeaseId);
    /// Keeps what is kept for the reports whose lease `waits` says still wait.
    fn retain_parcels(&mut self, waits: &dyn Fn(LeaseId) -> bool);

    /// Takes up `op`, an operation the state store holds, as a caller of its own that no
    /// client waits on. Returns that caller.
    fn adopt(&mut self, op: &mut StoredOperation, now: SystemTime) -> WaiterId;
    /// The callers among `callers` still waiting: not answered, and with a stream open.
    fn waiting_among(&self, callers: &[WaiterId]) -> Vec<WaiterId>;
    /// Shows `stage` to `callers`, waking their watchers only if it changed.
    fn show_callers(&mut self, callers: &[WaiterId], stage: &Stage);
    /// Keeps `callers` of the finished `operation` until the engine drops it.
    fn keep_finished(&mut self, operation: OperationId, callers: Vec<WaiterId>);
    /// Notes the end of `operation`, which another frontend ran, with what its callers are
    /// told.
    fn followed(&mut self, operation: OperationId, finished: &Finished);
    /// The operations clients ask for by name that the engine does not hold, whose
    /// channels someone still watches and which have not been shown a result.
    fn watched(&mut self) -> Vec<OperationId>;
    /// Shows `stage` on the channel of the watched `operation`.
    fn show_watched(&mut self, operation: OperationId, stage: &Stage);
}

/// The scheduler, the state store, and the bookkeeping between them.
#[derive(Debug)]
pub(in crate::farm) struct Engine {
    /// The farm's [`Ambient`], for the work done under the lock.
    pub(in crate::farm) ambient: Ambient,
    pub(in crate::farm) sched: Scheduler,
    /// How long the scheduler keeps a finished operation; the state store keeps its
    /// `done/` row at least as long.
    pub(in crate::farm) finished_retention: Duration,
    /// The operations a `Start` was sent for, marked before the `Start` leaves and never
    /// cleared while the scheduler holds the operation: a drain ends the streams of an
    /// operation that waits for a worker only if it is not in here.
    pub(in crate::farm) ever_started: BTreeSet<OperationId>,
    /// The nodes the drain cordoned itself when it closed this farm's intake
    /// ([`Engine::hold_placements`]), as opposed to an operator's cordon: an operation
    /// that waits only for these is queued, not stuck, since the other frontends take it.
    pub(in crate::farm) drain_held: BTreeSet<WorkerId>,
    /// The operations the scheduler says no live worker can run now, with its reason: the
    /// ones whose stage [`Engine::stage_for`] decides again when what it depends on changes.
    pub(in crate::farm) stuck: BTreeMap<OperationId, String>,
    /// The state store every change is written through before it is acknowledged, if
    /// the farm has one ([`Farm::with_state`](super::Farm::with_state)).
    pub(in crate::farm) durable: Option<Durable>,
    /// Leases given up that the state store has not yet been told of, oldest first.
    /// Until it has, it still holds the lease and no queue row for the operation, so
    /// every one is written again before anything that follows it is
    /// ([`Engine::log_requeues`]).
    pub(in crate::farm) unsettled: Vec<(Requeue, Option<u32>, Option<Fate>, FloorChange)>,
    /// The effects of committed records whose rows the state store has not taken, one
    /// batch per record, oldest first. A batch leaves only when the store has taken every
    /// row of it, in order, and nothing is carried out (no `Start`, no answer) before its
    /// row is written, so a store that refuses holds the farm's later effects behind the
    /// earliest refused one ([`Engine::release_unwritten`]).
    pub(in crate::farm) unwritten: VecDeque<VecDeque<Effect>>,
    /// The `Result`s the farm took but has not acknowledged, by lease, with the worker
    /// that sent each: their finish waits in `unwritten` for the state store. The
    /// acknowledgement goes out when that finish is settled, and never if it is dropped
    /// instead ([`Engine::forget_details`]).
    pub(in crate::farm) owed: BTreeMap<LeaseId, WorkerId>,
    /// The operation of each lease whose answer was settled, while the scheduler keeps
    /// the operation: a copy of its `Result` is a duplicate, not an unknown lease.
    pub(in crate::farm) answered: BTreeMap<LeaseId, OperationId>,
    /// Writes given up on for good: dropped as stale or fenced, or over the cap
    /// ([`Farm::dropped_writes`](super::Farm::dropped_writes)).
    pub(in crate::farm) write_drops: u64,
    /// The most committed records `unwritten` holds before a new one is dropped.
    pub(in crate::farm) held_cap: usize,
    /// The scheduler was told the state store is unavailable ([`Engine::watch_store`]).
    pub(in crate::farm) store_down: bool,
    /// When each node's session row in the state store was last written, or renewed: the
    /// next renewal falls due a renewal interval later.
    pub(in crate::farm) renewed: BTreeMap<WorkerId, FarmTime>,
    /// When each node's session row was last written successfully, at its registration or
    /// a renewal. Its heartbeats are acknowledged only while that was lately
    /// ([`ACK_WINDOW`](super::ACK_WINDOW)).
    pub(in crate::farm) session_ok: BTreeMap<WorkerId, FarmTime>,
    /// The nodes whose `Hello` found the state store unreachable, so this frontend has not
    /// claimed them, with the daemon process that said it ([`Engine::claim_pending`]).
    pub(in crate::farm) unclaimed: BTreeMap<WorkerId, DaemonInstance>,
    /// A handover took the work from this farm ([`Farm::fence`](super::Farm::fence)): it
    /// feeds the scheduler nothing and queues nothing.
    pub(in crate::farm) fenced: bool,
    /// A drain closed this farm's intake ([`Farm::close_intake`](super::Farm::close_intake)):
    /// it takes in no queued work of the other frontends and places none on its nodes, and
    /// goes on with what it holds.
    pub(in crate::farm) closed: bool,
    /// For each operation this farm's callers wait on ([`Farm::follow`](super::Farm::follow))
    /// that the last looks found neither unfinished nor ended, how many looks in a row. Only
    /// logged: no look count ends a caller's wait.
    pub(in crate::farm) untraced_kept: BTreeMap<OperationId, u32>,
    /// Lowered key floors the state store has not taken yet ([`Engine::lower_key_floors`]).
    pub(in crate::farm) unwritten_floors: Vec<KeyFloor>,
    /// What the intake poll keeps between two looks at the state store
    /// ([`Farm::intake`](super::Farm::intake)).
    pub(in crate::farm) intake: Intake,
}

impl Engine {
    /// An engine over `sched`, with no state store.
    pub(in crate::farm) fn new(
        sched: Scheduler,
        finished_retention: Duration,
        ambient: Ambient,
    ) -> Self {
        Self {
            ambient,
            sched,
            finished_retention,
            ever_started: BTreeSet::new(),
            drain_held: BTreeSet::new(),
            stuck: BTreeMap::new(),
            durable: None,
            unsettled: Vec::new(),
            unwritten: VecDeque::new(),
            owed: BTreeMap::new(),
            answered: BTreeMap::new(),
            write_drops: 0,
            held_cap: HELD_CAP,
            store_down: false,
            renewed: BTreeMap::new(),
            session_ok: BTreeMap::new(),
            unclaimed: BTreeMap::new(),
            fenced: false,
            closed: false,
            untraced_kept: BTreeMap::new(),
            unwritten_floors: Vec::new(),
            intake: Intake::default(),
        }
    }

    /// What an [`Adapter`] hook may read.
    pub(in crate::farm) fn view(&self) -> View<'_> {
        View {
            sched: &self.sched,
            stored: self.durable.is_some(),
        }
    }

    /// Feeds an input that cannot lead to an answer: only a `Report`, a heartbeat (a
    /// lease lost to a proven restart or a replacement) and a tick (a silent lease)
    /// propose a result, and only a committed result answers; those three deliver their
    /// answers themselves.
    pub(in crate::farm) fn feed_quiet(&mut self, a: &mut dyn Adapter, now: FarmTime, event: Event) {
        let answers = self.feed(a, now, event);
        debug_assert!(
            answers.is_empty(),
            "an answer without a report: {answers:?}"
        );
    }

    /// [`Engine::feed`] on a farm that is not fenced: feeds `event`, then a tick (so
    /// placement follows at once), and carries out the effects in order. Returns the
    /// answers, for the caller to deliver once the lock is released.
    pub(in crate::farm) fn carry_out(
        &mut self,
        a: &mut dyn Adapter,
        now: FarmTime,
        event: Event,
    ) -> Vec<Answer> {
        self.hold_placements(a, now);
        self.watch_store(now);
        self.claim_pending(a, now);
        self.reserve_seqs();
        a.clock(now);
        // Only the inputs whose callers deliver answers ([`Engine::feed_quiet`]) release
        // effects an earlier refusal held back: one of them may be an answer.
        let answering = matches!(
            event,
            Event::Tick | Event::Heartbeat { .. } | Event::Report { .. }
        );
        let event = self.held_event(event);
        let mut effects: VecDeque<Effect> = self.sched.apply(Input::new(now, event)).into();
        effects.extend(self.sched.apply(Input::new(now, Event::Tick)));
        self.log_requeues(a);
        if answering {
            // Before this input's own effects: they follow what was held back.
            for effect in self.release_unwritten(a).into_iter().rev() {
                effects.push_front(effect);
            }
        }
        let mut answers = Vec::new();
        while let Some(effect) = effects.pop_front() {
            match effect {
                Effect::Commit(record) => {
                    // A refusal for want of a daemon waits on if another frontend's could
                    // run the operation: it is not committed, and is proposed again after
                    // another unservable wait.
                    if self.held_for_other_frontends(&record) {
                        continue;
                    }
                    if let ControlRecord::Lease(grant) = &record {
                        a.offer(&self.view(), grant);
                    }
                    // Appended is committed; the effects of the commit (the `Start`, the
                    // answer) are carried out only once the state store has taken what
                    // they settle. What it refuses is kept and written again, in order,
                    // until it takes it or a newer writer fences it.
                    let committed = Event::Committed(record);
                    let next = self.sched.apply(Input::new(now, committed));
                    if self.unwritten.len() >= self.held_cap {
                        self.write_drops += 1;
                        tracing::error!(cap = self.held_cap, "records over the cap: one dropped");
                        continue;
                    }
                    self.unwritten.push_back(next.into());
                    if answering || self.unwritten.len() == 1 {
                        effects.extend(self.release_unwritten(a));
                    }
                }
                Effect::Start(start) => self.start(a, &start),
                Effect::Answer(answer) => {
                    a.failed(&self.view(), &answer);
                    answers.push(answer);
                }
                Effect::Waiting(waiting) => self.waiting(a, &waiting),
                Effect::Refuse(refusal) => self.refuse(a, &refusal),
            }
        }
        self.log_requeues(a);
        self.forget_dropped(a);
        answers
    }

    /// Logs the leases the scheduler has given up since the last call, one INFO line
    /// each, naming the operation by its first caller's name. Called before any
    /// effect is carried out (an effect can finish an operation, and with a zero
    /// finished retention drop it and forget its callers) and again at the end of a
    /// feed. A requeue only names an operation the scheduler held when it was made;
    /// one dropped since is logged with an empty name, never looked up by index.
    pub(in crate::farm) fn log_requeues(&mut self, a: &mut dyn Adapter) {
        let given_up = self.sched.take_requeues();
        self.settle_requeues(a, &given_up);
        for requeue in given_up {
            a.requeued(&requeue);
            let Requeue {
                operation,
                lease,
                worker: node,
                reason,
            } = requeue;
            let farm_runs = self.sched.farm_runs(operation).map_or(0, <[_]>::len);
            let operation = a.name(&self.view(), operation);
            // One line: the coverage of a logged field counts only where it is logged.
            // `farm_runs` is the farm-fault budget spent so far, of `farm_budget` reruns
            // (a lease not started has a budget of its own, in the scheduler).
            tracing::info!(
                %operation, %lease, %node, %reason, farm_runs, farm_budget = FARM_RERUNS,
                "lease given up; requeued"
            );
        }
    }

    /// Forgets the callers, and their channels, of every finished operation the
    /// scheduler has dropped: a WaitExecution on one is NOT_FOUND from now on. Every
    /// input ends here.
    pub(in crate::farm) fn forget_dropped(&mut self, a: &mut dyn Adapter) {
        let (sched, intake) = (&self.sched, &self.intake);
        let held =
            |operation: OperationId| sched.state(operation).is_some() || intake.keeps(operation);
        a.forget_dropped(&held);
        self.answered
            .retain(|_, operation| sched.state(*operation).is_some());
        self.ever_started
            .retain(|operation| sched.state(*operation).is_some());
    }

    /// Marks the operation of a committed lease started, then has the adapter send its
    /// `Start`. The `Start` names the newest heartbeat taken on the worker's stream and
    /// the window after it in which the daemon may still act on the `Start`.
    fn start(&mut self, a: &mut dyn Adapter, start: &StartLease) {
        // Marked before the `Start` leaves: a stream that reads the stage later never
        // sees an operation that has a `Start` in flight as one that cannot start.
        self.ever_started.insert(start.operation);
        a.start(&self.view(), start);
    }

    /// Tells an operation's callers why it waits, or that it no longer waits for a
    /// worker that can run it. Logged, so an operator sees it too.
    fn waiting(&mut self, a: &mut dyn Adapter, waiting: &Waiting) {
        let operation = waiting.operation;
        let stage = match &waiting.reason {
            Some(why) => {
                if self.runnable_elsewhere(operation) {
                    tracing::info!(
                        %operation,
                        reason = %why,
                        "no worker of this frontend can run the operation; another frontend can"
                    );
                } else {
                    tracing::warn!(%operation, reason = %why, "no live worker can run the operation");
                }
                self.stuck.insert(operation, why.clone());
                self.stage_for(operation, why)
            }
            None => {
                tracing::info!(%operation, "a live worker can run the operation again");
                self.stuck.remove(&operation);
                Stage::Queued
            }
        };
        a.show(&self.view(), operation, &stage);
    }

    /// Whether a daemon session of another frontend, as the last intake poll listed them,
    /// could run `operation`. Its work is taken by that frontend's intake, which takes the
    /// queued operations it did not enqueue.
    fn runnable_elsewhere(&self, operation: OperationId) -> bool {
        self.sched.request(operation).is_some_and(|request| {
            self.intake
                .remote
                .iter()
                .any(|(caps, capacity)| request.runnable_on(caps, capacity))
        })
    }

    /// The stage of `operation`, which no live worker of this frontend can run now (`why`).
    /// A worker of another frontend can run it when that frontend's intake takes the queued
    /// operation; this frontend follows the state store until then. What each stage means
    /// to a caller:
    ///
    /// - `WaitingKept`: a drain keeps its streams. The operation has started before, or a
    ///   daemon session of another frontend could run it.
    /// - `Queued`: the only thing between it and a worker is this drain's own cordon, with
    ///   the intake closed, so it is queued for the other frontends like any queued work.
    /// - `Waiting`: a drain ends its streams. It never started, no daemon session of
    ///   another frontend could run it, and it is held by an operator's cordon (alone, or
    ///   beside the drain's) or by no worker at all.
    fn stage_for(&self, operation: OperationId, why: &str) -> Stage {
        let cordoned = self.sched.cordoned_could(operation);
        let only_drain = !cordoned.is_empty()
            && cordoned
                .iter()
                .all(|worker| self.drain_held.contains(worker));
        if self.ever_started.contains(&operation) || self.runnable_elsewhere(operation) {
            Stage::WaitingKept(why.to_owned())
        } else if only_drain {
            Stage::Queued
        } else {
            Stage::Waiting(why.to_owned())
        }
    }

    /// Decides again the stage of every operation that waits for a worker, after the
    /// daemons of the other frontends were read.
    pub(in crate::farm) fn republish_stuck(&mut self, a: &mut dyn Adapter) {
        let alive: BTreeSet<OperationId> = self.sched.queued().collect();
        self.stuck.retain(|operation, _| alive.contains(operation));
        for (operation, why) in self.stuck.clone() {
            let stage = self.stage_for(operation, &why);
            a.show(&self.view(), operation, &stage);
        }
    }

    /// Answers the callers of an operation the scheduler refused FAILED_PRECONDITION
    /// with the reason, and keeps them for the finished retention. Nothing is cached,
    /// so nothing is awaited.
    pub(in crate::farm) fn refuse(&mut self, a: &mut dyn Adapter, refusal: &Refusal) {
        a.refuse(&self.view(), refusal);
        self.forget_dropped(a);
    }
}
