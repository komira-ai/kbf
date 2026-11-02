//! A virtual worker: the daemon a held lease of a host stands in for.
//!
//! The farm sees an ordinary worker, `js:<host>`, that registers, takes `Start`s, lists what
//! it runs in its heartbeats and reports results. Nothing of the farm knows otherwise: the
//! worker owns the receiving end of the `Outbound` channel the farm sends a daemon's messages
//! on, and calls the farm's own `register`, `resize`, `heartbeat_with_load` and `report`.
//!
//! What a real session does and this one skips, and why that is safe:
//!
//! - `peers.admit` (the certificate check): nothing here is a peer; the lease server's
//!   token is the credential, and the host is named by the lease the executor holds.
//! - `MapRequest` and `MapUpdate`: the task reads through a grant, not the storage map.
//! - `note_pack_reads` and `note_pack_writes`: the task's reads and its one pack are
//!   counted by the frontend's own ingest.
//! - `Welcome.pack_commit` and `placement_load`: nothing here commits a pack through
//!   `CommitPack`. The host's load is not the worker's to measure: it is the agent's, listed
//!   by the lease server with the host and handed to the farm with a heartbeat ([`super::load`]);
//!   a host with none listed, or one gone stale, sends none, and the farm places it by
//!   booking alone.
//! - `Start.kind`, `device_id`, `job` and `fence`: the node report names `container` as its
//!   only driver, so the farm places only actions on it, with no device; the worker fences
//!   every run it starts, as a daemon does that is told nothing else.
//!
//! # Time
//!
//! Every time here is a clock read where it is used, and the safety of the whole rests on two
//! numbers. `T` ([`Config::fence_after`]) is how long the worker goes on after the farm
//! last acknowledged a heartbeat, or the lease server last answered, before it asks its runs
//! to stop and falls silent. `G` ([`kbf_sched::fence::LEASE_GRACE`]) is how long the farm waits
//! after last hearing a worker before it requeues the worker's leases. `T` plus the
//! scheduler's margin stays under `G` ([`Config::validate`]), so a silent worker's runs
//! were asked to stop before the farm starts their retry.
//!
//! What that needs, and what it does not give:
//!
//! - A pass is a series of calls to the lease server with the heartbeat and the fence
//!   inside it, so every call is bounded ([`Config::call_timeout`]), a pass starts no new
//!   task once its budget is spent ([`Config::pass_budget`]), and the heartbeat and the fence
//!   read the clock where they act. A pass is bounded by those, not by a timer of its own: the
//!   caller steps the executor again when it returns, and a caller that does not step it
//!   leaves the farm hearing nothing.
//! - Asking a run to stop is a call to the lease server. If that is what cannot be
//!   reached, the fence falls silent and the farm requeues the action while the run goes on
//!   to its wall time: the work is done twice, and the second copy is not harmed by the first
//!   (each run writes only its own key). The fence bounds wasted work; it does not stop a run
//!   the lease server cannot be asked to stop.
//! - A `Start` posted at the end of its window ([`kbf_sched::fence::START_VALIDITY`]) is
//!   stopped `T` after the farm last heard the worker, and the farm reruns the action
//!   [`kbf_sched::fence::START_GRACE`] after the `Start`: the window, `T` and the margin stay
//!   under it ([`Config::validate`]).

use std::collections::{BTreeMap, VecDeque};
use std::ops::ControlFlow;

use kbf_lease_client::LeaseId;
use kbf_proto::worker::{Cancel, ResultAck, ServerMessage, Welcome, server_message};
use kbf_sched::DaemonInstance;
use kbf_sched::fence::START_VALIDITY;
use kbf_types::WorkerId;
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::executor::load::Seen;
use crate::executor::runs::Task;
use crate::executor::{Ctx, HostInfo, View, lease_gone, release_lease};
use crate::farm::{Outbound, Reported, StreamId};
use crate::worker::{HostFloor, PROTOCOL_VERSION, reported};

/// Why a worker is taken down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Retire {
    /// The lease server no longer has the lease.
    Gone,
    /// The host is lost.
    Lost,
    /// The farm or the lease server went silent for the fence time.
    Fenced,
    /// A run may be queued that the worker cannot find.
    Ambiguous,
    /// The host stopped taking work and is idle: a rotation, or a taint.
    Drained,
    /// The host's scratch quota is out of order and it is idle: its lease is kept, unused,
    /// until the given time, so the lease server cannot hand the same host out again.
    Benched(Instant),
}

/// Why a worker stopped taking work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Drain {
    /// The lease nears the end of its wall time.
    Rotation,
    /// The host reports a taint.
    Tainted,
    /// The host refused a start because its scratch quota is out of order. Not a taint: it
    /// recovers by itself, so the lease is kept idle until the time given, and not handed
    /// back to be leased again at once.
    Quota(Instant),
}

type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>;

/// One held lease, and the worker that stands for its host.
pub(super) struct HostWorker {
    pub(super) lease: LeaseId,
    pub(super) host: String,
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
    /// When the lease server last answered.
    server_ok: Instant,
    renew_at: Instant,
    /// When the lease expires unless renewed, as the lease server last said.
    pub(super) expires_at: Instant,
    pub(super) ends_at: Instant,
    pub(super) draining: Option<Drain>,
    pub(super) tasks: BTreeMap<kbf_types::LeaseId, Task>,
    /// The newest load the lease server listed for the host, until the farm has it.
    load: Option<Seen>,
}

impl Drop for HostWorker {
    fn drop(&mut self) {
        self.forget();
    }
}

impl HostWorker {
    /// Registers the worker of the host `host` under `lease`, which ends at `ends_at`.
    pub(super) fn start(
        ctx: &Ctx<'_>,
        lease: LeaseId,
        host: String,
        (expires_at, ends_at): (Instant, Instant),
        now: Instant,
    ) -> Self {
        let id = WorkerId::new(format!("js:{host}"));
        let (stream, instance, reported, inbox) = register(ctx, &id, lease, 0, "");
        Self {
            lease,
            host,
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
            renew_at: renew_after(ctx, expires_at, now),
            expires_at,
            ends_at,
            draining: None,
            tasks: BTreeMap::new(),
            load: None,
        }
    }

    /// Takes the farm's messages from `inbox`: for the tests that feed a worker by hand.
    #[cfg(test)]
    pub(super) fn replace_inbox(&mut self, inbox: Inbox) {
        self.inbox = inbox;
    }

    pub(super) fn info(&self) -> HostInfo {
        HostInfo {
            worker: self.id.clone(),
            lease: self.lease,
            host: self.host.clone(),
            runs: self.tasks.len(),
            draining: self.draining.is_some(),
        }
    }

    /// Drops what the worker holds for the farm: every task, with the ingests and reports in
    /// flight.
    pub(super) fn forget(&mut self) {
        self.tasks.values().for_each(Task::abort);
        self.tasks.clear();
    }

    /// One pass: take the farm's messages, keep the lease, act on what the lease server says of
    /// the host, start what was told to start, read the results, settle them, and heartbeat.
    pub(super) async fn step(
        &mut self,
        ctx: &Ctx<'_>,
        view: &View,
        now: Instant,
    ) -> Option<Retire> {
        let flow = self.pass(ctx, view, now).await;
        flow.break_value()
    }

    async fn pass(&mut self, ctx: &Ctx<'_>, view: &View, now: Instant) -> ControlFlow<Retire> {
        if self.intake() {
            self.reconnect(ctx, now).await;
        }
        self.renew(ctx, now, false).await?;
        self.watch(ctx, view, now)?;
        self.launch(ctx, now).await?;
        self.poll(ctx, now).await?;
        self.settle(ctx, now);
        self.rotate(ctx, now)?;
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

    /// The farm closed the stream, or replaced it: it gave up the leases it knew. Stops the
    /// runs, and registers again as a new process that names the old one its predecessor.
    async fn reconnect(&mut self, ctx: &Ctx<'_>, now: Instant) {
        tracing::warn!(worker = %self.id, "the farm's stream ended: stopping the runs and registering again");
        self.stop_all(ctx).await;
        self.forget();
        // Closed before the new registration, so the farm sees the old stream as ended: the
        // runs it knew are stopped, which is what lets it give their leases up at once.
        self.inbox.close();
        self.generation += 1;
        let predecessor = std::mem::take(&mut self.instance);
        let (stream, instance, reported, inbox) =
            register(ctx, &self.id, self.lease, self.generation, &predecessor);
        self.stream = stream;
        self.instance = instance;
        self.reported = reported;
        self.inbox = inbox;
        self.seq = 0;
        self.beats = VecDeque::from([(0, now)]);
        self.next_beat = now;
        self.last_ack = now;
        self.draining = None;
    }

    /// Renews the lease when a third of its ttl (or [`Config::renew_every`]) has passed, or at once if `due`.
    pub(super) async fn renew(
        &mut self,
        ctx: &Ctx<'_>,
        now: Instant,
        due: bool,
    ) -> ControlFlow<Retire> {
        if now < self.renew_at && !due {
            return ControlFlow::Continue(());
        }
        let renewed = ctx.api().renew(self.lease, Some(ctx.cfg.lease_ttl)).await;
        self.heard(now, &renewed);
        match renewed {
            Ok(renewed) => {
                // A third of what the server granted, which is at most its own maximum,
                // not of what was asked.
                self.expires_at = ctx.clock.local(renewed.expires_at_ms, now);
                self.renew_at = renew_after(ctx, self.expires_at, now);
            }
            Err(e) if lease_gone(&e) => return ControlFlow::Break(Retire::Gone),
            Err(e) => tracing::warn!(worker = %self.id, error = %e, "renewing the lease failed"),
        }
        ControlFlow::Continue(())
    }

    /// Notes that the lease server answered, whatever it said.
    pub(super) fn heard<T>(&mut self, now: Instant, answer: &Result<T, kbf_lease_client::Error>) {
        if answer.as_ref().err().is_none_or(|e| e.api().is_some()) {
            self.server_ok = now;
        }
    }

    /// What the lease server's host list says of this host: lost is fenced, tainted drains.
    fn watch(&mut self, ctx: &Ctx<'_>, view: &View, now: Instant) -> ControlFlow<Retire> {
        let host = view.host(&self.host);
        if let Some(host) = host {
            self.load = Seen::after_poll(self.load.take(), host.load.as_ref(), now);
        }
        let lost = host.is_some_and(kbf_lease_client::Host::is_lost);
        let tainted = host.is_some_and(kbf_lease_client::Host::is_tainted);
        if tainted {
            self.begin_drain(ctx, Drain::Tainted);
        }
        match lost {
            true => ControlFlow::Break(Retire::Lost),
            false => ControlFlow::Continue(()),
        }
    }

    /// Stops taking work: the farm is told the node is draining, and the worker is released
    /// once it holds nothing.
    pub(super) fn begin_drain(&mut self, ctx: &Ctx<'_>, why: Drain) {
        if self.draining.is_none() {
            self.draining = Some(why);
            self.reported.phase = kbf_sched::NodePhase::Draining;
            ctx.deps
                .farm
                .resize(&self.id, self.stream, self.reported.clone());
        }
    }

    /// The host refused a start for a fault of its own that it recovers from by itself (a
    /// scratch quota out of order, a files root that is gone): it takes no more work, and is
    /// held out for [`Config::held_out_for`] from `now`, so the lease server cannot hand it
    /// out again at once.
    pub(super) fn hold_out(&mut self, ctx: &Ctx<'_>, now: Instant) {
        self.begin_drain(ctx, Drain::Quota(now + ctx.cfg.held_out_for()));
    }

    /// Starts the drain when the lease nears the end of its wall time, and ends the worker
    /// once it is draining and idle.
    fn rotate(&mut self, ctx: &Ctx<'_>, now: Instant) -> ControlFlow<Retire> {
        let rotate_at = self.ends_at.checked_sub(ctx.cfg.drain_before_end);
        if rotate_at.is_none_or(|at| now >= at) {
            self.begin_drain(ctx, Drain::Rotation);
        }
        match (self.draining, self.tasks.is_empty()) {
            (Some(Drain::Quota(until)), true) => ControlFlow::Break(Retire::Benched(until)),
            (Some(_), true) => ControlFlow::Break(Retire::Drained),
            _ => ControlFlow::Continue(()),
        }
    }

    /// Heartbeats the farm when one is due, and fences when the farm or the lease server
    /// has been silent for the fence time. The clock is read here, not taken from the start
    /// of the pass: the calls before this took as long as they took, and the silence is
    /// counted to now.
    async fn beat(&mut self, ctx: &Ctx<'_>) -> ControlFlow<Retire> {
        let now = ctx.clock.now();
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
        let load = self
            .load
            .as_mut()
            .and_then(|seen| seen.take(now, ctx.cfg.host_check));
        // Taken by the accept rule like a daemon's reading: the scheduler is handed what it
        // accepts, with the heartbeat.
        let fed = ctx
            .deps
            .farm
            .note_node_load(&self.id, self.stream, load.as_ref());
        let acked = ctx
            .deps
            .farm
            .heartbeat_with_load(&self.id, self.stream, self.seq, running, fed)
            .await;
        if acked {
            self.last_ack = now;
        }
        ControlFlow::Continue(())
    }

    /// Takes the worker down.
    pub(super) async fn retire(mut self, ctx: &Ctx<'_>, why: Retire) {
        tracing::info!(worker = %self.id, lease = %self.lease, ?why, "retiring the worker");
        match why {
            Retire::Gone => {
                self.begin_drain(ctx, Drain::Rotation);
                self.report_lost(ctx).await;
            }
            Retire::Ambiguous => {
                self.begin_drain(ctx, Drain::Rotation);
                self.stop_all(ctx).await;
            }
            Retire::Lost | Retire::Fenced => self.stop_all(ctx).await,
            Retire::Drained | Retire::Benched(_) => {}
        }
        // A benched host's lease is kept, and the pool releases it when the hold ends.
        if !matches!(why, Retire::Benched(_)) {
            release_lease(ctx, self.lease).await;
        }
        self.forget();
    }
}

/// When the lease is renewed: a third of the time from `now` to `expires_at`, and at the
/// latest [`Config::renew_every`] from `now`.
fn renew_after(ctx: &Ctx<'_>, expires_at: Instant, now: Instant) -> Instant {
    let third = expires_at.saturating_duration_since(now) / 3;
    now + third.min(ctx.cfg.renew_every)
}

/// Registers a worker with the farm: the `Hello` a real node would send, the `Welcome` the
/// server answers it with, and the stream the farm's messages come back on.
fn register(
    ctx: &Ctx<'_>,
    id: &WorkerId,
    lease: LeaseId,
    generation: u32,
    predecessor: &str,
) -> (StreamId, String, Reported, Inbox) {
    let instance = format!("kbf-executor/{}/{lease}/{generation}", ctx.cfg.frontend);
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
