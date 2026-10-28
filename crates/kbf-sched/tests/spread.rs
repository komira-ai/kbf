//! Placement spreads load: a queued action goes to the worker with the least booked, as a
//! share of its capacity (the larger of its CPU share and its memory share), then the one
//! holding the fewest leases, then the first by name; not to the first worker in name
//! order that has room. The shape is a real farm's: one node that offers half the cores
//! of the others sorts first, and the work arrives far below the farm's capacity.
//!
//! First fit gave the first worker every lease the farm's demand left over: with 305
//! cores and a mean of about 26 concurrent leases, the node named first held most of
//! them and ran at several times its cores while the others were lightly used.

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, Input, Request, Scheduler};
use kbf_types::{
    ControlRecord, Digest, DigestFunction, Effect, FarmTime, LeaseGrant, LeaseKind, Outcome, Qos,
    Resources, StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;
/// Four nodes, in cores: the first, which sorts first, offers half the others.
const FLEET: [(&str, u64); 4] = [("n1", 44), ("n2", 87), ("n3", 87), ("n4", 87)];
const NODE_MEMORY: u64 = 223 * GIB;
/// An action of one core and 1 GiB: on a node of [`NODE_MEMORY`], its cores are the larger
/// share of what it books.
const SMALL: Resources = Resources::new(1_000, GIB);

fn digest(n: u32) -> Digest {
    let mut hash = [0u8; 32];
    hash[..4].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, u64::from(n))
}

fn linux() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .unwrap()
        .with_drivers(["container"])
}

fn mac() -> NodeCaps {
    NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
        .unwrap()
        .with_drivers(["native"])
}

/// Action `n` with `resources`, needing what the REAPI `platform` asks for.
fn request(n: u32, resources: Resources, platform: &[(&str, &str)]) -> Request {
    Request {
        key: kbf_types::work_key("main", digest(n)),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources,
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::from_platform(platform.iter().copied()).unwrap(),
    }
}

fn w(name: &str) -> WorkerId {
    WorkerId::new(name)
}

/// A scheduler at a fixed farm time: nothing here waits on a grace period.
struct Harness {
    s: Scheduler,
    /// The number of requests submitted so far: the next one's digest and waiter.
    submitted: u32,
    /// The workers registered.
    names: Vec<String>,
}

impl Harness {
    fn new() -> Self {
        Self {
            s: Scheduler::new(1),
            submitted: 0,
            names: Vec::new(),
        }
    }

    /// A farm of Linux workers `fleet` (name, cores), each with `memory`.
    fn linux(fleet: &[(&str, u64)], memory: u64) -> Self {
        let mut h = Self::new();
        for &(name, cores) in fleet {
            h.worker(name, cores, memory, linux());
        }
        h
    }

    /// Registers `name` with `cores` and `memory`.
    fn worker(&mut self, name: &str, cores: u64, memory: u64, caps: NodeCaps) {
        self.names.push(name.to_owned());
        self.feed(Event::WorkerUp {
            worker: w(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity: Resources::new(cores * 1_000, memory),
            caps,
        });
    }

    fn feed(&mut self, event: Event) -> Vec<Effect> {
        self.s
            .apply(Input::new(FarmTime::from_millis(1_000), event))
    }

    /// Submits `count` actions of `resources`, as the front asks.
    fn submit_many(&mut self, count: u32, resources: Resources) {
        for _ in 0..count {
            self.submit(resources, &[]);
        }
    }

    /// Submits one action of `resources` for `platform`.
    fn submit(&mut self, resources: Resources, platform: &[(&str, &str)]) {
        let waiter = WaiterId(u64::from(self.submitted));
        let request = request(self.submitted, resources, platform);
        self.submitted += 1;
        assert!(self.feed(Event::Submit { waiter, request }).is_empty());
    }

    /// Ticks; the grants proposed.
    fn tick(&mut self) -> Vec<LeaseGrant> {
        self.feed(Event::Tick)
            .into_iter()
            .filter_map(|effect| match effect {
                Effect::Commit(ControlRecord::Lease(grant)) => Some(grant),
                Effect::Waiting(_) => None,
                other => panic!("a tick proposed {other:?}"),
            })
            .collect()
    }

    /// Ticks after a submission of `resources`; the one worker that was chosen.
    fn chosen(&mut self, resources: Resources) -> String {
        self.submit(resources, &[]);
        let [grant] = self.tick().try_into().expect("one grant");
        grant.worker.as_str().to_owned()
    }

    /// Books `resources` on `worker` whatever placement would choose: every other worker
    /// is cordoned while the action is placed, so a test's setup does not depend on the
    /// choice it tests.
    fn place_on(&mut self, worker: &str, resources: Resources) -> LeaseGrant {
        let others: Vec<String> = self
            .names
            .iter()
            .filter(|n| *n != worker)
            .cloned()
            .collect();
        for other in &others {
            self.feed(Event::Cordon { worker: w(other) });
        }
        self.submit(resources, &[]);
        let [grant] = self.tick().try_into().expect("one grant");
        for other in &others {
            let placed = self.feed(Event::Uncordon { worker: w(other) });
            assert!(placed.is_empty(), "{placed:?}");
        }
        assert_eq!(grant.worker, w(worker));
        grant
    }

    /// Commits `grant` and reports it started.
    fn start(&mut self, grant: &LeaseGrant) {
        let effects = self.feed(Event::Committed(ControlRecord::Lease(grant.clone())));
        assert!(
            matches!(effects.as_slice(), [Effect::Start(_)]),
            "{effects:?}"
        );
        self.feed(Event::Started {
            operation: grant.operation,
            lease: grant.lease,
        });
    }

    /// Reports `grant` completed and commits the result: its lease ends.
    fn finish(&mut self, grant: &LeaseGrant) {
        let proposed = self.feed(Event::Report {
            operation: grant.operation,
            lease: grant.lease,
            outcome: Outcome::Completed {
                action_result: digest(u32::MAX).into(),
            },
        });
        let [Effect::Commit(record)] = proposed.as_slice() else {
            panic!("{proposed:?}");
        };
        let answered = self.feed(Event::Committed(record.clone()));
        assert!(
            matches!(answered.as_slice(), [Effect::Answer(_)]),
            "{answered:?}"
        );
    }

    /// How many leases `name` holds.
    fn held(&self, name: &str) -> u64 {
        u64::try_from(self.s.leases_on(&w(name)).len()).unwrap()
    }
}

/// Asserts that, of any two workers of `fleet`, the first holds a share of its cores
/// that is at most the second's plus one lease of its own: what a least-booked choice
/// leaves when no lease has ended. `held(a) / cores(a) <= held(b) / cores(b) +
/// 1 / cores(a)`, multiplied out.
fn assert_loaded_alike(h: &Harness, fleet: &[(&str, u64)]) {
    for &(a, cores_a) in fleet {
        for &(b, cores_b) in fleet {
            let (held_a, held_b) = (h.held(a), h.held(b));
            assert!(
                held_a * cores_b <= held_b * cores_a + cores_b,
                "{a} holds {held_a} of {cores_a} cores but {b} only {held_b} of {cores_b}"
            );
        }
    }
}

/// Catches: first fit in name order, which filled the node named first to its 44 cores
/// and the next to 87 before the third held any, while the farm had 305 free cores. A
/// burst of 130 one-core actions must leave every node at the same share of its cores,
/// give or take one lease, so the node that offers 44 cores holds a share of the leases
/// in proportion (about 19 of 130), not all it can.
#[test]
fn a_burst_loads_every_worker_alike() {
    let mut h = Harness::linux(&FLEET, NODE_MEMORY);
    h.submit_many(130, SMALL);
    assert_eq!(h.tick().len(), 130);

    assert_loaded_alike(&h, &FLEET);
    let first = h.held("n1");
    assert!(
        first <= 20,
        "the node offering the fewest cores holds {first} of 130 leases"
    );
}

/// Catches: first fit at the load the farm mostly has. One action at a time, never
/// finishing, the node named first held all 13 of the first 13 leases; the least-booked
/// choice gives it 2, then lets the larger nodes pass it as it fills.
#[test]
fn light_load_is_not_concentrated_either() {
    let mut h = Harness::linux(&FLEET, NODE_MEMORY);
    for round in 0..13 {
        h.submit_many(1, SMALL);
        assert_eq!(h.tick().len(), 1);
        assert_loaded_alike(&h, &FLEET);
        let first = h.held("n1");
        assert!(
            first <= 3,
            "round {round}: the node offering the fewest cores holds {first} leases"
        );
    }
}

/// Catches: `N` equal workers and `M` leases not split `M / N` give or take one: first
/// fit (all of the first worker's cores, then the next's), and a spread that skips a
/// worker. With fewer leases than workers, each lease has a worker of its own.
#[test]
fn equal_workers_split_the_leases_to_within_one() {
    for (workers, leases) in [(2, 5), (3, 7), (4, 10), (5, 12), (6, 3), (4, 2)] {
        let fleet: Vec<(String, u64)> = (0..workers).map(|i| (format!("w{i}"), 8)).collect();
        let fleet: Vec<(&str, u64)> = fleet.iter().map(|(n, c)| (n.as_str(), *c)).collect();
        let mut h = Harness::linux(&fleet, 16 * GIB);
        h.submit_many(leases, SMALL);
        assert_eq!(h.tick().len(), leases as usize);

        let held: Vec<u64> = fleet.iter().map(|(name, _)| h.held(name)).collect();
        let (low, high) = (
            u64::from(leases) / workers,
            u64::from(leases).div_ceil(workers),
        );
        assert!(
            held.iter().all(|&n| (low..=high).contains(&n)),
            "{leases} leases on {workers} equal workers: {held:?}"
        );
    }
}

/// Catches: a spread that ignores the cores (by lease count, 3 and 3 of six leases on a
/// 2-core and a 4-core worker, the small one over-booked), and one by absolute free cores
/// (the large worker takes everything until it is as free as the small one). A worker with
/// half the cores takes half the leases.
#[test]
fn a_worker_with_half_the_cores_takes_half_the_leases() {
    for (leases, small, large) in [(3, 1, 2), (6, 2, 4)] {
        let mut h = Harness::linux(&[("a", 2), ("b", 4)], 16 * GIB);
        h.submit_many(leases, SMALL);
        assert_eq!(h.tick().len(), leases as usize);
        assert_eq!(
            (h.held("a"), h.held("b")),
            (small, large),
            "{leases} leases on a 2-core worker named first and a 4-core one"
        );
    }
}

/// Catches: a spread that reads CPUs only. Two workers of 8 cores and 16 GiB: an action
/// of 4 cores and 12 GiB goes to the first (a tie), one of 4 cores and 1 GiB to the other
/// (nothing is booked there), and a third, with both workers at half their cores, to the
/// one with the memory free. First fit puts the second beside the first; a CPU-only
/// spread puts the third there (a tie in cores, then the name), where 12 of 16 GiB are
/// booked, and the daemon's container runs out of memory.
#[test]
fn a_worker_with_less_memory_free_gets_fewer_leases() {
    let mut h = Harness::linux(&[("a", 8), ("b", 8)], 16 * GIB);
    assert_eq!(h.chosen(Resources::new(4_000, 12 * GIB)), "a");
    assert_eq!(h.chosen(Resources::new(4_000, GIB)), "b");
    assert_eq!(h.chosen(SMALL), "b", "b has the memory");
}

/// Catches: a memory share that stops at the worker's memory. Admission books no memory,
/// so a worker holds leases whose memory adds up to more than it has: two workers of
/// 8 cores and 16 GiB hold four leases each, of 6 GiB on one and 4 GiB on the other (24
/// and 16 GiB of 16, both over; the cores are 4 of 8 on each). The next action goes to the
/// one that is less over: a share clamped to 1 sees a tie, and the name decides.
#[test]
fn memory_booked_past_the_worker_still_counts() {
    let mut h = Harness::linux(&[("a", 8), ("b", 8)], 16 * GIB);
    for _ in 0..4 {
        h.place_on("a", Resources::new(1_000, 6 * GIB));
        h.place_on("b", Resources::new(1_000, 4 * GIB));
    }
    assert_eq!(h.chosen(SMALL), "b");
}

/// Catches: a tie in the shares decided by the name alone. Worker `a` (8 cores) holds two
/// 2-core leases and `b` (4 cores) one: both are half booked, and the next action goes to
/// `b`, which holds fewer leases, though `a` is first by name.
#[test]
fn equal_shares_go_to_the_worker_with_fewer_leases() {
    let mut h = Harness::linux(&[("a", 8), ("b", 4)], 16 * GIB);
    h.place_on("a", Resources::new(2_000, GIB));
    h.place_on("a", Resources::new(2_000, GIB));
    h.place_on("b", Resources::new(2_000, GIB));
    assert_eq!(h.chosen(SMALL), "b");
}

/// Catches: a choice of the least booked worker that does not look at room: the action
/// waits, or is placed where it does not fit. `a` (4 cores, one booked) has a smaller
/// share than `b` (16 cores, six booked), but not the 4 cores the action asks; it goes to
/// `b`.
#[test]
fn the_least_booked_worker_is_chosen_among_those_with_room() {
    let mut h = Harness::linux(&[("a", 4), ("b", 16)], 64 * GIB);
    h.place_on("a", SMALL);
    for _ in 0..6 {
        h.place_on("b", SMALL);
    }
    assert_eq!(h.chosen(Resources::new(4_000, GIB)), "b");
}

/// Catches: spreading that strands capacity, a choice that skips a worker with room or
/// takes one without. 400 one-core actions against 305 cores: all 305 run, each node is
/// booked to its last core, and the rest wait.
#[test]
fn spreading_does_not_strand_capacity() {
    let mut h = Harness::linux(&FLEET, NODE_MEMORY);
    h.submit_many(400, SMALL);
    let granted = h.tick().len() + h.tick().len();
    assert_eq!(granted, 305, "the cores of the farm");

    for &(name, cores) in &FLEET {
        assert_eq!(
            h.s.booked(&w(name)).map(|booked| booked.cpu_millis),
            Some(cores * 1_000),
            "{name} is booked to its capacity"
        );
    }
    assert_eq!(h.s.queued().count(), 95);
}

/// Catches: a platform filter applied after the spread. The Mac is named first and holds
/// nothing, so it is the least booked of the three; Linux actions still go to the two
/// Linux workers, which split them, and the Mac's own go to the Mac.
#[test]
fn the_platform_is_matched_before_the_spread() {
    let mut h = Harness::new();
    h.worker("a-mac", 4, 8 * GIB, mac());
    h.worker("b-linux", 4, 8 * GIB, linux());
    h.worker("c-linux", 4, 8 * GIB, linux());
    for _ in 0..4 {
        h.submit(SMALL, &[("OSFamily", "linux")]);
    }
    assert_eq!(h.tick().len(), 4);
    assert_eq!(
        (h.held("a-mac"), h.held("b-linux"), h.held("c-linux")),
        (0, 2, 2)
    );

    h.submit(SMALL, &[("OSFamily", "Darwin")]);
    let [grant] = h.tick().try_into().expect("one grant");
    assert_eq!(grant.worker, w("a-mac"));
}

/// The contract of an action at a worker's whole memory (the last step of the memory
/// ladder): it waits while every node has a lease, and nothing reserves a node for it, so
/// work behind it is placed on the nodes; it runs, alone, once a node has emptied.
/// Two nodes of 4 cores hold two small leases each (placed by cordoning the other node,
/// so the setup does not depend on how placement chooses); a whole-host action is
/// queued, then a small one.
///
/// Catches: a hold for whole-memory actions. With a node held, the small action cannot go
/// to `a` (which the tie in load gives it, being first by name) and goes to `b`.
#[test]
fn a_whole_memory_action_waits_for_a_node_to_empty_and_holds_none() {
    let mut h = Harness::linux(&[("a", 4), ("b", 4)], 16 * GIB);
    let mut running = Vec::new();
    for node in ["a", "b", "a", "b"] {
        running.push(h.place_on(node, Resources::new(1_000, 4 * GIB)));
    }
    assert_eq!((h.held("a"), h.held("b")), (2, 2));
    h.submit(Resources::new(1_000, 16 * GIB), &[]);
    let [host] = h.s.queued().collect::<Vec<_>>().try_into().unwrap();
    assert!(h.tick().is_empty(), "every node has leases");

    h.submit(Resources::new(1_000, 4 * GIB), &[]);
    let [behind] = h.tick().try_into().expect("the work behind it runs");
    assert_eq!(behind.worker, w("a"), "no node is held back from it");
    running.push(behind);
    for grant in &running {
        h.start(grant);
    }
    assert!(h.tick().is_empty(), "both nodes still have leases");

    for grant in running.iter().filter(|grant| grant.worker == w("a")) {
        h.finish(grant);
    }
    let [grant] = h.tick().try_into().expect("one grant");
    assert_eq!((grant.operation, grant.worker), (host, w("a")));
    assert_eq!(h.held("a"), 1, "the whole-host action runs alone");
}

/// A known limit, which `docs/design/scheduler.md` states (issue #169): an action that
/// asks for most of a worker's cores holds no worker, and spreading leaves no worker
/// with the cores. Eight one-core leases on two workers of eight cores split four and
/// four, so a six-core action waits; first fit packed all eight on the first worker and
/// ran it on the second at once. Catches: a change that makes it run (or hold) without
/// the doc following.
#[test]
fn a_large_cpu_action_waits_beside_leases_spread_over_the_workers() {
    let mut h = Harness::linux(&[("a", 8), ("b", 8)], 16 * GIB);
    for _ in 0..8 {
        h.chosen(Resources::new(1_000, GIB));
    }
    assert_eq!((h.held("a"), h.held("b")), (4, 4));
    h.submit(Resources::new(6_000, 2 * GIB), &[]);
    assert!(h.tick().is_empty(), "neither worker has six cores free");
}
