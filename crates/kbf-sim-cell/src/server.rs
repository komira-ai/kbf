//! The simulated server: `kbf-server --meta-log=raft` on a fault-injecting disk.
//!
//! - **Metadata** is a [`RaftMetaLog`] over the server's [`FaultFs`], opened as the
//!   server opens it (a new log's id drawn from the seed). Its futures never wait: each
//!   is polled once, inside a Tokio runtime context for the tick task the open spawns,
//!   which never runs. Farm time is the simulation's clock: the server commits
//!   `Tick(now)` every [`TICK`] instead.
//! - **The farm core** is a [`Scheduler`] of the log's term and a [`FarmMachine`],
//!   fed as `crates/kbf-server/src/farm.rs` feeds them: each input and then a tick, a
//!   committed record fed straight back, a `Start` recorded in the machine, a result
//!   taken only from the holder of the operation's current lease and written to the
//!   action cache before its callers are answered (and answered even if that write
//!   fails), `Welcome` naming the log id as the lease epoch. Both live in memory, so a
//!   restart loses them, as the server does today.
//! - **Writes** go to the object store first; the blob is committed to the log once
//!   the store has it, and only then is the client told.
//! - **Faults** ([`ServerFault`]): a process kill, a power cut (now, or part-way
//!   through a later filesystem operation), and an I/O error, after which the log
//!   refuses every commit while the server goes on answering reads from its applied
//!   state. A server that has stopped or died starts again after
//!   [`ServerPlan::down_for`] over what the disk kept; a server whose log stopped
//!   starts again after its machine reboots.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::num::NonZeroU64;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_daemon::FENCE_AFTER;
use kbf_front::MetaLog;
use kbf_log::{FarmLogId, Format, LogCommand};
use kbf_meta::{
    ActionAnswer, ActionRecord, Applied, Closure, Command, Epoch, Location, MetaState, ObjectId,
    Retention, Role, StoreId,
};
use kbf_raft::Payload;
use kbf_sched::{DaemonInstance, Event as SchedEvent, Input, OpState, Request, Scheduler};
use kbf_server::RaftMetaLog;
use kbf_server::machine::{FarmMachine, Sent, Waiter};
use kbf_server::stamp::Stamp;
use kbf_sim::{Event, NodeId, NodeInput, Output};
use kbf_store::{Fault, FaultFs, Options, Store};
use kbf_types::{
    ActionKey, Answer, Digest, Effect, FarmTime, LeaseId, LeaseKind, Outcome, Qos, Resources,
    StartLease, StateMachine, WaiterId, WorkerId,
};

use crate::Msg;
use crate::check::Property;

/// How often the server commits farm time and runs a placement round.
pub const TICK: Duration = Duration::from_secs(1);
/// The heartbeat interval `Welcome` names.
pub const HEARTBEAT: Duration = Duration::from_secs(5);
const T_TICK: u64 = 0;
const T_START: u64 = 1;
const T_FAULT: u64 = 1 << 16;

/// A fault the server meets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerFault {
    /// The process is killed: everything it wrote stays readable.
    Kill,
    /// The machine loses power: only what was synced survives, with up to `keep`
    /// bytes of each file's unsynced tail and the name changes `names` selects
    /// ([`FaultFs::crash_reordered`]).
    PowerCut { keep: usize, names: u64 },
    /// As [`ServerFault::PowerCut`], at the `after`-th filesystem operation from now:
    /// part-way through a commit.
    PowerCutAt { after: u64, keep: usize, names: u64 },
    /// The `after`-th filesystem operation from now fails: the log stops.
    IoErrorAt { after: u64 },
}

/// When the server meets its faults, and how long it stays down after each.
#[derive(Clone, Debug, Default)]
pub struct ServerPlan {
    pub faults: Vec<(FarmTime, ServerFault)>,
    pub down_for: Duration,
}

/// What the server did that the checks read.
#[derive(Debug, Default)]
pub struct Seen {
    /// Each blob answered present, and when.
    pub present: Vec<(FarmTime, Digest)>,
    /// Each lease given up while its daemon was heard within T listing it.
    pub requeued_listed: Vec<(FarmTime, LeaseId, WorkerId)>,
    /// How many times each lease's result was accepted.
    pub accepted: BTreeMap<LeaseId, u32>,
    /// The actions with an accepted completed result.
    pub won: BTreeSet<Digest>,
    /// Those whose action-cache entry committed.
    pub cached: BTreeSet<Digest>,
    /// How many times the server opened its log, and each refusal.
    pub starts: u64,
    pub refused: Vec<(FarmTime, String)>,
}

/// The server node.
pub struct Server {
    plan: ServerPlan,
    disk: FaultFs,
    runtime: tokio::runtime::Runtime,
    up: Option<Up>,
    /// A planned power cut, armed in the disk: what it keeps.
    armed: Option<(usize, u64)>,
    restarting: bool,
    store: NodeId,
    /// What the last property-1 check saw: (incarnation, applied index, disk ops).
    checked: Cell<Option<(u64, u64, u64)>>,
    pub seen: Seen,
    out: Vec<Output<Msg>>,
}

struct Up {
    log: RaftMetaLog<FaultFs>,
    stopped: bool,
    /// The out queue's length when a commit failed, if one has.
    failed_at: Option<usize>,
    writer: Epoch,
    next_object: u64,
    lease_epoch: u64,
    sched: Scheduler,
    machine: FarmMachine,
    streams: BTreeMap<WorkerId, u64>,
    heard: BTreeMap<WorkerId, (FarmTime, Vec<LeaseId>)>,
    callers: BTreeMap<WaiterId, NodeId>,
    uploads: BTreeMap<ObjectId, (NodeId, Digest)>,
}

/// Polls `future` once: the metadata log answers without waiting.
fn ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(v) => v,
        Poll::Pending => unreachable!("the metadata log answers without waiting"),
    }
}

fn wall(t: FarmTime) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(t.as_millis())
}

/// The request every action makes: one core and 1 GiB, hermetic.
fn request(action: Digest) -> Request {
    Request {
        key: key(action),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, 1 << 30),
        needs: kbf_caps::Request::default(),
        hermetic: true,
        do_not_cache: false,
    }
}

fn key(action: Digest) -> ActionKey {
    ActionKey {
        instance: "main".to_owned(),
        action,
    }
}

impl Server {
    /// A server with an empty disk that meets `plan`'s faults and writes its objects
    /// to `store`.
    ///
    /// # Panics
    ///
    /// If a Tokio runtime cannot be built.
    #[must_use]
    pub fn new(plan: ServerPlan, store: NodeId) -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime");
        Self {
            plan,
            disk: FaultFs::new(),
            runtime,
            up: None,
            armed: None,
            restarting: false,
            store,
            checked: Cell::new(None),
            seen: Seen::default(),
            out: Vec::new(),
        }
    }

    pub(crate) fn take_outputs(&mut self) -> Vec<Output<Msg>> {
        std::mem::take(&mut self.out)
    }

    /// The applied metadata state, while the server is up.
    #[must_use]
    pub fn meta(&self) -> Option<MetaState> {
        let up = self.up.as_ref()?;
        ready(up.log.query(MetaState::clone)).ok()
    }

    /// Properties 1 and 6 against the disk: a replica that replays the durable log
    /// (what a power cut now would leave) through the server's applied index has the
    /// server's applied state and `Bootstrap`, and its farm time never goes back from
    /// one entry to the next. Skipped while the server is down, and when neither its
    /// applied index nor its disk has changed since the last check.
    ///
    /// # Errors
    ///
    /// The property that does not hold, and how.
    pub fn check_durable(&self) -> Result<(), (Property, String)> {
        let Some(up) = &self.up else { return Ok(()) };
        let applied = up.log.applied_index();
        let key = (self.seen.starts, applied, self.disk.ops());
        if self.checked.replace(Some(key)) == Some(key) {
            return Ok(());
        }
        let equality = |d: String| (Property::ReplicaEquality, d);
        let (_, stored) = Store::open(self.disk.crash(0), Options::default())
            .map_err(|e| equality(format!("the durable log does not open: {e}")))?;
        let mut replica = MetaState::new(Retention::default());
        let mut bootstrap = None;
        let mut last = 0;
        for entry in stored
            .entries
            .iter()
            .take_while(|e| e.id.index.0 <= applied)
        {
            last = entry.id.index.0;
            let Payload::Command(bytes) = &entry.payload else {
                continue;
            };
            match kbf_log::decode(Format::V1, bytes) {
                Ok(LogCommand::Meta(command)) => {
                    let before = replica.now();
                    replica.execute(last, command);
                    if replica.now() < before {
                        let d = format!("entry {last} moves farm time back from {before:?}");
                        return Err((Property::MonotoneTime, d));
                    }
                }
                Ok(LogCommand::Bootstrap(b)) => {
                    bootstrap.get_or_insert(b);
                }
                other => return Err(equality(format!("entry {last} is {other:?}"))),
            }
        }
        if last < applied {
            let d = format!("entry {applied} is applied, but the durable log ends at {last}");
            return Err(equality(d));
        }
        let live = ready(up.log.query(MetaState::clone)).expect("a query is never refused");
        if live != replica {
            let d = format!("the state applied through {applied} is not the durable log's");
            return Err(equality(d));
        }
        if bootstrap.map(|b| b.log_id) != Some(up.log.log_id()) {
            return Err(equality(format!(
                "the durable log does not name log {}",
                up.log.log_id()
            )));
        }
        Ok(())
    }

    fn timer(&mut self, after: Duration, tag: u64) {
        self.out.push(Output::Timer { after, tag });
    }

    /// Opens the log over what the disk holds and starts an empty farm core of its
    /// term. A refused open is recorded and tried again after `down_for`.
    fn start(&mut self, now: FarmTime, entropy: u64) {
        let log_id = FarmLogId::new(NonZeroU64::new(entropy).unwrap_or(NonZeroU64::MIN));
        let opened = {
            let _context = self.runtime.enter();
            ready(RaftMetaLog::open(
                self.disk.clone(),
                Retention::default(),
                "cell/",
                move || Ok(log_id),
            ))
        };
        self.seen.starts += 1;
        let log = match opened {
            Ok(log) => log,
            Err(e) => {
                self.seen.refused.push((now, e.to_string()));
                self.restart_later();
                return;
            }
        };
        let leadership = log.leadership().expect("a raft log names its term");
        let Ok(Applied::Epoch(writer)) = ready(log.commit(Command::AllocEpoch)) else {
            self.down(None);
            return;
        };
        self.up = Some(Up {
            lease_epoch: leadership.log_id.get(),
            sched: Scheduler::new(leadership.term).recording_requeues(),
            machine: FarmMachine::new(leadership.term),
            log,
            stopped: false,
            failed_at: None,
            writer,
            next_object: 0,
            streams: BTreeMap::new(),
            heard: BTreeMap::new(),
            callers: BTreeMap::new(),
            uploads: BTreeMap::new(),
        });
    }

    fn restart_later(&mut self) {
        if !std::mem::replace(&mut self.restarting, true) {
            self.timer(self.plan.down_for, T_START);
        }
    }

    /// The process dies: after a power cut keeping `cut`, or killed.
    fn down(&mut self, cut: Option<(usize, u64)>) {
        self.up = None;
        self.armed = None;
        self.disk = match cut {
            Some((keep, names)) => self.disk.crash_reordered(keep, names),
            None => self.disk.restart(),
        };
        self.restart_later();
    }

    fn fault(&mut self, fault: ServerFault) {
        match fault {
            ServerFault::Kill if self.up.is_some() => self.down(None),
            ServerFault::PowerCut { keep, names } if self.up.is_some() => {
                self.down(Some((keep, names)));
            }
            ServerFault::PowerCutAt { after, keep, names } if self.up.is_some() => {
                self.disk.fail_at(self.disk.ops() + after, Fault::Crash);
                self.armed = Some((keep, names));
            }
            ServerFault::IoErrorAt { after } if self.up.is_some() => {
                self.disk.fail_at(self.disk.ops() + after, Fault::Error);
            }
            _ => {}
        }
    }

    /// After an input: a commit that failed at a planned power cut kills the process
    /// (nothing it queued after the failure leaves it); one that failed otherwise has
    /// stopped the log, and the server is restarted after `down_for`.
    fn settle_failure(&mut self) {
        let Some(at) = self.up.as_mut().and_then(|up| up.failed_at.take()) else {
            return;
        };
        if let Some(cut) = self.armed.take() {
            self.out.truncate(at);
            self.down(Some(cut));
        } else {
            self.restart_later();
        }
    }
}

impl Up {
    fn commit(&mut self, out: &[Output<Msg>], command: Command) -> Option<Applied> {
        if self.stopped {
            return None;
        }
        let applied = ready(self.log.commit(command)).ok();
        if applied.is_none() {
            self.stopped = true;
            self.failed_at = Some(out.len());
        }
        applied
    }

    fn query<R: Send>(&self, f: impl FnOnce(&MetaState) -> R + Send) -> R {
        ready(self.log.query(f)).expect("a query is never refused")
    }

    /// Feeds `event` and then a tick, carrying out the effects in order, as the farm
    /// core does; returns the answers.
    fn feed(&mut self, now: FarmTime, event: SchedEvent, cx: &mut Cx<'_>) -> Vec<Answer> {
        let mut effects: VecDeque<Effect> = self.sched.apply(Input::new(now, event)).into();
        effects.extend(self.sched.apply(Input::new(now, SchedEvent::Tick)));
        let mut answers = Vec::new();
        while let Some(effect) = effects.pop_front() {
            match effect {
                Effect::Commit(record) => {
                    let committed = SchedEvent::Committed(record);
                    effects.extend(self.sched.apply(Input::new(now, committed)));
                }
                Effect::Start(start) => self.start_lease(now, &start, cx),
                Effect::Answer(answer) => answers.push(answer),
                Effect::Waiting(_) => {}
                Effect::Refuse(refusal) => {
                    self.machine.forget_leases(refusal.operation);
                    self.answer_callers(&refusal.waiters, false, cx);
                    self.machine
                        .keep_finished(refusal.operation, refusal.waiters);
                }
            }
        }
        for requeue in self.sched.take_requeues() {
            let listed = self
                .heard
                .get(&requeue.worker)
                .is_some_and(|(at, running)| {
                    now.saturating_duration_since(*at) < FENCE_AFTER
                        && running.contains(&requeue.lease)
                });
            if listed {
                let seen = (now, requeue.lease, requeue.worker);
                cx.seen.requeued_listed.push(seen);
            }
        }
        let sched = &self.sched;
        for id in self.machine.forget_dropped(|op| sched.state(op).is_some()) {
            self.callers.remove(&id);
        }
        answers
    }

    fn start_lease(&mut self, now: FarmTime, start: &StartLease, cx: &mut Cx<'_>) {
        let worker = &start.worker;
        let stream = self.streams.get(worker).copied().unwrap_or_default();
        cx.send(
            NodeId::from(worker.as_str()),
            Msg::Start {
                stream,
                lease: start.lease,
                operation: start.operation,
                action: start.key.action,
            },
        );
        let queued = self
            .sched
            .waiters(start.operation)
            .and_then(|w| w.first())
            .and_then(|id| self.machine.waiter(*id))
            .map_or(wall(now), |w| w.queued);
        let sent = Sent {
            operation: start.operation,
            action: start.key.action,
            stamp: Stamp {
                queued,
                started: wall(now),
            },
        };
        self.machine.start(start.lease, sent);
    }

    fn answer_callers(&self, waiters: &[WaiterId], cached: bool, cx: &mut Cx<'_>) {
        for id in waiters {
            let caller = self.callers.get(id);
            let waiter = self.machine.waiter(*id);
            if let (Some(caller), Some(waiter)) = (caller, waiter) {
                let action = waiter.key.action;
                cx.send(caller.clone(), Msg::Answered { action, cached });
            }
        }
    }

    fn hello(
        &mut self,
        now: FarmTime,
        from: &NodeId,
        stream: u64,
        instance: String,
        capacity: Resources,
        cx: &mut Cx<'_>,
    ) {
        let worker = WorkerId::new(from.as_str());
        if self.streams.get(&worker).is_some_and(|s| *s >= stream) {
            return;
        }
        self.streams.insert(worker.clone(), stream);
        self.machine.register(&worker);
        let welcome = Msg::Welcome {
            stream,
            epoch: self.lease_epoch,
            interval_ms: u64::try_from(HEARTBEAT.as_millis()).expect("small"),
        };
        cx.send(from.clone(), welcome);
        let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64")])
            .expect("a valid report")
            .with_drivers(["container"]);
        let event = SchedEvent::WorkerUp {
            worker,
            instance: DaemonInstance::new(instance),
            capacity,
            caps,
        };
        self.feed(now, event, cx);
    }

    fn heartbeat(
        &mut self,
        now: FarmTime,
        from: &NodeId,
        stream: u64,
        seq: u64,
        running: Vec<LeaseId>,
        cx: &mut Cx<'_>,
    ) {
        let worker = WorkerId::new(from.as_str());
        if self.streams.get(&worker) != Some(&stream) {
            cx.send(from.clone(), Msg::Reset { stream });
            return;
        }
        cx.send(from.clone(), Msg::HeartbeatAck { stream, seq });
        self.heard.insert(worker.clone(), (now, running.clone()));
        let event = SchedEvent::Heartbeat {
            worker: worker.clone(),
            running: running.clone(),
        };
        self.feed(now, event, cx);
        let cancels: Vec<LeaseId> = self.sched.not_held(&worker, &running).collect();
        for lease in cancels {
            cx.send(from.clone(), Msg::Cancel { stream, lease });
        }
    }

    /// The operation whose current lease is `lease`, if `worker` holds it and this
    /// process sent its `Start`.
    fn holder(&self, lease: LeaseId, worker: &WorkerId) -> Option<Sent> {
        let sent = self.machine.sent(lease)?;
        let current = match self.sched.state(sent.operation)? {
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
        current.then_some(sent)
    }

    fn report(
        &mut self,
        now: FarmTime,
        from: &NodeId,
        lease: LeaseId,
        action: Digest,
        outcome: Outcome,
        cx: &mut Cx<'_>,
    ) {
        let worker = WorkerId::new(from.as_str());
        let holder = self
            .holder(lease, &worker)
            .filter(|sent| sent.action == action);
        let Some(sent) = holder else {
            cx.send(
                from.clone(),
                Msg::ResultAck {
                    lease,
                    accepted: false,
                },
            );
            return;
        };
        let event = SchedEvent::Report {
            operation: sent.operation,
            lease,
            outcome,
        };
        let answers = self.feed(now, event, cx);
        let accepted = answers.iter().any(|a| a.lease == lease);
        for answer in answers {
            self.settle(&answer, cx);
        }
        if accepted {
            *cx.seen.accepted.entry(lease).or_default() += 1;
        }
        cx.send(from.clone(), Msg::ResultAck { lease, accepted });
    }

    /// Writes a completed answer's action-cache entry, then answers its callers.
    fn settle(&mut self, answer: &Answer, cx: &mut Cx<'_>) {
        self.machine.forget_leases(answer.operation);
        let action = answer
            .waiters
            .first()
            .and_then(|id| self.machine.waiter(*id))
            .map(|w| w.key.action);
        if let (Outcome::Completed { action_result }, Some(action)) = (answer.outcome, action) {
            cx.seen.won.insert(action);
            let record = ActionRecord {
                result: action_result,
                closure: Closure::new(),
            };
            let put = Command::PutAction {
                role: Role::Daemon,
                action,
                record,
            };
            if let Some(Applied::Action(Ok(_))) = self.commit(cx.out, put) {
                cx.seen.cached.insert(action);
            }
        }
        self.answer_callers(&answer.waiters, false, cx);
        self.machine
            .keep_finished(answer.operation, answer.waiters.clone());
        let sched = &self.sched;
        for id in self.machine.forget_dropped(|op| sched.state(op).is_some()) {
            self.callers.remove(&id);
        }
    }

    fn execute(&mut self, now: FarmTime, from: &NodeId, action: Digest, cx: &mut Cx<'_>) {
        if let ActionAnswer::Hit { .. } = self.query(|m| m.action(&action)) {
            cx.send(
                from.clone(),
                Msg::Answered {
                    action,
                    cached: true,
                },
            );
            return;
        }
        let waiter = self.machine.submit(Waiter {
            key: key(action),
            kind: LeaseKind::Action,
            do_not_cache: false,
            queued: wall(now),
        });
        self.callers.insert(waiter, from.clone());
        let event = SchedEvent::Submit {
            waiter,
            request: request(action),
        };
        self.feed(now, event, cx);
    }

    fn receive(&mut self, now: FarmTime, from: &NodeId, msg: Msg, cx: &mut Cx<'_>) {
        match msg {
            Msg::Upload { digest } => {
                let object = ObjectId::new(self.writer, self.next_object);
                self.next_object += 1;
                self.uploads.insert(object, (from.clone(), digest));
                cx.send(cx.store.clone(), Msg::Put { object, digest });
            }
            Msg::PutDone { object, digest } => {
                let Some((client, _)) = self.uploads.remove(&object) else {
                    return;
                };
                let location = Location {
                    store: StoreId::new(0),
                    object,
                    offset: 0,
                };
                let put = Command::PutBlob { digest, location };
                if let Some(Applied::Blob(Ok(_))) = self.commit(cx.out, put) {
                    cx.send(client, Msg::Uploaded { digest });
                }
            }
            Msg::FindMissing { digests } => {
                let missing = self.query(|m| m.find_missing(&digests)).missing;
                for digest in digests.iter().filter(|d| !missing.contains(d)) {
                    cx.seen.present.push((now, *digest));
                }
                let reply = Msg::Missing {
                    asked: digests,
                    missing,
                };
                cx.send(from.clone(), reply);
            }
            Msg::Execute { action } => self.execute(now, from, action, cx),
            Msg::Hello {
                stream,
                instance,
                capacity,
            } => self.hello(now, from, stream, instance, capacity, cx),
            Msg::Heartbeat {
                stream,
                seq,
                running,
            } => self.heartbeat(now, from, stream, seq, running, cx),
            Msg::Report {
                lease,
                action,
                outcome,
                ..
            } => self.report(now, from, lease, action, outcome, cx),
            _ => {}
        }
    }
}

/// What a handler writes to besides the farm core.
struct Cx<'a> {
    out: &'a mut Vec<Output<Msg>>,
    seen: &'a mut Seen,
    store: &'a NodeId,
}

impl Cx<'_> {
    fn send(&mut self, to: NodeId, msg: Msg) {
        self.out.push(Output::Send { to, msg });
    }
}

impl StateMachine for Server {
    type Input = NodeInput<Msg>;

    fn apply(&mut self, input: NodeInput<Msg>) -> Vec<Effect> {
        let now = input.now;
        match input.event {
            Event::Start => {
                self.timer(TICK, T_TICK);
                for (i, (at, _)) in self.plan.faults.iter().enumerate() {
                    let after = at.saturating_duration_since(now);
                    self.out.push(Output::Timer {
                        after,
                        tag: T_FAULT + i as u64,
                    });
                }
                self.start(now, input.entropy);
            }
            Event::Timer { tag: T_TICK } => {
                self.timer(TICK, T_TICK);
                if let Some(up) = self.up.as_mut() {
                    let mut cx = Cx {
                        out: &mut self.out,
                        seen: &mut self.seen,
                        store: &self.store,
                    };
                    up.commit(cx.out, Command::Tick(now));
                    up.feed(now, SchedEvent::Tick, &mut cx);
                }
            }
            Event::Timer { tag: T_START } => {
                self.restarting = false;
                if self.up.take().is_some() {
                    // A stopped log: the machine reboots, and only what was synced is
                    // read back. A new process over the same page cache could read
                    // back a write whose sync failed; `--runtime-dir` is planned to
                    // refuse that (docs/deploy/storage-node.md).
                    self.disk = self.disk.crash(0);
                }
                self.start(now, input.entropy);
            }
            Event::Timer { tag } => {
                let index = usize::try_from(tag - T_FAULT).expect("a fault index");
                let fault = self.plan.faults[index].1;
                self.fault(fault);
            }
            Event::Message { from, msg } => {
                if let Some(up) = self.up.as_mut() {
                    let mut cx = Cx {
                        out: &mut self.out,
                        seen: &mut self.seen,
                        store: &self.store,
                    };
                    up.receive(now, &from, msg, &mut cx);
                }
            }
        }
        self.settle_failure();
        Vec::new()
    }
}
