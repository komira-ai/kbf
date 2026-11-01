//! What the farm keeps beside its [`Engine`](super::engine::Engine): the callers and their
//! channels, the worker streams, the invocation record, the learned sizes and the counters
//! operators read. [`Side`] is the engine's [`Adapter`]: it turns the engine's effects into
//! `Start` and `LeaseOffer` messages, stage changes and log lines.

use std::collections::{BTreeMap, BTreeSet};
use std::time::SystemTime;

use kbf_estimator::Key;
use kbf_front::{Finished, Stage};
use kbf_proto::worker::{self, LeaseOffer, Start, server_message};
use kbf_sched::{DaemonInstance, FARM_RERUNS, Requeue, RequeueReason, Scheduler};
use kbf_types::{
    Answer, Effect, FarmTime, LeaseGrant, LeaseId, OperationId, Outcome, Refusal, StartLease,
    WaiterId, WorkerId,
};
use tokio::sync::watch;

use super::ambient::Ambient;
use super::durable::StoredOperation;
use super::engine::{Adapter, LearnedRequeue, View};
use super::failed_actions::{Class, FailedActions};
use super::handover::show;
use super::{Detail, Outbound, START_VALIDITY_MS, StreamId, finished_of, refused_as, wire_lease};
use crate::action_usage::Subject;
use crate::learned::{Learned, Row};
use crate::machine::{Booked, FarmMachine, Platform, Sent, Waiter, stored_name};
use crate::run_record::Recording;
use crate::stamp::Stamp;

/// A worker's newest stream.
#[derive(Debug)]
pub(in crate::farm) struct Link {
    pub stream: StreamId,
    /// The daemon process that opened it.
    pub instance: DaemonInstance,
    pub outbound: Outbound,
    /// The seq of the newest heartbeat taken on the stream; 0 before the first. Each
    /// `Start` names it, and the daemon acts on the `Start` only within
    /// `START_VALIDITY` of having sent that heartbeat (or, for 0, its `Hello`).
    pub newest_beat: u64,
    /// The `os` and `arch` its newest `Hello` reported (empty if it named none).
    pub os: String,
    pub arch: &'static str,
    /// The version `Welcome` named for its first `Hello`: the lesser of the `Hello`'s and
    /// the server's newest (`None` if the `Hello` named none). Like the link, it is the newest stream's: a daemon that returns on
    /// another version replaces it, and a `Hello` resent on the stream does not.
    pub protocol: Option<u32>,
}

/// The `os` and `arch` entries of a node report.
pub(crate) fn identity(caps: &kbf_caps::NodeCaps) -> (String, &'static str) {
    let os = caps.exact.get("os").cloned().unwrap_or_default();
    (os, caps.cpu.arch().name())
}

/// The farm's own state: everything the farm holds that the engine does not.
#[derive(Debug)]
pub(in crate::farm) struct Side {
    /// The farm's [`Ambient`], for the work done under the lock.
    pub ambient: Ambient,
    /// The callers' records, the finished operations whose callers are kept, the
    /// leases whose `Start` was sent, and the node registry.
    pub machine: FarmMachine,
    /// The channel of each caller the machine keeps, forgotten with its record.
    pub stages: BTreeMap<WaiterId, watch::Sender<Stage>>,
    pub next_stream: u64,
    /// The newest stream of each node the machine has registered.
    pub links: BTreeMap<WorkerId, Link>,
    /// Nodes whose drain has paused, once logged.
    pub paused: BTreeSet<WorkerId>,
    /// Where the invocation record's lease and finish events go, and the operations it is
    /// told of.
    pub recording: Recording,
    /// The detail of a report whose answer is not settled yet, by lease: the answer's
    /// `done/` row is made from it when the store is written, and the answer is settled
    /// with it once the store has taken the rows.
    pub parked: BTreeMap<LeaseId, Detail>,
    /// The learned-sizes shadow ([`crate::learned`]); off unless `--learned-sizes`
    /// says otherwise.
    pub learned: Learned,
    /// The channel of each operation clients ask for by name that the scheduler does not
    /// hold, shown what the state store says of it at each intake poll while a ticket holds
    /// the channel ([`State::stored_ticket`](super::State::stored_ticket)).
    pub watched: BTreeMap<OperationId, watch::Sender<Stage>>,
    /// The pack-read counts on the newest heartbeat of each node that sent any
    /// ([`Farm::note_pack_reads`](super::Farm::note_pack_reads)).
    pub pack_reads: BTreeMap<WorkerId, worker::PackReadCounts>,
    /// The pack-write counts on the newest heartbeat of each node that sent any
    /// ([`Farm::note_pack_writes`](super::Farm::note_pack_writes)).
    pub pack_writes: BTreeMap<WorkerId, worker::PackWriteCounts>,
    /// The input cache counts on the newest heartbeat of each node that sent any
    /// ([`Farm::note_input_cache`](super::Farm::note_input_cache)).
    pub input_caches: BTreeMap<WorkerId, worker::InputCacheCounts>,
    /// What the accepted node-load readings of each node that sent any come to
    /// ([`Farm::note_node_load`](super::Farm::note_node_load)).
    pub node_loads: super::node_load::NodeLoads,
    /// The operations this process settled as failed, per class
    /// (`kbf_execute_failed_actions_total`, [`Farm::failed_actions`](super::Farm::failed_actions)).
    pub failed: FailedActions,
}

impl Side {
    /// The farm's own state for a farm whose callers are named in `term`.
    pub(in crate::farm) fn new(term: u64, ambient: Ambient) -> Self {
        Self {
            ambient,
            machine: FarmMachine::new(term),
            stages: BTreeMap::new(),
            next_stream: 0,
            links: BTreeMap::new(),
            paused: BTreeSet::new(),
            recording: Recording::new(),
            parked: BTreeMap::new(),
            learned: Learned::new(crate::learned::LearnedSizes::Off, 0, 0),
            watched: BTreeMap::new(),
            pack_reads: BTreeMap::new(),
            pack_writes: BTreeMap::new(),
            input_caches: BTreeMap::new(),
            node_loads: BTreeMap::new(),
            failed: FailedActions::default(),
        }
    }

    /// The REAPI name of `operation`, as the log names it: `operations/<id>` in a farm over
    /// a state store; in a farm with none, its first waiter's, or empty for an operation
    /// without one.
    pub(in crate::farm) fn operation_name(
        &self,
        view: &View<'_>,
        operation: OperationId,
    ) -> String {
        if view.stored {
            return stored_name(operation);
        }
        self.first_waiter(view.sched, operation)
            .map_or_else(String::new, |(id, _)| self.machine.name(id))
    }

    /// The REAPI name the caller `waiter` of `operation` is given: the operation's own
    /// (`operations/<id>`, which any frontend of the store answers) in a farm over a state
    /// store, and in a farm with none the caller's in this process's term.
    pub(in crate::farm) fn name_of(
        &self,
        stored: bool,
        waiter: WaiterId,
        operation: OperationId,
    ) -> String {
        if stored {
            return stored_name(operation);
        }
        self.machine.name(waiter)
    }

    /// The first waiter of `operation`: its key and lease kind are the operation's.
    pub(in crate::farm) fn first_waiter(
        &self,
        sched: &Scheduler,
        operation: OperationId,
    ) -> Option<(WaiterId, &Waiter)> {
        let first = *sched.waiters(operation)?.first()?;
        self.machine.waiter(first).map(|waiter| (first, waiter))
    }

    /// Whether `stream` is `worker`'s newest.
    pub(in crate::farm) fn is_current(&self, worker: &WorkerId, stream: StreamId) -> bool {
        self.links.get(worker).is_some_and(|l| l.stream == stream)
    }

    /// Tells the machine of each node whose newest stream has closed (its response
    /// side is gone), so its registry says the node is not connected.
    pub(in crate::farm) fn note_closed_streams(&mut self) {
        for (worker, link) in &self.links {
            if link.outbound.is_closed() {
                self.machine.disconnect(worker);
            }
        }
    }

    /// Sends `worker` the message `build` makes from `operation`'s first waiter.
    ///
    /// Both are always there, so nothing here branches on them: the scheduler places
    /// work only on a worker that registered (and a worker's link outlives its stream),
    /// and only operations a waiter submitted (waiters leave only once it is finished).
    /// A stream that has just ended drops the message; its lease is reconciled later.
    fn send_for(
        &self,
        sched: &Scheduler,
        worker: &WorkerId,
        operation: OperationId,
        build: impl FnOnce(&Waiter) -> server_message::Message,
    ) {
        let message = self
            .first_waiter(sched, operation)
            .map(|(_, waiter)| build(waiter));
        // Whether it was sent: a stream that has just ended drops it.
        let _sent = self.links.get(worker).zip(message).map(|(link, message)| {
            link.outbound
                .send(Ok(worker::ServerMessage {
                    message: Some(message),
                }))
                .is_ok()
        });
    }
}

impl Adapter for Side {
    fn registered(&self) -> Vec<WorkerId> {
        self.machine.nodes().cloned().collect()
    }

    fn connected(&self) -> BTreeSet<WorkerId> {
        self.links
            .iter()
            .filter(|(_, link)| !link.outbound.is_closed())
            .map(|(worker, _)| worker.clone())
            .collect()
    }

    fn stream_open(&self, worker: &WorkerId) -> bool {
        let link = self.links.get(worker);
        link.is_some_and(|link| !link.outbound.is_closed())
    }

    fn clock(&mut self, now: FarmTime) {
        self.learned.clock(now);
    }

    fn name(&self, view: &View<'_>, operation: OperationId) -> String {
        self.operation_name(view, operation)
    }

    /// Tells the worker a lease is placed on it, before the grant commits.
    fn offer(&mut self, view: &View<'_>, grant: &LeaseGrant) {
        let (operation, lease, node) = (
            self.operation_name(view, grant.operation),
            grant.lease,
            &grant.worker,
        );
        // The key joins the daemon's `lease phases:` line (`lease=`) to the action, also
        // for a lease whose result is never taken (cancelled).
        let first = self.first_waiter(view.sched, grant.operation);
        let key = first.map(|(_, w)| tracing::field::display(w.usage.key));
        // What the action asked for, parsed (`-` when it named none), so an audit can
        // find a Linux action leased to a Mac.
        let platform = first.map(|(_, w)| &w.platform);
        let os_family = platform.map_or("-", |p| p.os_family);
        let isa = platform.map_or("-", |p| p.isa.as_str());
        tracing::info!(%operation, %lease, %node, key, %os_family, %isa, "lease granted");
        self.recording.granted(grant);
        self.send_for(view.sched, &grant.worker, grant.operation, |w| {
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
    fn start(&mut self, view: &View<'_>, start: &StartLease) {
        // Its callers are shown executing before the `Start` leaves: a stream that reads
        // the stage later never sees an operation that has a `Start` in flight as one
        // that cannot start.
        let waiters = view.sched.waiters(start.operation).unwrap_or_default();
        for stage in waiters.iter().filter_map(|id| self.stages.get(id)) {
            stage.send_replace(Stage::Executing);
        }
        let heartbeat_seq = self.links.get(&start.worker).map_or(0, |l| l.newest_beat);
        #[cfg(test)]
        super::BEFORE_START.with(|hook| {
            if let Some(hook) = &*hook.borrow() {
                let waiters = view.sched.waiters(start.operation).unwrap_or_default();
                let stages = waiters.iter().filter_map(|id| self.stages.get(id));
                hook(stages.map(|stage| stage.borrow().clone()).collect());
            }
        });
        self.send_for(view.sched, &start.worker, start.operation, |w| {
            server_message::Message::Start(Start {
                lease_id: Some(wire_lease(start.lease)),
                kind: start.kind.name().to_owned(),
                action_digest: Some(kbf_front::digest_to_proto(&w.key.action)),
                millicpus: start.resources.cpu_millis,
                memory_bytes: start.resources.memory_bytes,
                heartbeat_seq,
                valid_for_ms: START_VALIDITY_MS,
                device_id: start.device.clone().unwrap_or_default(),
                // Nothing sets these yet: an unset fence is a self-fence, as every
                // lease has always been.
                job: None,
                fence: 0,
            })
        });
        let now = self.ambient.wall();
        // An operation with a `Start` has its first waiter (see `send_for`), which names
        // the action.
        let sent = self
            .first_waiter(view.sched, start.operation)
            .map(|(_, w)| Sent {
                operation: start.operation,
                action: w.key.action,
                stamp: Stamp {
                    queued: w.queued,
                    started: now,
                },
                booked: Some(Booked {
                    cpu_millis: start.resources.cpu_millis,
                    memory_bytes: start.resources.memory_bytes,
                }),
            });
        sent.into_iter()
            .for_each(|sent| self.machine.start(start.lease, sent));
        self.learned.on_start(start);
        self.recording.started(start);
    }

    /// Logs, as one WARN line, an operation the scheduler finishes failed: the failure
    /// kind, and how much of the farm-fault budget and the memory ladder its runs used
    /// (`farm_runs` at `farm_budget` is the farm budget spent, so no rerun is left).
    /// Operations that complete are logged where their lease's result is taken
    /// ([`State::take_report`](super::State)).
    fn failed(&self, view: &View<'_>, answer: &Answer) {
        if let Outcome::Failed(failure) = answer.outcome {
            let operation = self.operation_name(view, answer.operation);
            tracing::warn!(
                %operation,
                lease = %answer.lease,
                ?failure,
                farm_runs = answer.farm_runs.len(),
                farm_budget = FARM_RERUNS,
                memory_runs = answer.memory_runs.len(),
                "operation finished failed"
            );
        }
    }

    /// Shows `stage` to the callers of `operation`. A caller that joins a waiting
    /// operation is told again; the others, who already know, see no repeated update.
    fn show(&mut self, view: &View<'_>, operation: OperationId, stage: &Stage) {
        let waiters = view.sched.waiters(operation).unwrap_or_default();
        for channel in waiters.iter().filter_map(|id| self.stages.get(id)) {
            channel.send_if_modified(|now| {
                let changed = *now != *stage;
                if changed {
                    now.clone_from(stage);
                }
                changed
            });
        }
    }

    fn refuse(&mut self, _view: &View<'_>, refusal: &Refusal) {
        tracing::warn!(operation = %refusal.operation, reason = %refusal.reason, "operation refused");
        self.recording.refused(refusal);
        let finished = refused_as(refusal);
        self.failed.count(Class::Farm);
        // A lease given up before the operation was refused may still be listed.
        self.machine.forget_leases(refusal.operation);
        for stage in refusal.waiters.iter().filter_map(|id| self.stages.get(id)) {
            stage.send_replace(Stage::Done(finished.clone()));
        }
        self.machine
            .keep_finished(refusal.operation, refusal.waiters.clone());
    }

    fn forget_dropped(&mut self, held: &dyn Fn(OperationId) -> bool) {
        let forgotten = self.machine.forget_dropped(held);
        for id in forgotten {
            self.stages.remove(&id);
        }
        debug_assert!(
            self.stages.keys().copied().eq(self.machine.waiter_ids()),
            "a caller's channel outlives its record, or a record has no channel"
        );
    }

    fn requeued(&mut self, requeue: &Requeue) {
        self.recording.requeued(requeue);
    }

    fn learned_requeue(&mut self, view: &View<'_>, requeue: &Requeue) -> LearnedRequeue {
        let nothing = LearnedRequeue {
            rows: Vec::new(),
            lowered: None,
        };
        if self.learned.off() {
            return nothing;
        }
        if let RequeueReason::OutOfMemory { raised, .. } = requeue.reason {
            let bounds = self.bounds_of(view.sched, requeue.operation, &requeue.worker);
            let rows = self.learned.requeued(requeue.lease, raised, &bounds);
            let lowered = Some(self.learned.take_lowered());
            return LearnedRequeue { rows, lowered };
        }
        self.learned.forget(requeue.lease);
        nothing
    }

    fn summary_key(&self, key: u64) -> Option<Key> {
        self.learned.summary_key(key)
    }

    fn estimates_refused(&mut self, refused: &[Row]) {
        self.learned.unwritten(refused);
    }

    fn size_keys(&self) -> bool {
        self.learned.keys()
    }

    /// How the operation of `effect` ended, if the effect ends one: the row the store
    /// takes in the transaction that finishes it. An answer's is what its callers are
    /// told ([`finished_of`], from the report's detail if one is kept).
    fn ending(&self, effect: &Effect) -> Option<Finished> {
        match effect {
            Effect::Answer(answer) => Some(finished_of(answer, self.parked.get(&answer.lease))),
            Effect::Refuse(refusal) => Some(refused_as(refusal)),
            Effect::Start(_) | Effect::Commit(_) | Effect::Waiting(_) => None,
        }
    }

    fn forget_parcel(&mut self, lease: LeaseId) {
        self.parked.remove(&lease);
    }

    fn retain_parcels(&mut self, waits: &dyn Fn(LeaseId) -> bool) {
        self.parked.retain(|lease, _| waits(*lease));
    }

    /// Takes `op`, which the state store holds, as a caller of its own that no client
    /// waits on, and notes the lease it holds as sent.
    fn adopt(&mut self, op: &mut StoredOperation, now: SystemTime) -> WaiterId {
        let key = op.action.clone();
        let waiter = self.machine.submit(Waiter {
            key: key.clone(),
            kind: op.request.kind,
            do_not_cache: op.request.do_not_cache,
            queued: now,
            platform: Platform::of(&op.request.needs),
            // A row from a writer that did not record the subject (an older build)
            // leaves the `action usage:` line naming the empty shape.
            usage: op
                .subject
                .take()
                .unwrap_or_else(|| Subject::of(kbf_front::Described::default()).0),
        });
        self.stages.insert(waiter, watch::channel(Stage::Queued).0);
        if let Some(grant) = &op.lease {
            let stamp = Stamp {
                queued: now,
                started: now,
            };
            let sent = Sent {
                operation: op.operation,
                action: key.action,
                stamp,
                booked: None,
            };
            self.machine.start(grant.lease, sent);
        }
        waiter
    }

    /// The callers among `callers` still waiting: not answered, and with a stream open.
    fn waiting_among(&self, callers: &[WaiterId]) -> Vec<WaiterId> {
        let waiting = |caller: &WaiterId| {
            let stage = self.stages.get(caller);
            stage.is_some_and(|stage| {
                !stage.is_closed() && !matches!(*stage.borrow(), Stage::Done(_))
            })
        };
        callers.iter().copied().filter(waiting).collect()
    }

    fn show_callers(&mut self, callers: &[WaiterId], stage: &Stage) {
        for channel in callers.iter().filter_map(|caller| self.stages.get(caller)) {
            show(channel, stage.clone());
        }
    }

    fn keep_finished(&mut self, operation: OperationId, callers: Vec<WaiterId>) {
        self.machine.keep_finished(operation, callers);
    }

    fn followed(&mut self, operation: OperationId, finished: &Finished) {
        self.recording.followed(operation, finished);
    }

    fn watched(&mut self) -> Vec<OperationId> {
        self.watched
            .retain(|_, channel| channel.receiver_count() > 0);
        self.watched
            .iter()
            .filter(|(_, channel)| !matches!(*channel.borrow(), Stage::Done(_)))
            .map(|(&operation, _)| operation)
            .collect()
    }

    fn show_watched(&mut self, operation: OperationId, stage: &Stage) {
        // The engine asks only about operations `watched` just listed.
        let channel = self.watched.get(&operation);
        channel
            .into_iter()
            .for_each(|channel| show(channel, stage.clone()));
    }
}
