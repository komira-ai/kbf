//! What a host does with the adapter around a call of the core: the calls the core runs a
//! closure of under its lock ([`Core::register`](js_server_core::Core::register),
//! [`Core::submit`](js_server_core::Core::submit), [`Core::report`](js_server_core::Core::report))
//! and the counters operators read. None of these is a hook: the core never calls them.

use std::time::UNIX_EPOCH;

use js_server_core::{Admitted, Held, Stream, View};
use kbf_front::{ExecuteCall, Stage, Ticket};
use kbf_proto::update::UpdateStatus;
use kbf_proto::worker::{self, NodeStatus, ServerMessage};
use kbf_sched::OpState;
use kbf_types::{ActionKey, LeaseId, LeaseKind, OperationId, WorkerId};
use tokio::sync::watch;

use super::adapter::{Attached, KbfAdapter, Outbound, show_on};
use crate::action_usage::Subject;
use crate::answer::Detail;
use crate::fleet::take_node_status;
use crate::machine::{Platform, Waiter, stored_name};
use crate::update_status::UpdateReport;

/// A worker's newest stream, as the farm holds it: where its messages go, and the first one.
#[derive(Debug)]
pub struct Connection {
    /// The outbound half of the stream.
    pub outbound: Outbound,
    /// The first message of the stream, `Welcome`.
    pub welcome: ServerMessage,
}

/// A caller of the farm, as its submit describes it.
#[derive(Clone, Debug)]
pub struct Caller {
    /// The instance name and action it submitted.
    pub key: ActionKey,
    /// The lease kind it asked for.
    pub kind: LeaseKind,
    /// The action must not be answered from, or written to, the action cache.
    pub do_not_cache: bool,
    /// What the `action usage:` line says the action is.
    pub usage: Subject,
    /// The OS family and ISA the action asked for.
    pub platform: Platform,
}

/// The number the core gave `stream`, which the farm's `StreamId` carries. Always `Some`; the
/// `Option` keeps the callers written as `StreamId(stream_number(stream)?)`. A caller that
/// registers a worker returns that `StreamId`: any other numbering makes every status and
/// pack count the views take for the stream be ignored.
#[must_use]
pub fn stream_number(stream: Stream) -> Option<u64> {
    Some(stream.number())
}

impl KbfAdapter {
    /// Attaches the connection of `worker`'s stream `stream`, which the core has just
    /// numbered, and sends it its first message. Run it as the `open` of
    /// [`Core::register`](js_server_core::Core::register): the `Start`s the registration
    /// leads to find the connection.
    pub fn attach(&mut self, worker: &WorkerId, stream: Stream, connection: Connection) {
        let Connection { outbound, welcome } = connection;
        // The receiver is the stream's own response, alive until the stream ends.
        let _ = outbound.send(Ok(welcome));
        self.nodes
            .insert(worker.clone(), Attached { stream, outbound });
        self.machine.register(worker);
    }

    /// Tells the machine of each node whose newest stream has closed (its response side is
    /// gone), so its registry says the node is not connected.
    pub fn note_closed_streams(&mut self) {
        for (worker, node) in &self.nodes {
            if node.outbound.is_closed() {
                self.machine.disconnect(worker);
            }
        }
    }

    /// Records the caller the core numbered for a submit (`admitted`), whose callers are
    /// shown `stage`. Run it as the `enrol` of
    /// [`Core::submit`](js_server_core::Core::submit): the operation's first `Start` finds
    /// the caller. `call` is the Execute call that submitted it, if the invocation recorder
    /// is told of it.
    ///
    /// A caller that follows an operation another frontend's daemon runs sees it executing
    /// at once; so does one that joins an operation that holds a lease here
    /// ([`KbfAdapter::mark_joined`]).
    pub fn enrol(
        &mut self,
        admitted: &Admitted,
        caller: Caller,
        call: Option<ExecuteCall>,
        stage: watch::Sender<Stage>,
    ) {
        let Caller {
            key,
            kind,
            do_not_cache,
            usage,
            platform,
        } = caller;
        let queued = (self.wall)();
        self.note_action(&key, &platform, &usage);
        let waiter = Waiter {
            key,
            kind,
            do_not_cache,
            queued,
            usage,
            platform,
        };
        self.machine.insert(admitted.waiter, waiter);
        if let Some(call) = call {
            let name = stored_name(admitted.operation);
            self.recording
                .submit(call, &name, admitted.operation, admitted.joined);
        }
        if matches!(admitted.held, Held::Elsewhere(_)) {
            stage.send_replace(Stage::Executing);
        }
        self.stages.insert(admitted.waiter, stage);
    }

    /// Shows the caller `admitted` executing if it joined an operation whose `Start` was
    /// sent, once the scheduler has heard of the request. The operation's state decides, not
    /// the leases this adapter sent: a lease taken up from the state store was never
    /// started here.
    pub fn mark_joined(&mut self, view: &View<'_>, admitted: &Admitted) {
        let started = matches!(
            view.sched().state(admitted.operation),
            Some(
                OpState::Leased {
                    committed: true,
                    ..
                } | OpState::Running { .. }
            )
        );
        let stage = self.stages.get(&admitted.waiter);
        if let Some(stage) = stage.filter(|_| admitted.joined && started) {
            stage.send_if_modified(|now| {
                let queued = *now == Stage::Queued;
                if queued {
                    *now = Stage::Executing;
                }
                queued
            });
        }
    }

    /// A ticket-side view of `operation` through the channel of the first caller the
    /// scheduler holds for it, if it holds the operation.
    pub(super) fn held_ticket(&self, view: &View<'_>, operation: OperationId) -> Option<Ticket> {
        let (first, waiter) = self.first_waiter(view.sched(), operation)?;
        self.stages.get(&first).map(|stage| Ticket {
            name: stored_name(operation),
            instance: waiter.key.instance.clone(),
            action: waiter.key.action,
            stage: stage.subscribe(),
        })
    }

    /// Keeps `detail`, the report of `lease`, before the report is fed: the answer it
    /// decides is written with the result it carries, in the transaction that finishes the
    /// operation, and settled with the same detail. Run it as the `park` of
    /// [`Core::report`](js_server_core::Core::report), which forgets what no answer used
    /// once the answers are settled.
    pub fn park(&mut self, lease: LeaseId, detail: Detail) {
        self.parked.insert(lease, detail);
    }

    /// A channel of its own on `operation`, which the scheduler does not hold, shown what the
    /// state store says of it now (`stage`) and at each intake poll while a ticket holds it.
    pub fn watch_operation(
        &mut self,
        operation: OperationId,
        stage: &Stage,
    ) -> watch::Receiver<Stage> {
        let watched = &mut self.watched;
        let channel = watched
            .entry(operation)
            .or_insert_with(|| watch::channel(stage.clone()).0);
        show_on(channel, stage);
        channel.subscribe()
    }

    /// Whether `stream` is `worker`'s newest.
    #[must_use]
    pub fn is_current(&self, worker: &WorkerId, stream: Stream) -> bool {
        self.nodes
            .get(worker)
            .is_some_and(|node| node.stream == stream)
    }

    /// `worker`'s newest stream, if it is the one the farm calls `number`: the farm numbers a
    /// stream by the number the core gave it ([`stream_number`]).
    #[must_use]
    pub fn current_stream(&self, worker: &WorkerId, number: u64) -> Option<Stream> {
        let node = self.nodes.get(worker)?;
        (stream_number(node.stream) == Some(number)).then_some(node.stream)
    }

    /// A `NodeStatus` of `worker` on `stream`: kept as the node's newest, unless `stream`
    /// was replaced (a newer stream sends its own after its `Welcome`). Returns the
    /// attention items it raises or clears against the node's previous status, from any
    /// stream, `true` for each raised; each is logged once, as the farm's own driver logs
    /// it ([`take_node_status`]). The time it was received is the wall clock's now.
    pub fn report_node_status(
        &mut self,
        worker: &WorkerId,
        stream: Stream,
        status: NodeStatus,
    ) -> Vec<(bool, String)> {
        if !self.is_current(worker, stream) {
            return Vec::new();
        }
        let received = (self.wall)().duration_since(UNIX_EPOCH).map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        });
        take_node_status(&mut self.machine, worker, status, received)
    }

    /// An `UpdateStatus` of `worker` on `stream`: kept as the node's newest, unless `stream`
    /// was replaced. Returns whether it was kept.
    pub fn report_update_status(
        &mut self,
        worker: &WorkerId,
        stream: Stream,
        status: UpdateStatus,
    ) -> bool {
        let current = self.is_current(worker, stream);
        if current {
            self.machine
                .report_update(worker, UpdateReport::new(status));
        }
        current
    }

    /// The pack-read counts of a heartbeat of `worker` on `stream`: kept as the node's
    /// newest when the heartbeat carries them, and, when it carries none, the node's are
    /// forgotten (a daemon that was restarted without `--pack-read` reads no packs, and
    /// must not show the counts of the process before). Ignored if `stream` was replaced.
    pub fn note_pack_reads(
        &mut self,
        worker: &WorkerId,
        stream: Stream,
        counts: Option<worker::PackReadCounts>,
    ) {
        if !self.is_current(worker, stream) {
            return;
        }
        match counts {
            Some(counts) => self.pack_reads.insert(worker.clone(), counts),
            None => self.pack_reads.remove(worker),
        };
    }

    /// The newest pack-read counts of every node that sent any, in node-id order.
    #[must_use]
    pub fn pack_reads(&self) -> Vec<(WorkerId, worker::PackReadCounts)> {
        let reads = self.pack_reads.iter();
        reads
            .map(|(worker, counts)| (worker.clone(), *counts))
            .collect()
    }

    /// As [`KbfAdapter::note_pack_reads`], for the pack-write counts.
    pub fn note_pack_writes(
        &mut self,
        worker: &WorkerId,
        stream: Stream,
        counts: Option<worker::PackWriteCounts>,
    ) {
        if !self.is_current(worker, stream) {
            return;
        }
        match counts {
            Some(counts) => self.pack_writes.insert(worker.clone(), counts),
            None => self.pack_writes.remove(worker),
        };
    }

    /// The newest pack-write counts of every node that sent any, in node-id order.
    #[must_use]
    pub fn pack_writes(&self) -> Vec<(WorkerId, worker::PackWriteCounts)> {
        let writes = self.pack_writes.iter();
        writes
            .map(|(worker, counts)| (worker.clone(), *counts))
            .collect()
    }

    /// As [`KbfAdapter::note_pack_reads`], for the input cache counts.
    pub fn note_input_cache(
        &mut self,
        worker: &WorkerId,
        stream: Stream,
        counts: Option<worker::InputCacheCounts>,
    ) {
        if !self.is_current(worker, stream) {
            return;
        }
        match counts {
            Some(counts) => self.input_caches.insert(worker.clone(), counts),
            None => self.input_caches.remove(worker),
        };
    }

    /// `worker`'s newest input cache counts, as `GET /v1/nodes/{id}` shows them.
    #[must_use]
    pub fn input_cache_of(&self, worker: &WorkerId) -> Option<crate::fleet::InputCacheView> {
        self.input_caches.get(worker).map(Into::into)
    }

    /// The newest input cache counts of every node that sent any, in node-id order.
    #[must_use]
    pub fn input_caches(&self) -> Vec<(WorkerId, worker::InputCacheCounts)> {
        let kept = self.input_caches.iter();
        kept.map(|(worker, counts)| (worker.clone(), *counts))
            .collect()
    }
}
