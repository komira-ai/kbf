//! [`KbfAdapter`]: what the build farm supplies to `js-server-core`'s [`Core`](js_server_core::Core).
//!
//! The core decides when each effect of the scheduler is carried out; the adapter carries it
//! out in the farm's terms. It is the farm's [`Side`](crate::farm) rebuilt on the core's
//! hooks:
//!
//! - a lease is offered to its node and started on the node's newest stream
//!   ([`Adapter::offer`], [`Adapter::start`], [`Adapter::cancel`]), in the `kbf.worker.v1`
//!   messages the farm has always sent;
//! - the callers of an operation are watch channels of [`kbf_front::Stage`]
//!   ([`Adapter::show`], [`Adapter::refuse`], [`Adapter::settle`]); an answer is delivered
//!   after the core's lock is released: the action-cache entry first, then the stages, then
//!   the acknowledgement the result was owed;
//! - how an operation ended is the `done/` row's value ([`kbf_front::Finished::to_row`]) and
//!   its `doneof/` row ([`Adapter::ending`]), the bytes the farm's own store layer writes;
//! - an operation the store holds, taken up at a start or by an intake poll, becomes a caller
//!   of its own that no client waits on ([`Adapter::adopted`]);
//! - the invocation record ([`crate::farm_js::record`]) and the node registry of the farm
//!   machine are kept beside.
//!
//! What a host does around the core (a `Hello`, a submit) is in [`crate::farm_js::host`].

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

use js_server_core::{Adapter, Delivery, Report, Stage as CoreStage, Stream, Taken, View};
use js_store::{Ending, RowCodec as _, Value, keys};
use kbf_front::{Backend, Finished, Recorder, Stage};
use kbf_proto::worker::{
    self, Cancel, LeaseOffer, ResultAck, ResultAckReason, ServerMessage, Start, server_message,
};
use kbf_sched::fence::START_VALIDITY;
use kbf_sched::{NodePhase, Request, Requeue, Scheduler};
use kbf_state::Store;
use kbf_types::{
    ActionKey, Answer, Effect, FarmTime, LeaseGrant, LeaseId, OperationId, Refusal, StartLease,
    WaiterId, WorkerId,
};
use tokio::sync::watch;
use tonic::Code;

use super::ablation::{Ablation, Booking};
use super::codec::KbfRowCodec;
use super::facts::WorkerFacts;
use super::learning::Learning;
use super::report_hooks::ReportHooks;
use crate::action_usage::Subject;
use crate::answer::{Detail, Pending, failed, finished_of, refused_as};
use crate::farm::NodeLoads;
use crate::farm::durable::{named, subject};
use crate::farm::failed_actions::{Class, FailedActions, class_of};
use crate::fleet::quarantine_change;
use crate::machine::{Booked, FarmMachine, Platform, Sent, Waiter, stored_name};
use crate::run_record::Recording;
use crate::stamp::Stamp;

/// Where the farm sends a worker's messages: the outbound half of its stream.
pub type Outbound = tokio::sync::mpsc::UnboundedSender<Result<ServerMessage, tonic::Status>>;

/// The wall clock, as the farm reads it: a closure, so a test moves it by hand.
pub type Wall = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// [`START_VALIDITY`] in milliseconds, as `Start.valid_for_ms` carries it.
const START_VALIDITY_MS: u64 = START_VALIDITY.as_millis() as u64;

/// The connection of a worker's newest stream.
#[derive(Debug)]
pub(super) struct Attached {
    pub(super) stream: Stream,
    pub(super) outbound: Outbound,
}

/// The farm's side of the core: the callers and their channels, the worker streams, the
/// invocation record and the counters operators read.
pub struct KbfAdapter {
    pub(super) cache: Backend,
    /// The state store the core writes, read here for the one thing a hook is not told: the
    /// request of an operation taken up from the store ([`Adapter::adopted`]).
    pub(super) store: Arc<dyn Store>,
    pub(super) wall: Wall,
    /// The callers' records, the finished operations whose callers are kept, the leases
    /// whose `Start` was sent, and the node registry. The callers are numbered by the core
    /// ([`FarmMachine::insert`]).
    pub(super) machine: FarmMachine,
    /// The channel of each caller the machine keeps, forgotten with its record.
    pub(super) stages: BTreeMap<WaiterId, watch::Sender<Stage>>,
    /// The newest stream of each worker the core has numbered one for.
    pub(super) nodes: BTreeMap<WorkerId, Attached>,
    /// The detail of a report whose answer is not settled yet, by lease: the answer's
    /// `done/` row is made from it when the store is written, and the answer is settled with
    /// it once the store has taken the rows.
    pub(super) parked: BTreeMap<LeaseId, Detail>,
    /// The channel of each operation clients ask for by name that the scheduler does not
    /// hold, shown what the store says of it at each intake poll while a ticket holds it.
    pub(super) watched: BTreeMap<OperationId, watch::Sender<Stage>>,
    /// Where the invocation record's lease and finish events go, and the operations it is
    /// told of.
    pub(super) recording: Recording,
    pub(super) pack_reads: BTreeMap<WorkerId, worker::PackReadCounts>,
    pub(super) pack_writes: BTreeMap<WorkerId, worker::PackWriteCounts>,
    pub(super) input_caches: BTreeMap<WorkerId, worker::InputCacheCounts>,
    /// What each registered worker's newest stream said of itself ([`WorkerFacts`]).
    pub(super) facts: BTreeMap<WorkerId, WorkerFacts>,
    /// What each node's load readings have come to, under the accept rule the farm's own
    /// driver uses ([`crate::farm::NodeLoads`]).
    pub(super) node_loads: NodeLoads,
    /// The operations this adapter settled or refused as failed, per class
    /// (`kbf_execute_failed_actions_total`).
    failed: FailedActions,
    /// The unreadable `done/` rows looked at, each look counted.
    unreadable_results: u64,
    /// What the hooks around a report keep between them ([`super::report_hooks`]).
    pub(super) hooks: ReportHooks,
    /// What the estimator is shown ([`super::learning`]).
    pub(super) learning: Learning,
    /// The counts that tell the ablation's rows apart ([`super::ablation`]).
    ablation: Ablation,
}

/// The `LeaseId` of the wire.
pub(super) fn wire_lease(lease: LeaseId) -> worker::LeaseId {
    worker::LeaseId {
        term: lease.term,
        seq: lease.seq,
    }
}

/// What the callers of an operation are told when its `done/` row cannot be read: UNAVAILABLE,
/// which clients retry (Buck2 retries it, as it does the refusal of an Execute call while a
/// frontend drains). The bytes are in the store and will not change, so the retry is an
/// Execute call, which is answered from the action cache or runs the action again.
pub(super) fn unreadable_result() -> Finished {
    failed(
        Code::Unavailable,
        "the stored result of this operation cannot be read",
    )
}

/// The result a `done/` row holds, or [`unreadable_result`] for a row that cannot be read.
/// Silent: [`KbfAdapter::decoded`] is the one that logs and counts.
pub(super) fn ended_as(row: &[u8]) -> Finished {
    Finished::from_row(row).unwrap_or_else(|_| unreadable_result())
}

/// Shows `stage` on `channel`, waking its watchers only if it changed.
pub(super) fn show_on(channel: &watch::Sender<Stage>, stage: &Stage) {
    channel.send_if_modified(|now| {
        let changed = *now != *stage;
        if changed {
            now.clone_from(stage);
        }
        changed
    });
}

impl KbfAdapter {
    /// An adapter for a core of scheduler term `term` over `store`, answering through
    /// `cache` and reading the wall clock from `wall`.
    #[must_use]
    pub fn new(term: u64, cache: Backend, store: Arc<dyn Store>, wall: Wall) -> Self {
        Self {
            cache,
            store,
            wall,
            machine: FarmMachine::new(term),
            stages: BTreeMap::new(),
            nodes: BTreeMap::new(),
            parked: BTreeMap::new(),
            watched: BTreeMap::new(),
            recording: Recording::new(),
            pack_reads: BTreeMap::new(),
            pack_writes: BTreeMap::new(),
            input_caches: BTreeMap::new(),
            facts: BTreeMap::new(),
            node_loads: NodeLoads::new(),
            failed: FailedActions::default(),
            unreadable_results: 0,
            hooks: ReportHooks::default(),
            learning: Learning::off(),
            ablation: Ablation::default(),
        }
    }

    /// This adapter, telling `recorder` of each Execute call's answer, each lease granted,
    /// started and given up, and each operation's end.
    #[must_use]
    pub fn recording(mut self, recorder: Recorder) -> Self {
        self.recording.tell(recorder);
        self
    }

    /// The farm machine: the callers, the leases whose `Start` was sent and the nodes.
    #[must_use]
    pub const fn machine(&self) -> &FarmMachine {
        &self.machine
    }

    /// The operations this adapter finished as failed, per class: those it settled by what
    /// their callers are told, and those it refused.
    pub(crate) const fn failed_actions(&self) -> FailedActions {
        self.failed
    }

    /// The counts of the ablation's rows ([`Ablation`]).
    pub(crate) const fn ablation(&self) -> &Ablation {
        &self.ablation
    }

    /// The first waiter of `operation`: its key and lease kind are the operation's.
    pub(super) fn first_waiter(
        &self,
        sched: &Scheduler,
        operation: OperationId,
    ) -> Option<(WaiterId, &Waiter)> {
        let first = sched.waiters(operation).and_then(<[_]>::first).copied();
        first.and_then(|first| self.machine.waiter(first).map(|waiter| (first, waiter)))
    }

    /// Sends `worker` the message `build` makes from `operation`'s first waiter. A worker
    /// that has no stream, a stream that has just ended and an operation with no waiter
    /// the machine knows drop it; the lease is reconciled later.
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
        message
            .into_iter()
            .for_each(|message| self.send(worker, message));
    }

    /// Sends `message` on `worker`'s newest stream. A worker with no stream and a stream
    /// that has just ended drop it.
    fn send(&self, worker: &WorkerId, message: server_message::Message) {
        let node = self.nodes.get(worker);
        let message = ServerMessage {
            message: Some(message),
        };
        let _sent = node.map(|node| node.outbound.send(Ok(message)).is_ok());
    }

    /// The result a `done/` row holds, or `None` for a row that cannot be read, which is
    /// logged at error and counted ([`KbfAdapter::unreadable_results`]) at each look.
    pub(super) fn readable(&mut self, row: &[u8]) -> Option<Finished> {
        Finished::from_row(row)
            .inspect_err(|error| {
                tracing::error!(%error, "a stored result cannot be read");
                self.unreadable_results += 1;
            })
            .ok()
    }

    /// The result a `done/` row holds, or [`unreadable_result`] for a row that cannot be
    /// read (logged and counted by [`KbfAdapter::readable`]), so that the callers an
    /// operation already has (followed, or watched by name) are ended with an error and
    /// none waits for a result that will not come. A client that asks for the operation by
    /// name is not given this: see `dispatch::wait`.
    pub(super) fn decoded(&mut self, row: &[u8]) -> Finished {
        self.readable(row).unwrap_or_else(unreadable_result)
    }

    /// How many unreadable `done/` rows the adapter has looked at, counting each look. The
    /// adapter has no metrics hook yet, so this is a count for tests until it is given the
    /// farm's metrics; nothing exports it.
    #[must_use]
    pub const fn unreadable_results(&self) -> u64 {
        self.unreadable_results
    }

    /// `stage` as the callers of the farm see it: one stage, or none for a stage this server
    /// does not know, which a later `js-server-core` may add. A `done/` row that cannot be
    /// read is shown as [`unreadable_result`].
    fn shown(&mut self, stage: &CoreStage) -> std::option::IntoIter<Stage> {
        let shown = match stage {
            CoreStage::Queued => Some(Stage::Queued),
            CoreStage::Waiting(why) => Some(Stage::Waiting(why.clone())),
            CoreStage::WaitingKept(why) => Some(Stage::WaitingKept(why.clone())),
            CoreStage::Executing => Some(Stage::Executing),
            CoreStage::Done(row) => Some(Stage::Done(self.decoded(row))),
            _ => None,
        };
        shown.into_iter()
    }

    /// The request of `operation`, as the store holds it in its `op/` row, and the action it
    /// names. `None` if the row is not there, cannot be read, or is not a request.
    pub(super) fn stored_request(&self, operation: OperationId) -> Option<(Request, ActionKey)> {
        let key = keys::operation(operation);
        match self.store.get(&key) {
            Ok(Some(row)) => match KbfRowCodec.read(&key, &row.value) {
                // The codec writes a request's key from an action's, so it reads back as one.
                Ok(Value::Submit(request)) => ActionKey::try_from(&request.key)
                    .ok()
                    .map(|action| (request, action)),
                _ => None,
            },
            Ok(None) => None,
            Err(error) => {
                tracing::error!(%operation, %error, "the state store cannot be read");
                None
            }
        }
    }
}

impl Adapter for KbfAdapter {
    fn clock(&mut self, now: FarmTime) {
        self.learning_clock(now);
    }

    fn before_report(
        &mut self,
        view: &View<'_>,
        _now: FarmTime,
        worker: &WorkerId,
        report: &Report,
    ) {
        self.note_report_start(view, worker, report);
    }

    fn after_report(
        &mut self,
        view: &View<'_>,
        _now: FarmTime,
        worker: &WorkerId,
        report: &Report,
        answers: &[Answer],
        taken: Taken,
    ) {
        self.note_report_end(view, worker, report, answers, taken);
    }

    fn stream_open(&self, worker: &WorkerId, stream: Stream) -> bool {
        let node = self.nodes.get(worker);
        node.is_some_and(|node| node.stream == stream && !node.outbound.is_closed())
    }

    fn name(&self, _view: &View<'_>, operation: OperationId) -> String {
        stored_name(operation)
    }

    /// Logs a quarantine raised or cleared as an attention item.
    fn phased(&mut self, worker: &WorkerId, before: &NodePhase, after: &NodePhase) {
        if let Some((raise, line)) = quarantine_change(worker.as_str(), before, after) {
            if raise {
                tracing::warn!(target: "kbf_server::attention", "{line}");
            } else {
                tracing::info!(target: "kbf_server::attention", "{line}");
            }
        }
    }

    /// Tells the worker a lease is placed on it, before the grant commits.
    fn offer(&mut self, view: &View<'_>, grant: &LeaseGrant) {
        let (operation, lease, node) = (stored_name(grant.operation), grant.lease, &grant.worker);
        // The key joins the daemon's `lease phases:` line (`lease=`) to the action, also
        // for a lease whose result is never taken (cancelled).
        let first = self.first_waiter(view.sched(), grant.operation);
        let key = first.map(|(_, w)| tracing::field::display(w.usage.key));
        // What the action asked for, parsed (`-` when it named none), so an audit can
        // find a Linux action leased to a Mac.
        let platform = first.map(|(_, w)| &w.platform);
        let os_family = platform.map_or("-", |p| p.os_family);
        let isa = platform.map_or("-", |p| p.isa.as_str());
        tracing::info!(%operation, %lease, %node, key, %os_family, %isa, "lease granted");
        self.recording.granted(grant);
        self.send_for(view.sched(), &grant.worker, grant.operation, |w| {
            server_message::Message::LeaseOffer(LeaseOffer {
                lease_id: Some(wire_lease(grant.lease)),
                kind: w.kind.name().to_owned(),
                action_digest: Some(kbf_front::digest_to_proto(&w.key.action)),
            })
        });
    }

    /// Sends the `Start` of a committed lease and marks its callers executing. The `Start`
    /// names the newest heartbeat taken on the worker's stream and the window after it in
    /// which the daemon may still act on the `Start`.
    fn start(&mut self, view: &View<'_>, start: &StartLease) {
        // Its callers are shown executing before the `Start` leaves: a stream that reads
        // the stage later never sees an operation that has a `Start` in flight as one that
        // cannot start.
        let waiters = view.sched().waiters(start.operation).unwrap_or_default();
        for stage in waiters.iter().filter_map(|id| self.stages.get(id)) {
            stage.send_replace(Stage::Executing);
        }
        let heartbeat_seq = view.link(&start.worker).map_or(0, |l| l.newest_beat);
        #[cfg(test)]
        crate::farm::BEFORE_START.with(|hook| {
            if let Some(hook) = &*hook.borrow() {
                let stages = waiters.iter().filter_map(|id| self.stages.get(id));
                hook(stages.map(|stage| stage.borrow().clone()).collect());
            }
        });
        self.send_for(view.sched(), &start.worker, start.operation, |w| {
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
        let now = (self.wall)();
        // An operation with a `Start` has its first waiter (see `send_for`), which names
        // the action.
        let sent = self
            .first_waiter(view.sched(), start.operation)
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
        self.recording.started(start);
        self.learning_started(start);
        let cost = view.sched().lease_cost(start.lease);
        self.ablation.booked(Booking::of(start.kind, cost));
    }

    /// Tells `worker` to stop the run of `lease`. A stream that has just ended drops the
    /// message; the next heartbeat that lists the lease asks again.
    fn cancel(&mut self, _view: &View<'_>, worker: &WorkerId, lease: LeaseId) {
        let cancel = server_message::Message::Cancel(Cancel {
            lease_id: Some(wire_lease(lease)),
        });
        self.send(worker, cancel);
    }

    /// Shows `stage` to the callers of `operation`. A caller that joins a waiting operation
    /// is told again; the others, who already know, see no repeated update.
    fn show(&mut self, view: &View<'_>, operation: OperationId, stage: &CoreStage) {
        let waiters = view.sched().waiters(operation).unwrap_or_default();
        for stage in self.shown(stage) {
            for channel in waiters.iter().filter_map(|id| self.stages.get(id)) {
                show_on(channel, &stage);
            }
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

    fn requeued(&mut self, requeue: &Requeue) {
        self.recording.requeued(requeue);
        self.learning_requeued(requeue);
        self.ablation.requeued(requeue.reason);
    }

    /// Works out what the callers of a finished operation get, and keeps them until the
    /// scheduler drops the operation. The report's detail, if there is one, is the one
    /// parked for the answer's lease ([`KbfAdapter::park`]), the one its `done/` row was
    /// made from.
    fn settle(&mut self, _view: &View<'_>, answer: &Answer, ack: Option<&WorkerId>) -> Delivery {
        let detail = self.parked.remove(&answer.lease);
        self.machine.forget_leases(answer.operation);
        let waiters: Vec<&Waiter> = answer
            .waiters
            .iter()
            .filter_map(|id| self.machine.waiter(*id))
            .collect();
        let stages: Vec<watch::Sender<Stage>> = answer
            .waiters
            .iter()
            .filter_map(|id| self.stages.get(id).cloned())
            .collect();
        let reason = detail.as_ref().and_then(Detail::reason);
        if let Some(class) = class_of(answer.outcome, detail.as_ref()) {
            self.failed.count(class);
        }
        let finished = finished_of(answer, detail.as_ref());
        let (stored, write) = match detail {
            Some(Detail::Ran(pending)) => {
                let Pending { result, record } = *pending;
                let cacheable =
                    result.exit_code == 0 && waiters.first().is_some_and(|w| !w.do_not_cache);
                let stored = Some(record.result);
                let write = waiters
                    .first()
                    .filter(|_| cacheable)
                    .map(|w| (w.key.action, record));
                (stored, write)
            }
            _ => (None, None),
        };
        self.recording.finished(answer, &finished, stored, reason);
        self.machine
            .keep_finished(answer.operation, answer.waiters.clone());
        // A result whose finish waited for the store is acknowledged to its sender's
        // newest stream: the store has taken the row.
        let ack = ack.and_then(|worker| self.nodes.get(worker)).map(|node| {
            let ack = ResultAck {
                lease_id: Some(wire_lease(answer.lease)),
                accepted: true,
                reason: ResultAckReason::Accepted as i32,
            };
            (node.outbound.clone(), ack)
        });
        let cache = self.cache.clone();
        Box::pin(async move {
            if let Some((action, record)) = write
                && let Err(e) = cache.commit_action_record(action, record).await
            {
                tracing::error!(%action, error = %e, "action-cache write of an accepted result failed");
            }
            for stage in stages {
                stage.send_replace(Stage::Done(finished.clone()));
            }
            if let Some((outbound, ack)) = ack {
                // A stream that has just ended drops it; the daemon sends the result again.
                let message = server_message::Message::ResultAck(ack);
                let _ = outbound.send(Ok(ServerMessage {
                    message: Some(message),
                }));
            }
        })
    }

    fn forget_dropped(&mut self, held: &dyn Fn(OperationId) -> bool) {
        for id in self.machine.forget_dropped(held) {
            self.stages.remove(&id);
        }
        debug_assert!(
            self.stages.keys().copied().eq(self.machine.waiter_ids()),
            "a caller's channel outlives its record, or a record has no channel"
        );
    }

    fn forget_parcel(&mut self, lease: LeaseId) {
        self.parked.remove(&lease);
    }

    fn retain_parcels(&mut self, waits: &dyn Fn(LeaseId) -> bool) {
        self.parked.retain(|lease, _| waits(*lease));
    }

    /// How the operation of `effect` ended, if the effect ends one: the `done/` row, which
    /// is what its callers are told, and the `doneof/` row, which names the action and the
    /// instance so a client that asks by name is authorized against them.
    fn ending(&self, effect: &Effect, retention: std::time::Duration) -> Option<Ending> {
        let (finished, waiters) = match effect {
            Effect::Answer(answer) => (
                finished_of(answer, self.parked.get(&answer.lease)),
                &answer.waiters,
            ),
            Effect::Refuse(refusal) => (refused_as(refusal), &refusal.waiters),
            Effect::Start(_) | Effect::Commit(_) | Effect::Waiting(_) => return None,
        };
        let ending = Ending::new(finished.to_row(), retention);
        // Every waiter of an operation names the same action.
        let key = waiters.first().and_then(|id| self.machine.waiter(*id));
        Some(match key {
            Some(waiter) => ending.with_parcel(named::encode(&waiter.key)),
            None => ending,
        })
    }

    /// Takes `operation`, which the state store holds, as a caller of its own that no client
    /// waits on, and notes the lease it holds as sent.
    ///
    /// The hook is told the operation, its parcel and its request. A request whose key is not
    /// an action's leaves the caller out: the operation would otherwise be started with an
    /// action it never named, and an operation with no caller the machine knows is sent no
    /// `Start` (its lease is given up after the grace and the operation runs again). The
    /// lease the operation held and its size key are not used here.
    fn adopted(
        &mut self,
        operation: OperationId,
        waiter: WaiterId,
        parcel: Option<&[u8]>,
        request: &Request,
        _lease: Option<&LeaseGrant>,
        _size_key: Option<u64>,
    ) {
        let Ok(key) = ActionKey::try_from(&request.key) else {
            tracing::error!(%operation, "an operation the store holds has no readable request: no caller");
            return;
        };
        let now = (self.wall)();
        // A row from a writer that did not record the subject (an older build) leaves the
        // `action usage:` line naming the empty shape.
        let usage = parcel
            .and_then(|parcel| subject::decode(parcel).ok())
            .unwrap_or_else(|| Subject::of(kbf_front::Described::default()).0);
        self.machine.insert(
            waiter,
            Waiter {
                key,
                kind: request.kind,
                do_not_cache: request.do_not_cache,
                queued: now,
                platform: Platform::of(&request.needs),
                usage,
            },
        );
        self.stages.insert(waiter, watch::channel(Stage::Queued).0);
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

    fn show_callers(&mut self, callers: &[WaiterId], stage: &CoreStage) {
        for stage in self.shown(stage) {
            for channel in callers.iter().filter_map(|caller| self.stages.get(caller)) {
                show_on(channel, &stage);
            }
        }
    }

    fn keep_finished(&mut self, operation: OperationId, callers: Vec<WaiterId>) {
        self.machine.keep_finished(operation, callers);
    }

    fn followed(&mut self, operation: OperationId, ended: &[u8]) {
        // The callers were shown this row just before (`show_callers`), which logged and
        // counted it if it cannot be read; the recording ends as they were told.
        self.recording.followed(operation, &ended_as(ended));
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

    fn show_watched(&mut self, operation: OperationId, stage: &CoreStage) {
        // The core asks only about operations `watched` just listed.
        for stage in self.shown(stage) {
            let channel = self.watched.get(&operation);
            channel
                .into_iter()
                .for_each(|channel| show_on(channel, &stage));
        }
    }
}
