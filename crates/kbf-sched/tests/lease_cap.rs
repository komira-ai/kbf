//! A node's lease cap (`Resources::leases`, filled from the `action_slots` entry of its
//! node report): the scheduler holds no more concurrent Action leases on it than the cap,
//! whatever its CPUs would allow, and a node without a cap is placed by CPUs alone.

use kbf_caps::NodeCaps;
use kbf_sched::{DaemonInstance, Event, Input, Request, Scheduler};
use kbf_types::{
    Digest, DigestFunction, Effect, FarmTime, LeaseGrant, LeaseKind, Outcome, Qos, Resources,
    StateMachine, WaiterId, WorkerId,
};

const GIB: u64 = 1 << 30;

fn digest(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

fn node() -> WorkerId {
    WorkerId::new("mac")
}

/// A scheduler with one 8-core Mac, and the leases it has started and not finished.
struct Farm {
    s: Scheduler,
    secs: u64,
    next: u64,
    running: Vec<LeaseGrant>,
}

impl Farm {
    fn new(cap: Option<u64>) -> Self {
        let mut farm = Self {
            s: Scheduler::new(1),
            secs: 0,
            next: 0,
            running: Vec::new(),
        };
        farm.feed(Event::WorkerUp {
            worker: node(),
            instance: DaemonInstance::new("mac"),
            predecessor_stopped: false,
            capacity: capacity(cap),
            caps: NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
                .unwrap()
                .with_drivers(["native", "native-whole-machine"]),
        });
        farm
    }

    /// Feeds `event`, commits what is proposed and records the leases started.
    fn feed(&mut self, event: Event) {
        let now = FarmTime::from_millis(self.secs * 1_000);
        let mut effects = self.s.apply(Input::new(now, event));
        while !effects.is_empty() {
            let mut more = Vec::new();
            for effect in effects {
                match effect {
                    Effect::Commit(record) => {
                        more.extend(self.s.apply(Input::new(now, Event::Committed(record))));
                    }
                    Effect::Start(start) => self.running.push(LeaseGrant {
                        lease: start.lease,
                        operation: start.operation,
                        worker: start.worker,
                    }),
                    _ => {}
                }
            }
            effects = more;
        }
    }

    fn submit(&mut self, count: u64) {
        self.submit_kind(count, LeaseKind::Action, Resources::new(1_000, GIB));
    }

    fn submit_kind(&mut self, count: u64, kind: LeaseKind, resources: Resources) {
        for _ in 0..count {
            let n = self.next;
            self.next += 1;
            let request = Request {
                key: kbf_sim::work_key("main", digest(n)),
                qos: Qos::Ci,
                kind,
                resources,
                hermetic: true,
                do_not_cache: false,
                needs: kbf_caps::Request::default(),
            };
            self.feed(Event::Submit {
                waiter: WaiterId(n),
                request,
            });
        }
        self.secs += 1;
        self.feed(Event::Tick);
    }

    /// The node's capacity becomes `cap` (its report changed on the same stream).
    fn report(&mut self, cap: Option<u64>) {
        self.feed(Event::Capacity {
            worker: node(),
            capacity: capacity(cap),
            caps: NodeCaps::from_report([("arch", "arm64"), ("os", "macos")])
                .unwrap()
                .with_drivers(["native", "native-whole-machine"]),
        });
        self.secs += 1;
        self.feed(Event::Tick);
    }

    /// The oldest running lease finishes.
    fn finish_one(&mut self) {
        let done = self.running.remove(0);
        self.feed(Event::Report {
            operation: done.operation,
            lease: done.lease,
            outcome: Outcome::Completed {
                action_result: digest(1_000_000 + done.operation.0),
            },
        });
        self.secs += 1;
        self.feed(Event::Tick);
    }

    fn held(&self) -> usize {
        self.s.leases_on(&node()).len()
    }
}

fn capacity(cap: Option<u64>) -> Resources {
    let base = Resources::new(8_000, 32 * GIB);
    cap.map_or(base, |slots| base.with_lease_cap(slots))
}

/// Catches: a cap that placement ignores (a Mac with two pool users given as many
/// leases as it has cores, the extras waiting on the node), and a cap that is
/// off by one (a third lease granted, or only one). A finished lease must free its slot,
/// so queued work still runs to the end.
#[test]
fn a_node_with_two_slots_never_runs_a_third_action_lease() {
    let mut farm = Farm::new(Some(2));
    farm.submit(6);
    assert_eq!(farm.held(), 2);
    assert_eq!(farm.s.queued().count(), 4);
    for left in (0..4).rev() {
        farm.finish_one();
        assert_eq!(farm.held(), 2, "a slot freed is filled, never exceeded");
        assert_eq!(farm.s.queued().count(), left);
    }
    assert_eq!(farm.held(), 2);
    farm.finish_one();
    assert_eq!(farm.held(), 1);
    farm.finish_one();
    assert_eq!(farm.held(), 0);
}

/// Catches: a lease cap that affects a node reporting none, which would then run fewer
/// leases than its cores allow.
#[test]
fn a_node_without_a_cap_is_placed_by_cpus_alone() {
    let mut farm = Farm::new(None);
    farm.submit(12);
    assert_eq!(farm.held(), 8);
    assert_eq!(farm.s.queued().count(), 4);
}

/// Catches: a lowered cap that cancels running leases (the node is told to stop work
/// that is fine), one that stops nothing (new leases still placed past it), and a raised
/// cap that is not applied to the next placement. A cap of zero stops all new leases and
/// is not read as no cap.
#[test]
fn a_lowered_cap_stops_new_placements_and_keeps_running_leases() {
    let mut farm = Farm::new(Some(4));
    farm.submit(8);
    assert_eq!(farm.held(), 4);

    // Slots dirtied: two left. Four run on; nothing new is placed until under the cap.
    farm.report(Some(2));
    assert_eq!(farm.held(), 4, "no running lease is evicted");
    farm.finish_one();
    farm.finish_one();
    assert_eq!(farm.held(), 2, "still at the cap: the queue waits");
    farm.finish_one();
    assert_eq!(farm.held(), 2, "one below the cap, one placed");

    // All dirty: running leases stay, nothing new starts.
    farm.report(Some(0));
    assert_eq!(farm.held(), 2);
    farm.finish_one();
    farm.finish_one();
    assert_eq!(farm.held(), 0);
    assert!(farm.s.queued().count() > 0, "zero slots is not no cap");

    // Cleaned: three slots again.
    farm.report(Some(3));
    assert_eq!(farm.held(), 3);
}

/// Catches: the cap applied to a whole-machine lease (which needs an empty node, not a
/// free slot, so a capped node would refuse it), or a whole-machine lease that lets
/// Actions in beside it.
#[test]
fn a_whole_machine_lease_is_placed_on_an_empty_capped_node_and_blocks_actions() {
    let mut farm = Farm::new(Some(2));
    farm.submit_kind(1, LeaseKind::WholeMachine, Resources::new(8_000, 32 * GIB));
    assert_eq!(farm.held(), 1);
    farm.submit(4);
    assert_eq!(farm.held(), 1, "no action beside a whole-machine lease");
    farm.finish_one();
    assert_eq!(
        farm.held(),
        2,
        "the actions follow, up to the cap, not to the cores"
    );
    assert_eq!(farm.s.queued().count(), 2);
}
