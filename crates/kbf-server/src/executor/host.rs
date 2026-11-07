//! A virtual worker: the daemon the job service stands behind.
//!
//! The farm sees an ordinary worker, `js:<frontend>-<n>`, that registers, takes `Start`s,
//! lists what it runs in its heartbeats and reports results. Nothing of the farm knows
//! otherwise: the worker owns the receiving end of the `Outbound` channel the farm sends a
//! daemon's messages on, and calls the farm's own `register`, `resize`, `heartbeat` and
//! `report`. Its shape (the cores, the memory and the slots the flags declare) is how many
//! actions of which size the farm may have in flight on it; each becomes a job, and the job
//! service decides which host runs it. The worker holds no host, and is never retired because
//! a host went away: a job is.
//!
//! What a real session does and this one skips, and why that is safe:
//!
//! - `peers.admit` (the certificate check): nothing here is a peer; the job service's token
//!   is the credential.
//! - `MapRequest` and `MapUpdate`: the task reads through a grant, not the storage map.
//! - `note_pack_reads` and `note_pack_writes`: the task's reads and its one pack are
//!   counted by the frontend's own ingest.
//! - `Welcome.pack_commit` and `placement_load`: nothing here commits a pack through
//!   `CommitPack`. No load is reported: a job's host is the service's, and the farm places a
//!   worker with no load reading by booking alone.
//! - `Start.device_id`, `job` and `fence`: the node report names the `container` and
//!   `native-whole-machine` drivers ([`DRIVERS`](super::config::DRIVERS)) and no device, so the
//!   farm places `action` and `whole_machine` leases on it, never a device or a `job`; the
//!   worker fences every job it submits, as a daemon does that is told nothing else.
//!   `Start.kind` is read: a `whole_machine` lease is submitted as a job that needs the whole
//!   host, any other as one that needs the share the farm booked.
//!
//! # Time
//!
//! Every time here is a clock read where it is used, and the safety of the whole rests on two
//! numbers. `T` ([`Config::fence_after`]) is how long the worker goes on after the farm last
//! acknowledged a heartbeat, or the job service last answered a call while the worker may hold
//! a job, before it cancels its jobs and falls silent. `G` ([`LEASE_GRACE`]) is how long the
//! farm waits after last hearing a worker before it requeues the worker's leases. `T` plus the
//! scheduler's margin stays under `G` ([`Config::validate`]), so a silent worker's jobs were
//! cancelled before the farm starts their retry. A worker that holds no job makes no call, and
//! the service's silence is not counted against it.
//!
//! What that needs, and what it does not give:
//!
//! - A pass is a series of calls to the job service with the heartbeat and the fence inside
//!   it, so every call is bounded ([`Config::call_timeout`]), a pass starts no new task once
//!   its budget is spent ([`Config::pass_budget`]), and the heartbeat and the fence read the
//!   clock where they act. A pass is bounded by those, not by a timer of its own: the caller
//!   steps the executor again when it returns, and a caller that does not step it leaves the
//!   farm hearing nothing.
//! - Cancelling a job is a call to the job service. If that is what cannot be reached, the
//!   fence falls silent and the farm requeues the action while the job goes on to its wall
//!   limit: the work is done twice, and the second copy is not harmed by the first (each job
//!   writes only its own key). The fence bounds wasted work; it does not stop a job the
//!   service cannot be asked to cancel.
//! - A `Start` submitted at the end of its window ([`START_VALIDITY`]) is cancelled `T` after
//!   the farm last heard the worker, and the farm reruns the action
//!   [`kbf_sched::fence::START_GRACE`] after the `Start`: the window, `T` and the margin stay
//!   under it ([`Config::validate`]).
//! - A fenced worker stays silent for `G`, by when the farm has requeued every lease it held,
//!   and then registers again as a new process that names the old one its predecessor.
//!
//! [`Config::fence_after`]: super::Config::fence_after
//! [`Config::validate`]: super::Config::validate
//! [`Config::call_timeout`]: super::Config::call_timeout
//! [`Config::pass_budget`]: super::Config::pass_budget

use std::collections::{BTreeMap, VecDeque};
use std::ops::ControlFlow;

use kbf_job_client::Error;
use kbf_proto::worker::{Cancel, ResultAck, ServerMessage, Welcome, server_message};
use kbf_sched::DaemonInstance;
use kbf_sched::fence::{LEASE_GRACE, START_VALIDITY};
use kbf_types::WorkerId;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::executor::api::answered;
use crate::executor::runs::Task;
use crate::executor::{Ctx, WorkerInfo};
use crate::farm::{Outbound, Reported, StreamId};
use crate::worker::{HostFloor, PROTOCOL_VERSION, reported};

/// Why a worker's pass ends early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Retire {
    /// The farm or the job service went silent for the fence time.
    Fenced,
}

type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>;

/// One virtual worker of the farm, and the tasks it was told to run.
pub(super) struct HostWorker {
    pub(super) id: WorkerId,
    instance: String,
    generation: u32,
    stream: StreamId,
    reported: Reported,
    inbox: Inbox,
    /// The seq of the newest heartbeat sent.
    seq: u64,
    /// When each heartbeat that a `Start` may still name was sent, by seq: those sent less
    /// than [`START_VALIDITY`] ago. Seq 0 is the registration.
    beats: VecDeque<(u64, Instant)>,
    next_beat: Instant,
    /// When the newest heartbeat the farm acknowledged was sent.
    last_ack: Instant,
    /// When the job service last answered, or when the worker last held no job.
    server_ok: Instant,
    pub(super) draining: bool,
    pub(super) tasks: BTreeMap<kbf_types::LeaseId, Task>,
    /// Fenced: silent, until then.
    silent_until: Option<Instant>,
    /// The farm lease whose job was read last: the next pass's reads begin after it.
    pub(super) read_after: Option<kbf_types::LeaseId>,
}

impl Drop for HostWorker {
    fn drop(&mut self) {
        self.forget();
    }
}

impl HostWorker {
    /// Registers the `n`th virtual worker of this frontend.
    pub(super) fn start(ctx: &Ctx<'_>, n: usize, now: Instant) -> Self {
        let id = WorkerId::new(format!("js:{}-{n}", ctx.cfg.frontend));
        let (stream, instance, reported, inbox) = register(ctx, &id, 0, "");
        Self {
            id,
            instance,
            generation: 0,
            stream,
            reported,
            inbox,
            seq: 0,
            beats: VecDeque::from([(0, now)]),
            next_beat: now,
            last_ack: now,
            server_ok: now,
            draining: false,
            tasks: BTreeMap::new(),
            silent_until: None,
            read_after: None,
        }
    }

    /// Takes the farm's messages from `inbox`: for the tests that feed a worker by hand.
    #[cfg(test)]
    pub(super) fn replace_inbox(&mut self, inbox: Inbox) {
        self.inbox = inbox;
    }

    pub(super) fn info(&self) -> WorkerInfo {
        WorkerInfo {
            worker: self.id.clone(),
            runs: self.tasks.len(),
            draining: self.draining,
            silent: self.silent_until.is_some(),
        }
    }

    /// Drops what the worker holds for the farm: every task, with the ingests and reports in
    /// flight.
    pub(super) fn forget(&mut self) {
        self.tasks.values().for_each(Task::abort);
        self.tasks.clear();
    }

    /// One pass: take the farm's messages, submit what was told to start, read the jobs,
    /// settle them, and heartbeat; or, fenced, stay silent until the farm has requeued.
    pub(super) async fn step(&mut self, ctx: &Ctx<'_>, now: Instant) {
        if let Some(until) = self.silent_until {
            if now < until {
                return;
            }
            self.wake(ctx, now);
        }
        if let ControlFlow::Break(Retire::Fenced) = self.pass(ctx, now).await {
            self.fence(ctx, now).await;
        }
    }

    async fn pass(&mut self, ctx: &Ctx<'_>, now: Instant) -> ControlFlow<Retire> {
        if self.intake() {
            self.reconnect(ctx, now).await;
        }
        self.launch(ctx, now).await;
        self.poll(ctx, now).await;
        self.settle(ctx, now);
        self.beat(ctx).await
    }

    /// Takes every message the farm queued since the last pass. True if the farm ended the
    /// stream (a status in place of a message, as a handover sends) or dropped it (the worker
    /// was registered again).
    fn intake(&mut self) -> bool {
        loop {
            match self.inbox.try_recv() {
                Ok(Ok(message)) => self.on_message(message),
                Ok(Err(status)) => {
                    tracing::warn!(worker = %self.id, %status, "the farm ended the stream");
                    return true;
                }
                Err(mpsc::error::TryRecvError::Empty) => return false,
                Err(mpsc::error::TryRecvError::Disconnected) => return true,
            }
        }
    }

    fn on_message(&mut self, message: ServerMessage) {
        let farm_lease = |l: kbf_proto::worker::LeaseId| kbf_types::LeaseId::new(l.term, l.seq);
        match message.message {
            Some(server_message::Message::Start(start)) => self.accept(&start),
            Some(server_message::Message::Cancel(Cancel { lease_id })) => {
                lease_id
                    .map(farm_lease)
                    .iter()
                    .for_each(|l| self.cancel(*l));
            }
            Some(server_message::Message::ResultAck(ResultAck { lease_id, .. })) => {
                lease_id.map(farm_lease).iter().for_each(|l| self.acked(*l));
            }
            _ => {}
        }
    }

    /// The send time of the heartbeat `seq` (0 is the registration), if it is one the worker
    /// sent and still remembers: a `Start` that names another is refused, as a daemon refuses.
    pub(super) fn sent_at(&self, seq: u64) -> Option<Instant> {
        self.beats
            .iter()
            .find_map(|(sent, at)| (*sent == seq).then_some(*at))
    }

    /// The farm closed the stream, or replaced it: it gave up the leases it knew. Cancels the
    /// jobs, and registers again as a new process that names the old one its predecessor.
    async fn reconnect(&mut self, ctx: &Ctx<'_>, now: Instant) {
        tracing::warn!(worker = %self.id, "the farm's stream ended: cancelling the jobs and registering again");
        self.cancel_all(ctx).await;
        self.forget();
        self.register_again(ctx, now);
    }

    /// Registers again, under a new instance that names the old one: the farm sees the old
    /// stream as ended, so it gives up at once whatever lease it still knew of it.
    fn register_again(&mut self, ctx: &Ctx<'_>, now: Instant) {
        // Closed before the new registration, so the farm sees the old stream as ended.
        self.inbox.close();
        self.generation += 1;
        let predecessor = std::mem::take(&mut self.instance);
        let (stream, instance, reported, inbox) =
            register(ctx, &self.id, self.generation, &predecessor);
        self.stream = stream;
        self.instance = instance;
        self.reported = reported;
        self.inbox = inbox;
        self.seq = 0;
        self.beats = VecDeque::from([(0, now)]);
        self.next_beat = now;
        self.last_ack = now;
        self.server_ok = now;
        self.draining = false;
    }

    /// The fence: every job is cancelled, as far as the service can be reached, the tasks are
    /// forgotten, and the worker says nothing to the farm for [`LEASE_GRACE`], by when the farm
    /// has requeued every lease it held.
    async fn fence(&mut self, ctx: &Ctx<'_>, now: Instant) {
        tracing::warn!(worker = %self.id, "the farm or the job service has been silent for the fence time: cancelling the jobs and falling silent");
        self.cancel_all(ctx).await;
        self.forget();
        self.silent_until = Some(now + LEASE_GRACE);
    }

    /// The silence after a fence is over: the worker registers again.
    fn wake(&mut self, ctx: &Ctx<'_>, now: Instant) {
        tracing::info!(worker = %self.id, "the fence's silence is over: registering again");
        self.silent_until = None;
        self.register_again(ctx, now);
    }

    /// Notes that the job service answered, whatever it said.
    pub(super) fn heard<T>(&mut self, ctx: &Ctx<'_>, answer: &Result<T, Error>) {
        if answered(answer) {
            self.server_ok = ctx.now();
        }
    }

    /// Stops taking work: the farm is told the node is draining.
    pub(super) fn begin_drain(&mut self, ctx: &Ctx<'_>) {
        self.draining = true;
        self.reported.phase = kbf_sched::NodePhase::Draining;
        ctx.deps
            .farm
            .resize(&self.id, self.stream, self.reported.clone());
    }

    /// Heartbeats the farm when one is due, and fences when the farm, or the job service while
    /// the worker may hold a job, has been silent for the fence time. The clock is read here,
    /// not taken from the start of the pass: the calls before this took as long as they took,
    /// and the silence is counted to now.
    async fn beat(&mut self, ctx: &Ctx<'_>) -> ControlFlow<Retire> {
        let now = ctx.now();
        if !self.holds_jobs() {
            self.server_ok = now;
        }
        let older = self.last_ack.min(self.server_ok);
        if now.saturating_duration_since(older) > ctx.cfg.fence_after {
            return ControlFlow::Break(Retire::Fenced);
        }
        if now < self.next_beat {
            return ControlFlow::Continue(());
        }
        self.seq += 1;
        self.beats.push_back((self.seq, now));
        self.beats.retain(|(_, sent)| now < *sent + START_VALIDITY);
        self.next_beat = now + ctx.cfg.heartbeat;
        let running = self.listed();
        let acked = ctx
            .deps
            .farm
            .heartbeat(&self.id, self.stream, self.seq, running)
            .await;
        if acked {
            self.last_ack = now;
        }
        ControlFlow::Continue(())
    }

    /// Takes the worker down: the farm is told it takes no more work, and its jobs are
    /// cancelled.
    pub(super) async fn retire(mut self, ctx: &Ctx<'_>) {
        tracing::info!(worker = %self.id, "retiring the worker");
        self.begin_drain(ctx);
        self.cancel_all(ctx).await;
        self.forget();
    }
}

/// Registers a worker with the farm: the `Hello` a real node would send, the `Welcome` the
/// server answers it with, and the stream the farm's messages come back on.
fn register(
    ctx: &Ctx<'_>,
    id: &WorkerId,
    generation: u32,
    predecessor: &str,
) -> (StreamId, String, Reported, Inbox) {
    let instance = format!("kbf-executor/{id}/{}/{generation}", ctx.cfg.instance);
    let hello = ctx.cfg.hello(&id.to_string(), &instance, predecessor);
    let mut reported =
        reported(&hello, HostFloor::DEFAULT).expect("a validated config makes a valid Hello");
    reported.protocol = Some(PROTOCOL_VERSION);
    let welcome = ServerMessage {
        message: Some(server_message::Message::Welcome(Welcome {
            protocol_version: PROTOCOL_VERSION,
            heartbeat_interval_ms: u64::try_from(ctx.cfg.heartbeat.as_millis()).unwrap_or(u64::MAX),
            epoch: ctx.deps.farm.lease_epoch(),
            ..Welcome::default()
        })),
    };
    let (outbound, inbox): (Outbound, Inbox) = mpsc::unbounded_channel();
    let stream = ctx.deps.farm.register(
        id,
        DaemonInstance::new(instance.clone()),
        DaemonInstance::new(predecessor),
        reported.clone(),
        outbound,
        welcome,
    );
    (stream, instance, reported, inbox)
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
