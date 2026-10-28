//! Admission by measured CPU (`Scheduler::with_cpu_admission`): an action is placed on a
//! node only while the node's busy reading is under a share of its CPUs, at most `ramp`
//! per reading, with the ways through that keep it from ever refusing or starving work.
//! Each test names the defect it catches. The fleet-scale behaviour is `f4_fleet`'s
//! `cpu_admit` module.

use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_sched::{CpuAdmission, DaemonInstance, Event, Input, Request, Scheduler, harness};
use kbf_types::{
    ActionKey, Digest, DigestFunction, FarmTime, LeaseKind, Qos, Resources, StateMachine, WaiterId,
    WorkerId,
};

const GIB: u64 = 1 << 30;
/// Heartbeat interval of the rigs, in milliseconds.
const BEAT: u64 = 5_000;
/// A 10-core node: a reading of 5000 is 50 percent.
const CPUS: u64 = 10;

fn digest(n: u64) -> Digest {
    let mut hash = [0u8; 32];
    hash[..8].copy_from_slice(&n.to_be_bytes());
    Digest::new(DigestFunction::Sha256, hash, n)
}

fn node(name: &str) -> WorkerId {
    WorkerId::new(name)
}

fn caps() -> NodeCaps {
    NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("report")
        .with_drivers(["container", "native", "native-whole-machine", "vm"])
}

/// 50 percent of the node's CPUs, 3 leases per reading, aging after 100 s, readings every
/// `BEAT` ms.
fn policy() -> CpuAdmission {
    CpuAdmission::new(50, Duration::from_millis(BEAT))
        .with_ramp(3)
        .with_max_wait(Duration::from_secs(100))
}

/// A scheduler with node `n1` (and `n2` when asked), driven at a farm time the test sets.
struct Rig {
    s: Scheduler,
    ms: u64,
    next: u64,
}

impl Rig {
    fn new(policy: Option<CpuAdmission>) -> Self {
        let s = Scheduler::new(1);
        let mut rig = Self {
            s: policy.map_or(s.clone(), |p| s.with_cpu_admission(p)),
            ms: 0,
            next: 0,
        };
        rig.up("n1", Resources::new(CPUS * 1_000, 64 * GIB));
        rig
    }

    fn feed(&mut self, event: Event) {
        harness::feed(&mut self.s, FarmTime::from_millis(self.ms), event);
    }

    fn up(&mut self, name: &str, capacity: Resources) {
        self.feed(Event::WorkerUp {
            worker: node(name),
            instance: DaemonInstance::new(name),
            predecessor_stopped: false,
            capacity,
            caps: caps(),
        });
    }

    /// Farm time `ms` (never backwards).
    fn at(&mut self, ms: u64) {
        self.ms = ms;
    }

    /// What a daemon's heartbeat is to the server: a reading, then the heartbeat listing
    /// the leases the node holds (a reading is not a sign of life by itself).
    fn busy(&mut self, name: &str, busy_millis: u64) {
        self.feed(Event::Busy {
            worker: node(name),
            busy_millis,
        });
        let running = self.s.leases_on(&node(name));
        self.feed(Event::Heartbeat {
            worker: node(name),
            running,
        });
    }

    /// Queues `count` one-core actions of `kind` with `memory` bytes, then runs a round.
    fn submit_as(&mut self, count: u64, kind: LeaseKind, resources: Resources) {
        for _ in 0..count {
            let n = self.next;
            self.next += 1;
            self.feed(Event::Submit {
                waiter: WaiterId(n),
                request: Request {
                    key: ActionKey {
                        instance: "main".to_owned(),
                        action: digest(n),
                    },
                    qos: Qos::Ci,
                    kind,
                    resources,
                    hermetic: true,
                    do_not_cache: false,
                    needs: kbf_caps::Request::default(),
                },
            });
        }
        self.tick();
    }

    fn submit(&mut self, count: u64) {
        self.submit_as(count, LeaseKind::Action, Resources::new(1_000, GIB));
    }

    fn tick(&mut self) {
        self.feed(Event::Tick);
    }

    fn held(&self, name: &str) -> usize {
        self.s.leases_on(&node(name)).len()
    }

    fn queued(&self) -> usize {
        self.s.queued().count()
    }
}

/// A rig whose node already holds one lease, with a fresh reading `busy_millis` taken at
/// time 0 and nothing admitted since.
fn loaded(busy_millis: u64) -> Rig {
    let mut rig = Rig::new(Some(policy()));
    rig.submit(1);
    assert_eq!(rig.held("n1"), 1, "a node with no lease takes one");
    rig.busy("n1", busy_millis);
    rig
}

/// Catches a feature that is on without being asked for: a scheduler built without
/// `with_cpu_admission` places by bookings alone whatever the readings say (the default of
/// every farm).
#[test]
fn without_the_feature_a_busy_reading_holds_nothing_back() {
    let mut rig = Rig::new(None);
    rig.submit(1);
    rig.busy("n1", CPUS * 1_000);
    rig.submit(6);
    assert_eq!(rig.held("n1"), 7);
}

/// Catches the threshold compared the wrong way or off by one: a reading exactly at 50
/// percent of the node's CPUs (5000 of 10000) admits nothing, and one CPU-millisecond
/// below it admits.
#[test]
fn a_reading_exactly_at_the_threshold_admits_nothing() {
    let mut at = loaded(5_000);
    at.submit(1);
    assert_eq!((at.held("n1"), at.queued()), (1, 1));

    let mut below = loaded(4_999);
    below.submit(1);
    assert_eq!((below.held("n1"), below.queued()), (2, 0));
}

/// Catches a threshold taken of the wrong quantity: it is the node's CPUs (what placement
/// may book, `capacity.cpu_millis`) times the percentage, so a node of 4 CPUs is closed
/// by a reading that leaves a node of 10 CPUs open.
#[test]
fn the_threshold_is_a_share_of_the_nodes_own_cpus() {
    let mut rig = loaded(2_500);
    rig.submit(1);
    assert_eq!(rig.held("n1"), 2, "2500 is a quarter of 10 CPUs");

    let mut small = Rig::new(Some(policy()));
    small.up("n1", Resources::new(4_000, 64 * GIB));
    small.submit(1);
    small.busy("n1", 2_500);
    small.submit(1);
    assert_eq!(
        (small.held("n1"), small.queued()),
        (1, 1),
        "2500 is over half of 4 CPUs"
    );
}

/// Catches a ramp that is off by one in either direction, or that is not counted against
/// the reading: with a ramp of 3 and a reading of 0, the node takes two actions and then a
/// third (R-1 admitted, room), and no fourth (R admitted, none); a new reading starts the
/// count again.
#[test]
fn at_most_ramp_actions_are_admitted_per_reading() {
    let mut rig = loaded(0);
    rig.submit(2);
    assert_eq!(rig.held("n1"), 3, "the lease before the reading, then two");
    rig.submit(1);
    assert_eq!(
        rig.held("n1"),
        4,
        "R-1 admitted since the reading: room for one more"
    );
    rig.submit(1);
    assert_eq!(rig.held("n1"), 4, "R admitted since the reading: none");
    assert_eq!(rig.queued(), 1);

    rig.at(1_000);
    rig.busy("n1", 0);
    rig.tick();
    assert_eq!(rig.held("n1"), 5, "a new reading admits the waiting action");
    rig.submit(5);
    assert_eq!(rig.held("n1"), 7, "and two more: three in all since it");
}

/// Catches a ramp that counts only the grants of one placement round: three actions placed
/// in each of three rounds between two readings are the node's nine against a ramp of 3.
#[test]
fn the_ramp_counts_across_rounds_between_readings() {
    let mut rig = loaded(0);
    for _ in 0..3 {
        rig.submit(1);
    }
    assert_eq!(rig.held("n1"), 4);
    rig.submit(1);
    assert_eq!(rig.held("n1"), 4);
}

/// Catches a stale reading that still holds work back, and a staleness limit off by one:
/// a reading of a full node is current for exactly three heartbeat intervals and stale
/// after; and a node never heard from (no reading yet) is admitted to as without the
/// feature, a daemon that sends none being unknown and not busy.
#[test]
fn a_reading_older_than_three_intervals_is_ignored() {
    let mut rig = loaded(CPUS * 1_000);
    rig.at(3 * BEAT);
    rig.submit(1);
    assert_eq!(
        (rig.held("n1"), rig.queued()),
        (1, 1),
        "three intervals old: current"
    );
    rig.at(3 * BEAT + 1);
    rig.tick();
    assert_eq!(
        (rig.held("n1"), rig.queued()),
        (2, 0),
        "older than three: fail open"
    );
}

/// Catches fail-open missing for a node without a reading, and a reading that outlives its
/// session: a daemon that registers again measures its own windows, so the earlier
/// process's reading is dropped.
#[test]
fn no_reading_is_admitted_to_as_without_the_feature() {
    let mut rig = Rig::new(Some(policy()));
    rig.submit(8);
    assert_eq!(
        rig.held("n1"),
        8,
        "no reading: the ramp does not apply either"
    );

    let mut again = loaded(CPUS * 1_000);
    again.submit(1);
    assert_eq!(again.queued(), 1);
    again.up("n1", Resources::new(CPUS * 1_000, 64 * GIB));
    again.tick();
    assert_eq!(again.queued(), 0, "the new session has no reading yet");
}

/// Catches a node with no lease held back by a reading: it cannot be oversubscribed by
/// work it does not have, and the reading may be the last process's. The first action
/// gets in whatever the node's reading says, and the next waits.
#[test]
fn a_node_with_no_lease_takes_one_whatever_its_reading() {
    let mut rig = Rig::new(Some(policy()));
    rig.busy("n1", CPUS * 1_000);
    rig.submit(2);
    assert_eq!((rig.held("n1"), rig.queued()), (1, 1));
}

/// Catches an action queued past `max_wait` that still waits (starving behind a busy
/// node), and an aging boundary off by one: the action is held while it has waited less
/// than 100 s, and placed at 100 s whatever the reading, with a fresh full reading all the
/// while.
#[test]
fn an_action_queued_for_max_wait_is_admitted_whatever_the_reading() {
    let mut rig = loaded(CPUS * 1_000);
    rig.submit(1);
    assert_eq!(rig.queued(), 1);
    for t in [20_000, 60_000, 99_999] {
        rig.at(t);
        rig.busy("n1", CPUS * 1_000);
        rig.tick();
        assert_eq!(rig.queued(), 1, "waited {t} ms");
    }
    rig.at(100_000);
    rig.busy("n1", CPUS * 1_000);
    rig.tick();
    assert_eq!((rig.held("n1"), rig.queued()), (2, 0));
}

/// Catches aging measured from the wrong moment: from the node's reading or the first
/// placement round instead of from when the action was queued. An action queued at 50 s
/// has waited 50 s at 100 s, and an earlier one has waited the full 100.
#[test]
fn aging_counts_from_when_each_action_was_queued() {
    let mut rig = loaded(CPUS * 1_000);
    rig.submit(1);
    rig.at(50_000);
    rig.busy("n1", CPUS * 1_000);
    rig.submit(1);
    assert_eq!(rig.queued(), 2);
    rig.at(100_000);
    rig.busy("n1", CPUS * 1_000);
    rig.tick();
    assert_eq!(
        (rig.held("n1"), rig.queued()),
        (2, 1),
        "only the older one aged"
    );
}

/// Catches the clause applied to the leases it is not for: a VM books its own memory and
/// CPUs and is placed by its booking alone, even on a node whose reading is full. And
/// that it holds an action only where the node is busy: the same node takes the VM and
/// keeps the action waiting.
#[test]
fn a_vm_is_not_held_back_by_the_reading() {
    let mut rig = Rig::new(Some(policy()));
    rig.up("n1", Resources::new(CPUS * 1_000, 64 * GIB).with_vms(2));
    rig.submit(1);
    rig.busy("n1", CPUS * 1_000);
    rig.submit(1);
    assert_eq!(rig.queued(), 1, "the action waits");
    rig.submit_as(1, LeaseKind::Vm, Resources::new(2_000, 8 * GIB).with_vms(1));
    assert_eq!(rig.held("n1"), 2, "the VM is placed");
}

/// Catches a whole-machine lease held back by the reading: it books an empty node whole,
/// and the clause is not part of its placement.
#[test]
fn a_whole_machine_lease_is_not_held_back_by_the_reading() {
    let mut rig = Rig::new(Some(policy()));
    rig.busy("n1", CPUS * 1_000);
    rig.submit_as(
        1,
        LeaseKind::WholeMachine,
        Resources::new(CPUS * 1_000, 64 * GIB),
    );
    assert_eq!((rig.held("n1"), rig.queued()), (1, 0));
}

/// Catches an action at a node's whole memory (placed only on a node with no lease) held
/// back by the reading of the node's earlier process.
#[test]
fn an_action_at_the_whole_hosts_memory_is_not_held_back() {
    let mut rig = Rig::new(Some(policy()));
    rig.busy("n1", CPUS * 1_000);
    rig.submit_as(1, LeaseKind::Action, Resources::new(1_000, 64 * GIB));
    assert_eq!((rig.held("n1"), rig.queued()), (1, 0));
}

/// Catches a held-back node not skipped in favour of another that has room: with one busy
/// and one idle node, the action goes to the idle one although the busy one is the less
/// loaded by bookings.
#[test]
fn a_busy_node_is_skipped_for_one_that_is_not() {
    let mut rig = Rig::new(Some(policy()));
    rig.up("n2", Resources::new(CPUS * 1_000, 64 * GIB));
    // n1 holds nothing booked of three; n2 holds more. By load n1 is first.
    rig.submit(4);
    assert_eq!(rig.held("n1") + rig.held("n2"), 4);
    rig.busy("n1", CPUS * 1_000);
    rig.busy("n2", 0);
    let (n1, n2) = (rig.held("n1"), rig.held("n2"));
    rig.submit(2);
    assert_eq!(rig.held("n1"), n1, "the busy node takes none");
    assert_eq!(rig.held("n2"), n2 + 2);
}

/// Catches a reading for a node that never registered failing the scheduler or creating
/// the node.
#[test]
fn a_reading_for_an_unknown_node_is_ignored() {
    let mut rig = Rig::new(Some(policy()));
    rig.busy("ghost", 5);
    rig.submit(1);
    assert_eq!(rig.held("n1"), 1);
    assert_eq!(rig.s.leases_on(&node("ghost")).len(), 0);
}

/// Catches work refused for waiting on a busy node: the clause delays and never refuses,
/// so an action that waits on one past the unservable wait is still queued.
#[test]
fn waiting_on_a_busy_node_is_never_a_refusal() {
    let mut rig = Rig::new(Some(policy().with_max_wait(Duration::from_secs(100_000))));
    rig.submit(1);
    rig.busy("n1", CPUS * 1_000);
    rig.submit(1);
    for step in 1..=10u64 {
        rig.at(step * BEAT * 60);
        rig.busy("n1", CPUS * 1_000);
        rig.tick();
    }
    assert_eq!((rig.held("n1"), rig.queued()), (1, 1));
    let effects = rig
        .s
        .apply(Input::new(FarmTime::from_millis(rig.ms), Event::Tick));
    assert!(effects.is_empty(), "{effects:?}");
}

/// Catches VM placements counted against the action ramp: with a ramp of 3, two VMs placed
/// after a reading leave all three actions their room.
#[test]
fn vms_placed_since_a_reading_do_not_use_the_ramp() {
    let mut rig = Rig::new(Some(policy()));
    rig.up("n1", Resources::new(CPUS * 1_000, 64 * GIB).with_vms(4));
    rig.submit(1);
    rig.busy("n1", 0);
    rig.submit_as(2, LeaseKind::Vm, Resources::new(1_000, 4 * GIB).with_vms(1));
    assert_eq!(rig.held("n1"), 3);
    rig.submit(4);
    assert_eq!((rig.held("n1"), rig.queued()), (6, 1));
}

/// Catches a ramp of 0 admitting nothing for good (every reading would hold the node shut
/// however idle it is): it is taken as 1, so an idle node still takes one action per reading.
#[test]
fn a_ramp_of_zero_is_one() {
    let mut rig = Rig::new(Some(policy().with_ramp(0)));
    rig.submit(1);
    rig.busy("n1", 0);
    rig.submit(3);
    assert_eq!((rig.held("n1"), rig.queued()), (2, 2));
}
