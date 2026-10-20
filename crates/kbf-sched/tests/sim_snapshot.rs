//! The snapshot against the scheduler's own accessors over a seed sweep: workers that
//! come and go, actions of several platforms and QoS levels submitted at random, some
//! never started, some silent, and the snapshot checked after every input.
//!
//! The checks: the leases of the snapshot are exactly the leases held, per worker and in
//! lease order; the queue depth is the queue length and the groups add up to it; the
//! oldest age is the largest of the groups'; the waiting groups never count more than
//! the queue holds and each example waits for its reason; and a seed replays to the same
//! snapshots.

use std::collections::BTreeMap;

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, Input, Request, Scheduler};
use kbf_sim::{Chance, SimRng};
use kbf_types::{
    ActionKey, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, LeaseKind, Outcome, Qos,
    Resources, StateMachine, WaiterId, WorkerId,
};

const SEEDS: u64 = 48;
const GIB: u64 = 1 << 30;
const WORKERS: [&str; 3] = ["w0", "w1", "w2"];
const END: u64 = 400;

fn digest(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

struct World {
    sched: Scheduler,
    rng: SimRng,
    now: u64,
    next: u64,
    /// Started leases, and the second each reports.
    running: BTreeMap<u64, Vec<LeaseGrant>>,
    /// Seconds a worker stays silent until.
    silent: BTreeMap<&'static str, u64>,
    /// What the checks saw.
    seen_waiting: u64,
    seen_avoided: u64,
    seen_running: u64,
    seen_not_running: u64,
    trace: Vec<String>,
}

impl World {
    fn new(seed: u64) -> Self {
        let mut w = Self {
            sched: Scheduler::new(3),
            rng: SimRng::from_seed(seed),
            now: 0,
            next: 0,
            running: BTreeMap::new(),
            silent: BTreeMap::new(),
            seen_waiting: 0,
            seen_avoided: 0,
            seen_running: 0,
            seen_not_running: 0,
            trace: Vec::new(),
        };
        for (i, name) in WORKERS.into_iter().enumerate() {
            // The third is a Mac: only it can run the actions that ask for macOS.
            let os = if i == 2 { "macos" } else { "linux" };
            let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", os)])
                .unwrap()
                .with_drivers(["container", "native"]);
            w.feed(Event::WorkerUp {
                worker: WorkerId::new(name),
                instance: DaemonInstance::new(name),
                predecessor_stopped: false,
                capacity: Resources::new(3_000, 8 * GIB),
                caps,
            });
        }
        w
    }

    fn feed(&mut self, event: Event) {
        let now = FarmTime::from_millis(self.now * 1_000);
        self.trace.push(format!("{} {event:?}", self.now));
        let mut effects: Vec<Effect> = self.sched.apply(Input::new(now, event));
        while !effects.is_empty() {
            let mut more = Vec::new();
            for effect in effects {
                match effect {
                    Effect::Commit(record) => {
                        more.extend(self.sched.apply(Input::new(now, Event::Committed(record))));
                    }
                    Effect::Start(start) => {
                        // Most workers say they started; some never do.
                        if self.rng.chance(Chance::percent(85)) {
                            let started = Event::Started {
                                operation: start.operation,
                                lease: start.lease,
                            };
                            more.extend(self.sched.apply(Input::new(now, started)));
                            let ends = self.now + self.rng.between(3, 40);
                            let grant = LeaseGrant {
                                lease: start.lease,
                                operation: start.operation,
                                worker: start.worker,
                            };
                            self.running.entry(ends).or_default().push(grant);
                        }
                    }
                    other => self.trace.push(format!("  {other:?}")),
                }
            }
            effects = more;
        }
        self.check(now);
    }

    fn check(&mut self, now: FarmTime) {
        let snap = self.sched.snapshot();
        assert_eq!(snap.term, 3);
        assert!(snap.now <= now);
        // The leases are the leases held, in lease order.
        assert_eq!(snap.leases.len(), self.sched.leases_held());
        let ids: Vec<_> = snap.leases.iter().map(|l| l.lease).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "lease order");
        for name in WORKERS {
            let node = WorkerId::new(name);
            let mine: Vec<_> = self
                .sched
                .lease_snapshot(Some(&node))
                .iter()
                .map(|l| l.lease)
                .collect();
            assert_eq!(mine, self.sched.leases_on(&node), "{name}");
            let from_all: Vec<_> = snap
                .leases
                .iter()
                .filter(|l| l.node == node)
                .map(|l| l.lease)
                .collect();
            assert_eq!(mine, from_all, "{name}: the filter is a slice of the whole");
        }
        for l in &snap.leases {
            assert!(l.age >= l.phase_age, "{l:?}");
            assert_eq!(l.started_at.is_some(), l.phase.name() == "running", "{l:?}");
            assert!(l.granted_at <= snap.now);
            if l.phase.name() == "running" {
                self.seen_running += 1;
            } else {
                self.seen_not_running += 1;
            }
        }
        // The queue adds up.
        let q = &snap.queue;
        assert_eq!(q.depth, self.sched.queued_len());
        assert_eq!(q.groups.iter().map(|g| g.depth).sum::<usize>(), q.depth);
        assert_eq!(
            q.oldest_age,
            q.groups.iter().map(|g| g.oldest_age).max(),
            "oldest age"
        );
        assert!(q.waiting.iter().map(|g| g.count).sum::<usize>() <= q.depth);
        for g in &q.waiting {
            assert_eq!(
                self.sched.waiting(g.example),
                Some(g.reason.as_str()),
                "example"
            );
            self.seen_waiting += 1;
        }
        assert!(q.avoided_total >= q.avoided.len());
        for a in &q.avoided {
            assert!(self.sched.queued().any(|id| id == a.operation));
            assert!(!a.nodes.is_empty());
            self.seen_avoided += 1;
        }
    }

    fn second(&mut self) {
        for name in WORKERS {
            let worker = WorkerId::new(name);
            if self.silent.get(name).is_some_and(|&until| until > self.now) {
                continue;
            }
            let running = self.sched.leases_on(&worker);
            self.feed(Event::Heartbeat { worker, running });
        }
        if self.now < END - 150 && self.rng.chance(Chance::percent(60)) {
            let n = self.next;
            self.next += 1;
            let mut needs = kbf_caps::Request::default();
            if self.rng.chance(Chance::percent(25)) {
                needs = kbf_caps::Request::parse([("os", "macos")]).unwrap();
            }
            let qos = match self.rng.below(3) {
                0 => Qos::Interactive,
                1 => Qos::Ci,
                _ => Qos::Batch,
            };
            let request = Request {
                key: ActionKey {
                    instance: "main".to_owned(),
                    action: digest(n),
                },
                qos,
                kind: LeaseKind::Action,
                resources: Resources::new(1_000, GIB),
                hermetic: true,
                do_not_cache: false,
                needs,
            };
            self.feed(Event::Submit {
                waiter: WaiterId(n),
                request,
            });
        }
        if self.rng.chance(Chance::percent(3)) {
            let pick = usize::try_from(self.rng.below(3)).expect("small");
            self.silent.insert(WORKERS[pick], self.now + 90);
        }
        if let Some(done) = self.running.remove(&self.now) {
            for grant in done {
                self.feed(Event::Report {
                    operation: grant.operation,
                    lease: grant.lease,
                    outcome: Outcome::Completed {
                        action_result: digest(1_000_000 + grant.operation.0),
                    },
                });
            }
        }
        self.feed(Event::Tick);
    }
}

fn run(seed: u64) -> World {
    let mut w = World::new(seed);
    while w.now < END {
        w.second();
        w.now += 1;
    }
    w
}

/// Catches a snapshot that disagrees with the scheduler it copies, after any input: a
/// lease missing or extra, in the wrong order or on the wrong node; a queue depth that
/// is not the queue's; groups that do not add up; an oldest age that is not the
/// longest; a waiting example that does not wait for its reason; an avoided operation
/// that is not queued. The sweep reaches running and not-running leases, waiting work
/// and avoided work, so each check ran against data.
#[test]
fn the_snapshot_agrees_with_the_scheduler_over_seeds() {
    let (mut waiting, mut avoided, mut running, mut not_running) = (0, 0, 0, 0);
    for seed in 0..SEEDS {
        let w = run(seed);
        waiting += w.seen_waiting;
        avoided += w.seen_avoided;
        running += w.seen_running;
        not_running += w.seen_not_running;
    }
    assert!(running > 0, "no running lease seen");
    assert!(not_running > 0, "no lease that was not running seen");
    assert!(waiting > 0, "no waiting reason seen");
    assert!(avoided > 0, "no avoided operation seen");
}

/// Catches a snapshot that depends on anything but the inputs: one seed twice gives the
/// same trace and the same final snapshot.
#[test]
fn a_seed_replays_to_the_same_snapshot() {
    let (a, b) = (run(5), run(5));
    assert_eq!(a.trace, b.trace);
    assert_eq!(a.sched.snapshot(), b.sched.snapshot());
}
