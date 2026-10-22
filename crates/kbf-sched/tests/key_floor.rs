//! The kill-driven key floor (failure classes, 6.1; the learned-sizes design, E6): an
//! action killed at its own limit leaves a floor under its `size_key`, which every
//! operation of the key picks up at the next placement round, strictly below the memory
//! of the largest live node, and which a successor books the same way.
//!
//! Each test names the defect it catches and the mutant that was planted to see it go
//! red (see the pull request).

use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::fence::LEASE_GRACE;
use kbf_sched::{
    DaemonInstance, Event, KeyFloor, OpState, Recovered, RecoveredLease, Request, Scheduler,
};
use kbf_sim::SimRng;
use kbf_types::{
    ActionKey, Digest, DigestFunction, Effect, Failure, FarmTime, LeaseKind, OperationId, Outcome,
    Qos, Resources, StartLease, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
const OOM: Outcome = Outcome::Failed(Failure::OutOfMemory);
/// The key every action of these tests shares unless a test says otherwise.
const KEY: u64 = 0x5eed;

fn action(n: u8, instance: &str) -> ActionKey {
    ActionKey {
        instance: instance.to_owned(),
        action: Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n)),
    }
}

/// Action `n` of instance `main`: one core and `memory` bytes on Linux, keyed `size_key`.
fn request(n: u8, memory: u64, size_key: Option<u64>) -> Request {
    Request {
        key: action(n, "main"),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, memory),
        hermetic: true,
        do_not_cache: false,
        size_key,
        needs: kbf_caps::Request::from_platform([("OSFamily", "linux")]).unwrap(),
    }
}

fn linux() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container"])
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn ok() -> Outcome {
    Outcome::Completed {
        action_result: Digest::new(DigestFunction::Sha256, [200; 32], 1),
    }
}

/// A scheduler for term 1 with a clock the test moves; every commit is fed straight back.
struct Farm {
    s: Scheduler,
    now: FarmTime,
}

impl Farm {
    fn new() -> Self {
        Self::over(Scheduler::new(1))
    }

    fn over(s: Scheduler) -> Self {
        Self {
            s,
            now: FarmTime::default(),
        }
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        kbf_sched::harness::feed(&mut self.s, self.now, event)
    }

    fn advance(&mut self, by: Duration) {
        self.now = self.now.saturating_add(by);
    }

    fn node(&mut self, name: &str, cpus: u64, memory_gib: u64) {
        let event = Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity: Resources::new(cpus * 1_000, memory_gib * GIB),
            caps: linux(),
        };
        assert!(self.feed(event).is_empty());
    }

    fn heartbeat(&mut self, name: &str, running: &[&StartLease]) {
        let event = Event::Heartbeat {
            worker: w(name),
            running: running.iter().map(|s| s.lease).collect(),
        };
        self.feed(event);
    }

    fn submit(&mut self, waiter: u64, request: Request) {
        let waiter = WaiterId(waiter);
        assert!(self.feed(Event::Submit { waiter, request }).is_empty());
    }

    /// A placement round: every `Start` it makes.
    fn tick(&mut self) -> Vec<StartLease> {
        starts(&self.feed(Event::Tick))
    }

    /// `start` ends with `outcome`, then a placement round: the `Start`s that made.
    fn end(&mut self, start: &StartLease, outcome: Outcome) -> Vec<StartLease> {
        let event = Event::Report {
            operation: start.operation,
            lease: start.lease,
            outcome,
        };
        let mut effects = self.feed(event);
        effects.extend(self.feed(Event::Tick));
        starts(&effects)
    }

    /// `start` is killed at its own limit; it must run again: the next `Start`.
    fn killed(&mut self, start: &StartLease) -> StartLease {
        let next = self.end(start, OOM);
        let [next] = next.as_slice() else {
            panic!("want the rerun of {start:?}: {next:?}");
        };
        assert_eq!(next.operation, start.operation);
        next.clone()
    }
}

fn starts(effects: &[Effect]) -> Vec<StartLease> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Start(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// One start, whatever else a tick did.
fn only(mut starts: Vec<StartLease>) -> StartLease {
    assert_eq!(starts.len(), 1, "{starts:?}");
    starts.remove(0)
}

/// An action of key `KEY` is killed at 4 GiB, then at 16: the key now floors at 32 GiB.
/// Returns the farm, with the last (finished) operation's lease done.
fn farm_with_floor_16() -> Farm {
    let mut f = Farm::new();
    f.node("big", 8, 64);
    f.submit(1, request(1, 4 * GIB, Some(KEY)));
    let first = only(f.tick());
    let second = f.killed(&first);
    assert_eq!(second.resources.memory_bytes, 16 * GIB);
    assert!(f.end(&second, ok()).is_empty());
    assert_eq!(f.s.key_floor(KEY), Some(16 * GIB));
    f
}

/// Catches the certain win's absence: a changed digest of a key that was killed starts
/// the ladder again at 4 GiB (the digest floor does not know it), so it is killed again
/// and climbs 4 -> 16 once more. Mutant: the kill leaves no key floor (nothing is
/// raised in `rerun_after_memory_kill`).
#[test]
fn a_new_digest_of_a_killed_key_starts_at_the_floor() {
    let mut f = farm_with_floor_16();
    f.submit(2, request(2, 4 * GIB, Some(KEY)));
    let start = only(f.tick());
    assert_eq!(start.resources.memory_bytes, 16 * GIB);
    assert_eq!(
        f.s.memory_floor(&action(2, "main")),
        None,
        "the digest has no floor of its own"
    );
    // And it climbs from there, not from 4: killed at 16 it retries at 32.
    let third = f.killed(&start);
    assert_eq!(third.resources.memory_bytes, 32 * GIB);
    assert_eq!(f.s.key_floor(KEY), Some(32 * GIB));
}

/// Catches a key floor applied only when an operation is queued: operations of the key
/// already waiting when the kill happens still start at their old booking (and are
/// killed, one by one, for what the first kill taught). One core, so two of them wait.
/// Mutant: the raise happens in `submit` and not in the placement round.
#[test]
fn queued_operations_pick_up_the_floor_at_placement() {
    let mut f = Farm::new();
    f.node("one-core", 1, 64);
    for n in 1..=3 {
        f.submit(u64::from(n), request(n, 4 * GIB, Some(KEY)));
    }
    let first = only(f.tick());
    assert_eq!(first.resources.memory_bytes, 4 * GIB);
    let rerun = f.killed(&first);
    assert_eq!(rerun.resources.memory_bytes, 16 * GIB);
    assert_eq!(f.s.key_floor(KEY), Some(16 * GIB));
    // Operations 2 and 3 were queued at 4 GiB before the kill.
    let second = only(f.end(&rerun, ok()));
    assert_ne!(second.operation, first.operation);
    assert_eq!(second.resources.memory_bytes, 16 * GIB, "queued before");
    let third = only(f.end(&second, ok()));
    assert_eq!(third.resources.memory_bytes, 16 * GIB, "queued before");
    // The grant, not only the request, books it: the node counts it.
    assert_eq!(f.s.booked(&w("one-core")).unwrap().memory_bytes, 16 * GIB);
}

/// Catches a key floor that lets one request raise another's: the key stands for
/// (instance, platform class, basis, shape), so an action under another key (another
/// instance, or another platform) is untouched, and so is one with no key. The
/// scheduler keeps floors per key value; hashing the instance in is the front door's
/// job (and tested there). Mutant: `floor_key` ignores the key and floors every
/// request of the same action digest.
#[test]
fn another_key_and_no_key_are_unchanged() {
    let mut f = farm_with_floor_16();
    let mut other = request(2, 4 * GIB, Some(KEY + 1));
    other.key.instance = "other".to_owned();
    f.submit(2, other);
    f.submit(3, request(3, 4 * GIB, None));
    let starts = f.tick();
    assert_eq!(starts.len(), 2);
    for start in &starts {
        assert_eq!(start.resources.memory_bytes, 4 * GIB, "{start:?}");
    }
    assert_eq!(f.s.key_floor(KEY + 1), None);
}

/// Catches a key floor for a request that is not an ordinary action: a request for GPUs
/// books by another measure, and a whole-machine lease books the machine. Both ignore a
/// `size_key` they carry, in what they book and in what they leave behind when killed.
/// Mutant: `floor_key` drops the kind and GPU check.
#[test]
fn only_an_action_without_gpus_has_a_key_floor() {
    let mut f = farm_with_floor_16();
    let mut gpu_caps = linux();
    gpu_caps.consumables.insert(kbf_caps::Consumable::Gpus, 1);
    f.feed(Event::WorkerUp {
        worker: w("gpu"),
        instance: DaemonInstance::new("gpu"),
        predecessor_stopped: false,
        capacity: Resources::new(8_000, 64 * GIB).with_gpus(1),
        caps: gpu_caps,
    });
    let whole_caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["native-whole-machine"]);
    f.feed(Event::WorkerUp {
        worker: w("whole"),
        instance: DaemonInstance::new("whole"),
        predecessor_stopped: false,
        capacity: Resources::new(8_000, 8 * GIB),
        caps: whole_caps,
    });

    let mut gpu = request(2, 4 * GIB, Some(KEY));
    gpu.resources = gpu.resources.with_gpus(1);
    let mut machine = request(3, 4 * GIB, Some(KEY));
    machine.kind = LeaseKind::WholeMachine;
    f.submit(2, gpu.clone());
    f.submit(3, machine);
    let started = f.tick();
    assert_eq!(started.len(), 2, "{started:?}");
    let gpu_start = started.iter().find(|s| s.worker == w("gpu")).expect("gpu");
    assert_eq!(gpu_start.resources.memory_bytes, 4 * GIB, "a GPU request");
    // The whole-machine lease would not fit its 8 GiB node at 16 GiB.
    let whole_start = started
        .iter()
        .find(|s| s.worker == w("whole"))
        .expect("whole");
    assert_eq!(whole_start.resources.memory_bytes, 8 * GIB);

    // And a GPU request killed under a key of its own leaves no key floor.
    assert!(f.end(gpu_start, ok()).is_empty());
    let mut other = gpu;
    other.key = action(4, "main");
    other.size_key = Some(KEY + 1);
    f.submit(4, other);
    let start = only(f.tick());
    let rerun = f.killed(&start);
    assert_eq!(rerun.resources.memory_bytes, 16 * GIB);
    assert_eq!(f.s.key_floor(KEY + 1), None, "a GPU kill left a key floor");
}

/// Catches 50 concurrent kills of one key counted as 50 events: each kill of an
/// operation booked below the floor already in force belongs to the event that set it,
/// so the floor is one rung (16 GiB), not a rung per kill, and every retry and every
/// later digest books exactly it. Mutant: the kill's rung is taken from `max(booked,
/// floor)`, so each of the 50 climbs the floor again.
#[test]
fn a_burst_of_kills_is_one_event() {
    let mut f = Farm::new();
    f.node("wide", 128, 256);
    for n in 1..=50u8 {
        f.submit(u64::from(n), request(n, 4 * GIB, Some(KEY)));
    }
    let running = f.tick();
    assert_eq!(running.len(), 50);
    let mut reruns = Vec::new();
    for start in &running {
        reruns.extend(f.end(start, OOM));
    }
    assert_eq!(reruns.len(), 50);
    assert!(
        reruns.iter().all(|s| s.resources.memory_bytes == 16 * GIB),
        "{:?}",
        reruns
            .iter()
            .map(|s| s.resources.memory_bytes / GIB)
            .collect::<Vec<_>>()
    );
    assert_eq!(f.s.key_floor(KEY), Some(16 * GIB));
    // The key is not disabled: the next 50 digests book the raised rung.
    for n in 51..=100u8 {
        f.submit(u64::from(n), request(n, 4 * GIB, Some(KEY)));
    }
    let next = f.tick();
    assert_eq!(next.len(), 50);
    assert!(next.iter().all(|s| s.resources.memory_bytes == 16 * GIB));
}

/// Catches a key floor at or above the memory of the largest node: the ladder's last
/// step is the whole host (an exclusive lease that runs alone), and a floor there would
/// put every digest of the key on a whole host. On a 16 GiB farm the kill at 4 GiB
/// retries at the cap, 16 GiB, and the key floor stays at 15.75 GiB, so a new digest
/// runs beside another lease. Mutant: the floor is not clamped
/// (`kill_floor(.., u64::MAX)`).
#[test]
fn a_key_floor_stays_below_the_largest_node() {
    let mut f = Farm::new();
    f.node("a", 8, 16);
    f.submit(1, request(1, 4 * GIB, Some(KEY)));
    let first = only(f.tick());
    let rerun = f.killed(&first);
    assert_eq!(rerun.resources.memory_bytes, 16 * GIB, "the digest's cap");
    assert_eq!(f.s.key_floor(KEY), Some(16 * GIB - 256 * MIB));
    // The digest's own retry owns the host; once it ends, a new digest of the key runs
    // beside another lease.
    assert!(f.end(&rerun, ok()).is_empty());
    f.submit(2, request(2, 4 * GIB, Some(KEY)));
    let second = only(f.tick());
    assert_eq!(second.resources.memory_bytes, 16 * GIB - 256 * MIB);
    f.submit(3, request(3, 4 * GIB, None));
    assert_eq!(only(f.tick()).resources.memory_bytes, 4 * GIB);
}

/// Catches the bound taken from the wrong nodes: a node that is gone, one that does not
/// satisfy the platform and one without the CPUs do not count; the largest of the rest
/// does, cordoned or not.
#[test]
fn the_bound_is_the_largest_live_node_that_could_run_it() {
    let mut f = Farm::new();
    let mac = NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
        .unwrap()
        .with_drivers(["native"]);
    f.node("small", 8, 16);
    f.node("medium", 8, 32);
    f.node("huge-few-cpus", 1, 128);
    f.feed(Event::WorkerUp {
        worker: w("mac"),
        instance: DaemonInstance::new("mac"),
        predecessor_stopped: false,
        capacity: Resources::new(8_000, 256 * GIB),
        caps: mac,
    });
    let mut wants = request(1, 4 * GIB, Some(KEY));
    wants.resources = Resources::new(2_000, 4 * GIB);
    assert_eq!(
        f.s.key_floor_bound(&wants),
        32 * GIB,
        "the mac and the 1-CPU node do not count"
    );
    wants.resources = Resources::new(1_000, 4 * GIB);
    assert_eq!(f.s.key_floor_bound(&wants), 128 * GIB);
    // A silent node is not live.
    f.advance(LEASE_GRACE + Duration::from_secs(1));
    f.heartbeat("small", &[]);
    assert_eq!(f.s.key_floor_bound(&wants), 16 * GIB);
    // No node at all: no bound, hence no floor.
    let mut empty = Farm::new();
    assert_eq!(empty.s.key_floor_bound(&wants), 0);
    empty.submit(1, request(1, 4 * GIB, Some(KEY)));
    assert!(empty.tick().is_empty());
}

/// Property: over generated farms (node memories from 6 to 200 GiB, CPUs from 1 to 16)
/// and generated kill chains, a key floor is never at or above the memory of the
/// largest node that matches, so no booking it makes owns a host; and a new digest of
/// the key starts at the floor, at the raised size, or at its own, never above the
/// floor. Mutant: the clamp removed.
#[test]
fn generated_farms_never_book_a_whole_host_from_a_key_floor() {
    let mut rng = SimRng::from_seed(0xe6);
    let mut floors = 0;
    for round in 0..300 {
        let mut f = Farm::new();
        let nodes = 1 + rng.below(3);
        let mut largest = 0;
        for i in 0..nodes {
            let memory = 6 + rng.below(195);
            largest = largest.max(memory);
            f.node(&format!("n{i}"), 1 + rng.below(16), memory);
        }
        f.submit(1, request(1, 4 * GIB, Some(KEY)));
        let mut start = only(f.tick());
        let mut digest = 1;
        // Kill it until the farm has no larger rung, or a few times.
        for _ in 0..rng.below(5) {
            let next = f.end(&start, OOM);
            let Some(next) = next.into_iter().find(|s| s.operation == start.operation) else {
                break;
            };
            start = next;
            if let Some(floor) = f.s.key_floor(KEY) {
                floors += 1;
                assert!(
                    floor < largest * GIB,
                    "round {round}: floor {floor} >= {largest}"
                );
            }
        }
        // Whatever the chain, a fresh digest of the key books under the largest node.
        digest += 1;
        f.submit(u64::from(digest), request(digest, 4 * GIB, Some(KEY)));
        for s in f.tick() {
            if s.operation != start.operation {
                assert!(
                    s.resources.memory_bytes < largest * GIB,
                    "round {round}: {} >= {largest} GiB",
                    s.resources.memory_bytes / GIB
                );
            }
        }
    }
    assert!(floors > 100, "the property is vacuous: {floors} floors");
}

/// Catches a floor raised for the successor to forget: after a leader change the
/// successor, over the same rows, books the raise the first leader's lease ran under
/// (the Start carried 16 GiB; a ledger that says 4 would book the lease's next kill from
/// the wrong rung), and a queued operation is raised at its first round. Mutant:
/// `adopt` does not make the raise again.
#[test]
fn a_successor_books_the_raise_the_lease_ran_under() {
    let mut f = farm_with_floor_16();
    f.submit(2, request(2, 4 * GIB, Some(KEY)));
    f.submit(3, request(3, 4 * GIB, Some(KEY)));
    let running = f.tick();
    assert_eq!(running.len(), 2);
    assert!(running.iter().all(|s| s.resources.memory_bytes == 16 * GIB));
    assert_eq!(f.s.booked(&w("big")).unwrap().memory_bytes, 32 * GIB);

    // What the state store holds: the submitted requests, the lease of the first, the
    // key floor.
    let rows = vec![KeyFloor {
        key: KEY,
        ask: 16 * GIB,
        stamp: 0,
    }];
    let mut successor = Scheduler::new(2);
    successor.restore_key_floors(rows);
    let held = &running[0];
    let recovered = vec![
        Recovered {
            operation: held.operation,
            request: request(2, 4 * GIB, Some(KEY)),
            waiter: WaiterId(2),
            lease: Some(RecoveredLease {
                lease: held.lease,
                worker: w("big"),
            }),
            losses: 0,
            fate: None,
        },
        Recovered {
            operation: running[1].operation,
            request: request(3, 4 * GIB, Some(KEY)),
            waiter: WaiterId(3),
            lease: None,
            losses: 0,
            fate: None,
        },
    ];
    successor.recover(7, OperationId(10), 20, recovered);
    let mut g = Farm::over(successor);
    g.node("big", 8, 64);
    assert_eq!(
        g.s.booked(&w("big")).unwrap().memory_bytes,
        16 * GIB,
        "the adopted lease books what it ran under"
    );
    g.heartbeat("big", &[held]);
    let next = only(g.tick());
    assert_eq!(next.operation, running[1].operation);
    assert_eq!(next.resources.memory_bytes, 16 * GIB);
    assert_eq!(
        g.s.state(held.operation)
            .map(|s| matches!(s, OpState::Leased { .. })),
        Some(true)
    );
}

/// Catches a key floor missing from the value two schedulers are compared by, and one
/// that is not handed to the state store with the requeue: the memory state differs
/// when only a key floor differs; the change names the raised key floor and the evicted
/// keys once; and rows given back to a successor floor a new request.
#[test]
fn key_floors_are_in_the_memory_state_and_the_floor_change() {
    let mut f = farm_with_floor_16();
    let mut same = farm_with_floor_16();
    assert_eq!(f.s.memory_state(), same.s.memory_state());
    // A third scheduler with one more raise differs only by its key floor.
    same.submit(9, request(9, 4 * GIB, Some(KEY + 1)));
    let nine = only(same.tick());
    let again = same.killed(&nine);
    assert_eq!(again.resources.memory_bytes, 16 * GIB);
    assert_ne!(f.s.memory_state(), same.s.memory_state());

    // The change of the first kill of a farm with room for one key floor.
    let mut g = Farm::over(Scheduler::new(1).with_memory_floors(1));
    g.node("big", 8, 64);
    let mut ops = Vec::new();
    for n in 1..=2u8 {
        g.submit(u64::from(n), request(n, 4 * GIB, Some(KEY + u64::from(n))));
        let start = only(g.tick());
        ops.push(start.operation);
        let next = g.killed(&start);
        assert!(g.end(&next, ok()).is_empty());
    }
    let first = g.s.take_floor_change(ops[0]);
    assert_eq!(first.key_raised, None, "key 1 was forgotten");
    assert_eq!(first.key_evicted, vec![KEY + 1]);
    let second = g.s.take_floor_change(ops[1]);
    assert_eq!(
        second.key_raised,
        Some(KeyFloor {
            key: KEY + 2,
            ask: 16 * GIB,
            stamp: 1
        })
    );
    assert_eq!(second.key_evicted, Vec::<u64>::new());
    // Given back, it floors a fresh request.
    let mut successor = Scheduler::new(2);
    successor.restore_key_floors(vec![second.key_raised.expect("raised")]);
    assert_eq!(successor.key_floor(KEY + 2), Some(16 * GIB));
    assert_eq!(successor.key_floor(KEY + 1), None);
    let mut h = Farm::over(successor);
    h.node("big", 8, 64);
    h.submit(1, request(1, 4 * GIB, Some(KEY + 2)));
    assert_eq!(only(h.tick()).resources.memory_bytes, 16 * GIB);
    // Nothing is held for a request that has no key.
    let _ = &mut f;
}

/// Catches a floor that outlives the node that made it possible: the big node leaves
/// while an operation raised to 32 GiB waits, and the only node left has 16. Without
/// the re-clamp the operation would wait for a node that is not coming and be refused
/// "all smaller than its request" after the unservable wait. Mutant: no re-clamp on an
/// unservable verdict.
#[test]
fn an_operation_raised_beyond_the_remaining_nodes_is_lowered_not_refused() {
    let mut f = Farm::new();
    f.node("big", 1, 64);
    // A floor of 32 GiB: killed at 4, then 16.
    f.submit(1, request(1, 4 * GIB, Some(KEY)));
    let first = only(f.tick());
    let second = f.killed(&first);
    let third = f.killed(&second);
    assert_eq!(third.resources.memory_bytes, 32 * GIB);
    assert!(f.end(&third, ok()).is_empty());
    assert_eq!(f.s.key_floor(KEY), Some(32 * GIB));
    // One core: operation 2 holds it, operation 3 waits, raised to 32 GiB.
    f.submit(2, request(2, 4 * GIB, Some(KEY)));
    let held = only(f.tick());
    assert_eq!(held.resources.memory_bytes, 32 * GIB);
    f.submit(3, request(3, 4 * GIB, Some(KEY)));
    assert!(f.tick().is_empty());

    // The big node falls silent; a 16 GiB node joins.
    f.advance(LEASE_GRACE + Duration::from_secs(1));
    f.node("small", 8, 16);
    f.heartbeat("small", &[]);
    let mut moved = Vec::new();
    for _ in 0..3 {
        moved.extend(f.tick());
    }
    // Both operations run on the small node: the one that lost its lease at its
    // booking (32 GiB does not fit 16, so it is lowered too), the waiting one lowered.
    let sizes: Vec<u64> = moved.iter().map(|s| s.resources.memory_bytes).collect();
    assert_eq!(sizes.len(), 2, "{moved:?}");
    for size in sizes {
        assert!(size < 16 * GIB, "{size}");
        assert!(size >= 4 * GIB, "{size}");
    }
    assert!(moved.iter().all(|s| s.worker == w("small")));
}
