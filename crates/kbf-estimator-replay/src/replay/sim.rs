//! The discrete-event loop that drives the real scheduler with a trace.
//!
//! Time is the trace's, in milliseconds after its first arrival. Events at one instant
//! run in this order: an attempt ends, an operation arrives, a heartbeat. An attempt
//! ends `run_us` (or `kill_us`) after its `Start`; the estimator hears of it then and not
//! before, so what an arrival can learn from is exactly the attempts that ended at or
//! before it (the look-ahead rule of the design).

use std::collections::{BTreeMap, BTreeSet};

use kbf_caps::NodeCaps;
use kbf_estimator::{
    Bounds, Day, Fallback, Hint, Level, Outcome as RunOutcome, Run, Summaries, Summary, memory_max,
};
use kbf_sched::{
    DaemonInstance, Event, LadderError, MemoryLadder, Request, RequeueReason, Scheduler,
};
use kbf_types::{
    ActionKey, Digest, DigestFunction, Effect, Failure, FarmTime, LeaseId, LeaseKind, OperationId,
    Outcome, Qos, Resources, StartLease, WaiterId, WorkKey, WorkerId,
};

use super::trace::{Op, Trace, fnv};
use super::{Arm, Params};

const DAY_MS: u64 = 86_400_000;

/// Why a replay did not run.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SimError {
    /// The ladder is not one the scheduler accepts.
    #[error("memory ladder: {0}")]
    Ladder(#[from] LadderError),
    /// The trace has no node to run on.
    #[error("the trace names no node with a capacity")]
    NoNodes,
    /// The loop did not end.
    #[error("the replay did not finish within {0} events")]
    Stuck(u64),
}

/// What the estimator could say about an operation when it arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The action has no shape key.
    NoKey,
    /// An estimate from this level.
    Level(Level),
    /// No estimate, and why.
    Cold(Fallback),
}

/// One attempt of one operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttemptRec {
    /// The operation (index into the trace).
    pub op: usize,
    /// When it entered the queue, in milliseconds.
    pub queued_ms: u64,
    /// When it started.
    pub started_ms: u64,
    /// What it booked.
    pub booked: u64,
    /// Whether it was killed at its own limit.
    pub killed: bool,
}

/// One operation of a replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpRec {
    /// When it arrived, in milliseconds.
    pub arrives_ms: u64,
    /// The memory the arm submitted it with.
    pub submitted_memory: u64,
    /// When the operation was answered or refused.
    pub finished_ms: Option<u64>,
    /// It joined an operation of the same action in flight and ran no attempt of its own.
    pub joined: bool,
    /// The scheduler refused it.
    pub refused: bool,
    /// What the estimator knew at arrival.
    pub lookup: Lookup,
    /// Whether its key was mixed at arrival.
    pub mixed: bool,
    /// Whether the arm raised its first booking above what the record's first attempt
    /// booked.
    pub raised: bool,
}

/// What became of the attempts the estimator was shown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SampleFunnel {
    /// Attempts the estimator took as memory samples.
    pub taken: u64,
    /// Attempts it refused, by reason.
    pub refused: BTreeMap<String, u64>,
    /// Attempts that also became CPU samples (none do: the replay has no node CPU
    /// pressure, which the estimator requires).
    pub cpu: u64,
    /// Attempts of operations with no shape key: never shown.
    pub no_key: u64,
}

/// The outcome of one arm.
#[derive(Clone, Debug)]
pub struct ArmRun {
    /// The arm.
    pub arm: Arm,
    /// Every attempt, in start order.
    pub attempts: Vec<AttemptRec>,
    /// Every operation, in trace order.
    pub ops: Vec<OpRec>,
    /// What the estimator took.
    pub samples: SampleFunnel,
    /// The memory of the largest node: a booking at or above it is a whole host.
    pub host_memory: u64,
}

/// An attempt that is running and when it ends.
struct Ending {
    worker: WorkerId,
    lease: LeaseId,
    operation: OperationId,
    op: usize,
    started_ms: u64,
    booked: u64,
    killed: bool,
}

enum Ev {
    Finish(Ending),
    Submit(usize),
    Beat,
}

impl Ev {
    /// The order of events at one instant.
    fn rank(&self) -> u8 {
        match self {
            Self::Finish(_) => 0,
            Self::Submit(_) => 1,
            Self::Beat => 2,
        }
    }
}

struct Sim<'a> {
    trace: &'a Trace,
    arm: Arm,
    params: &'a Params,
    sched: Scheduler,
    now_ms: u64,
    events: BTreeMap<(u64, u8, u64), Ev>,
    seq: u64,
    summaries: Summaries,
    bounds: Bounds,
    needs: kbf_caps::Request,
    /// Operations in flight, by action.
    active: BTreeMap<WorkKey, usize>,
    joined: BTreeMap<usize, Vec<usize>>,
    queued_at: BTreeMap<usize, u64>,
    running: BTreeMap<WorkerId, BTreeSet<LeaseId>>,
    beating: bool,
    out: ArmRun,
}

/// Replays `trace` under `arm`.
///
/// # Errors
/// See [`SimError`].
pub fn simulate(trace: &Trace, arm: Arm, params: &Params) -> Result<ArmRun, SimError> {
    let host_memory = trace
        .nodes
        .values()
        .map(|c| c.memory_bytes)
        .max()
        .ok_or(SimError::NoNodes)?;
    let node_cpu = trace
        .nodes
        .values()
        .map(|c| c.cpu_millis)
        .max()
        .unwrap_or(0);
    let ladder = MemoryLadder::from_gib(&params.ladder_gib)?;
    let mut sched = Scheduler::new(1)
        .with_load_config(kbf_server::PlacementLoad::Off.config())
        .recording_requeues();
    sched.set_memory_ladder(ladder);
    let bounds = Bounds {
        default_memory: params.first_booking(),
        host_memory,
        node_cpu_millis: u32::try_from(node_cpu).unwrap_or(u32::MAX),
    };
    let mut sim = Sim {
        trace,
        arm,
        params,
        sched,
        now_ms: 0,
        events: BTreeMap::new(),
        seq: 0,
        summaries: Summaries::default(),
        bounds,
        // A constant platform, parsed by the same code the front uses.
        needs: kbf_caps::Request::from_platform([("OSFamily", "linux")])
            .expect("OSFamily=linux is a platform kbf serves"),
        active: BTreeMap::new(),
        joined: BTreeMap::new(),
        queued_at: BTreeMap::new(),
        running: BTreeMap::new(),
        beating: false,
        out: ArmRun {
            arm,
            attempts: Vec::new(),
            ops: Vec::new(),
            samples: SampleFunnel::default(),
            host_memory,
        },
    };
    sim.register_nodes();
    for (i, op) in trace.ops.iter().enumerate() {
        sim.out.ops.push(OpRec {
            arrives_ms: op.arrives_us / 1_000,
            submitted_memory: 0,
            finished_ms: None,
            joined: false,
            refused: false,
            lookup: Lookup::NoKey,
            mixed: false,
            raised: false,
        });
        sim.at(op.arrives_us / 1_000, Ev::Submit(i));
    }
    sim.run()?;
    Ok(sim.out)
}

impl Sim<'_> {
    fn at(&mut self, ms: u64, ev: Ev) {
        self.seq += 1;
        self.events.insert((ms, ev.rank(), self.seq), ev);
    }

    fn farm_time(&self) -> FarmTime {
        FarmTime::from_millis(self.now_ms)
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        let now = self.farm_time();
        kbf_sched::harness::feed(&mut self.sched, now, event)
    }

    fn register_nodes(&mut self) {
        // A constant node report: a Linux x86_64 node that runs containers.
        let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
            .expect("a Linux x86_64 report is valid")
            .with_drivers(["container"]);
        for (name, capacity) in &self.trace.nodes {
            let worker = WorkerId::new(name);
            let up = Event::WorkerUp {
                worker: worker.clone(),
                instance: DaemonInstance::new(name),
                predecessor_stopped: false,
                capacity: Resources::new(capacity.cpu_millis, capacity.memory_bytes),
                caps: caps.clone(),
            };
            self.feed(up);
            self.running.insert(worker, BTreeSet::new());
        }
    }

    fn run(&mut self) -> Result<(), SimError> {
        let mut steps = 0_u64;
        while let Some(((ms, _, _), ev)) = self.events.pop_first() {
            steps += 1;
            if steps > self.params.max_events {
                return Err(SimError::Stuck(self.params.max_events));
            }
            self.now_ms = ms;
            match ev {
                Ev::Submit(i) => self.submit(i),
                Ev::Finish(ending) => self.finish(&ending),
                Ev::Beat => self.beat(),
            }
        }
        Ok(())
    }

    fn day(&self) -> Day {
        Day(u32::try_from(self.now_ms / DAY_MS).unwrap_or(u32::MAX))
    }

    /// What the arm books for `op`, and what the estimator could say.
    fn booking(&self, op: &Op) -> (u64, Lookup, bool) {
        let day = self.day();
        let (lookup, mixed, floor, estimate) = match &op.key {
            None => (Lookup::NoKey, false, 0, None),
            Some(key) => {
                let found = self
                    .summaries
                    .estimate(key, day, &self.bounds, &Hint::default());
                let lookup = match &found {
                    Ok(e) => Lookup::Level(e.level),
                    Err(f) => Lookup::Cold(*f),
                };
                let mixed = self.summaries.get(&key.l2()).is_some_and(Summary::mixed);
                // The memory axis counts as named when the record's booking is not the
                // default: a client's hint is never floored.
                let named = op.first_booked != self.params.first_booking();
                let floor = if named {
                    0
                } else {
                    self.summaries.key_floor(key, &self.bounds)
                };
                (lookup, mixed, floor, found.ok().filter(|_| !named))
            }
        };
        let memory = match self.arm {
            Arm::Null => op.first_booked,
            Arm::KeyFloor => op.first_booked.max(floor),
            Arm::SampleUpperBound => {
                estimate.map_or_else(|| op.first_booked.max(floor), |e| e.memory_bytes.max(floor))
            }
        };
        (memory, lookup, mixed)
    }

    fn submit(&mut self, i: usize) {
        let op = &self.trace.ops[i];
        let key = action_key(op);
        if let Some(&leader) = self.active.get(&key) {
            self.joined.entry(leader).or_default().push(i);
            self.out.ops[i].joined = true;
            return;
        }
        let (memory, lookup, mixed) = self.booking(op);
        let rec = &mut self.out.ops[i];
        rec.submitted_memory = memory;
        rec.lookup = lookup;
        rec.mixed = mixed;
        rec.raised = memory > op.first_booked;
        self.active.insert(key.clone(), i);
        self.queued_at.insert(i, self.now_ms);
        self.heartbeat_all();
        let request = Request {
            key,
            qos: Qos::Ci,
            kind: LeaseKind::Action,
            resources: Resources::new(op.cpu_millis, memory),
            needs: self.needs.clone(),
            hermetic: true,
            whole_worker: false,
            do_not_cache: false,
        };
        let effects = self.feed(Event::Submit {
            waiter: WaiterId(i as u64),
            request,
        });
        self.apply(effects);
        self.place();
        if !self.beating {
            self.beating = true;
            self.at(
                self.now_ms.saturating_add(self.params.heartbeat_ms),
                Ev::Beat,
            );
        }
    }

    /// A placement round.
    fn place(&mut self) {
        let effects = self.feed(Event::Tick);
        self.apply(effects);
    }

    /// Every node reports the leases it holds. Nodes do this all the time; the replay does
    /// it when it matters: before a submission after an idle stretch (a node unheard of for
    /// the lease grace is not offered work) and on every beat while work is in flight.
    fn heartbeat_all(&mut self) {
        let workers: Vec<WorkerId> = self.running.keys().cloned().collect();
        for worker in workers {
            let running = self.running[&worker].iter().copied().collect();
            let effects = self.feed(Event::Heartbeat {
                worker,
                running,
                load: None,
            });
            self.apply(effects);
        }
    }

    fn beat(&mut self) {
        self.heartbeat_all();
        self.place();
        if self.active.is_empty() {
            self.beating = false;
        } else {
            self.at(
                self.now_ms.saturating_add(self.params.heartbeat_ms),
                Ev::Beat,
            );
        }
    }

    fn apply(&mut self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::Start(start) => self.start(&start),
                Effect::Answer(answer) => self.answered(&answer.waiters, false),
                Effect::Refuse(refusal) => self.answered(&refusal.waiters, true),
                _ => {}
            }
        }
    }

    fn start(&mut self, start: &StartLease) {
        // The scheduler starts only operations the replay submitted, and keeps each in
        // `active` until it answers.
        let i = *self
            .active
            .get(&start.key)
            .expect("a Start is for an operation in flight");
        let op = &self.trace.ops[i];
        let booked = start.resources.memory_bytes;
        let killed = memory_max(booked).is_some_and(|limit| op.need > limit);
        let micros = if killed { op.kill_us } else { op.run_us };
        self.out.attempts.push(AttemptRec {
            op: i,
            queued_ms: self.queued_at.get(&i).copied().unwrap_or(self.now_ms),
            started_ms: self.now_ms,
            booked,
            killed,
        });
        self.running
            .entry(start.worker.clone())
            .or_default()
            .insert(start.lease);
        let end = self.now_ms.saturating_add((micros / 1_000).max(1));
        self.at(
            end,
            Ev::Finish(Ending {
                worker: start.worker.clone(),
                lease: start.lease,
                operation: start.operation,
                op: i,
                started_ms: self.now_ms,
                booked,
                killed,
            }),
        );
    }

    fn finish(&mut self, e: &Ending) {
        let Ending {
            worker,
            lease,
            operation,
            op: i,
            started_ms,
            booked,
            killed,
        } = e;
        let (lease, operation, i, started_ms, booked, killed) =
            (*lease, *operation, *i, *started_ms, *booked, *killed);
        self.running
            .entry(worker.clone())
            .or_default()
            .remove(&lease);
        let outcome = if killed {
            Outcome::Failed(Failure::OutOfMemory)
        } else {
            Outcome::Completed {
                action_result: Digest::new(DigestFunction::Sha256, [0; 32], 0).into(),
            }
        };
        let effects = self.feed(Event::Report {
            operation,
            lease,
            outcome,
        });
        // What the ladder retried with, for the estimator's floor.
        let raised = self
            .sched
            .take_requeues()
            .into_iter()
            .filter(|r| r.operation == operation)
            .find_map(|r| raised_by(r.reason));
        self.learn(i, started_ms, booked, killed, raised);
        if killed {
            self.queued_at.insert(i, self.now_ms);
        }
        self.apply(effects);
        self.place();
    }

    /// Shows the estimator an attempt that has just ended.
    fn learn(&mut self, i: usize, started_ms: u64, booked: u64, killed: bool, raised: Option<u64>) {
        let op = &self.trace.ops[i];
        let Some(key) = &op.key else {
            self.out.samples.no_key += 1;
            return;
        };
        let wall_us = (self.now_ms - started_ms).saturating_mul(1_000);
        let run = attempt_run(op.need, op.cpu_usage_us, wall_us, booked, killed, raised);
        let seen = self.summaries.observe(key, &run, self.day(), &self.bounds);
        let samples = &mut self.out.samples;
        match seen.memory {
            Ok(()) => samples.taken += 1,
            Err(why) => *samples.refused.entry(format!("{why:?}")).or_default() += 1,
        }
        samples.cpu += u64::from(seen.cpu.entered());
    }

    /// An operation ended: answered, or refused. Its joined twins end with it.
    fn answered(&mut self, waiters: &[WaiterId], refused: bool) {
        // An answer or a refusal has at least the waiter the replay attached.
        let WaiterId(first) = waiters[0];
        let leader = first as usize;
        let key = action_key(&self.trace.ops[leader]);
        self.active.remove(&key);
        let twins = self.joined.remove(&leader).unwrap_or_default();
        for i in std::iter::once(leader).chain(twins) {
            let rec = &mut self.out.ops[i];
            rec.finished_ms = Some(self.now_ms);
            rec.refused = refused;
        }
    }
}

/// What the estimator is shown of an attempt that needed `need` bytes and ran `wall_us`
/// under a booking of `booked`: its peak is cut at the limit, a kill is an own-limit kill.
/// The server's shadow builds the same `Run` from a lease's usage
/// ([`kbf_server::learned::run_of`]); a test holds the two to the same estimates.
pub fn attempt_run(
    need: u64,
    cpu_usage_us: u64,
    wall_us: u64,
    booked: u64,
    killed: bool,
    raised: Option<u64>,
) -> Run {
    let limit = memory_max(booked).unwrap_or(0);
    Run {
        outcome: if killed {
            RunOutcome::OwnLimitKill
        } else {
            RunOutcome::Success
        },
        wall_us,
        peak_memory_bytes: Some(need.min(limit)),
        booked_memory_bytes: booked,
        raised_memory_bytes: raised,
        limit_hits: 0,
        swap_peak_bytes: 0,
        cpu_usage_us,
        node_cpu_pressure_permille: None,
    }
}

/// The booking a requeue retries with, if it is an own-limit kill's.
pub fn raised_by(reason: RequeueReason) -> Option<u64> {
    match reason {
        RequeueReason::OutOfMemory { raised, .. } => Some(raised),
        _ => None,
    }
}

fn action_key(op: &Op) -> WorkKey {
    let seed = fnv(format!("{}\n{}", op.instance, op.digest).as_bytes());
    let mut hash = [0_u8; 32];
    for (i, chunk) in hash.chunks_mut(8).enumerate() {
        chunk.copy_from_slice(
            &fnv(&[seed.to_le_bytes().as_slice(), &[i as u8]].concat()).to_le_bytes(),
        );
    }
    ActionKey {
        instance: op.instance.clone(),
        action: Digest::new(DigestFunction::Sha256, hash, 1),
    }
    .into()
}
