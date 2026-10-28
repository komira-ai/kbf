//! `--whole-node-max`: the whole-node settings reach the farm's scheduler, the wall time of
//! a request reaches it with its submit, and a farm that is not given the option places as
//! it did. Time is tokio's paused clock, which the tests move.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use kbf_front::{Cache, Submission};
use kbf_sched::{Blocked, WholeReason, WholeState, WholeWait};
use kbf_types::{LeaseKind, OperationId};

use super::*;
use crate::Args;

type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>;

/// The farm a server builds from `flags`, as `serve` and `frontend` both build it.
fn farm_with(flags: &[&str]) -> Farm {
    let args = Args::parse_from(std::iter::once("kbf-server").chain(flags.iter().copied()));
    let listeners = args.listeners().expect("listeners");
    Farm::from_listeners(Arc::new(Cache::memory()), &listeners)
}

/// A farm of `nodes` workers of four cores, each serving actions and whole-machine leases.
struct Rig {
    farm: Farm,
    nodes: Vec<(WorkerId, StreamId, Inbox)>,
    beats: u64,
}

impl Rig {
    fn new(flags: &[&str], nodes: &[&str]) -> Self {
        let farm = farm_with(flags);
        let nodes = nodes
            .iter()
            .map(|name| {
                let caps = kbf_caps::NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
                    .expect("caps")
                    .with_drivers(["container", "native-whole-machine"]);
                let (outbound, inbox) = mpsc::unbounded_channel();
                let worker = WorkerId::new(*name);
                let stream = farm.register(
                    &worker,
                    DaemonInstance::new(*name),
                    DaemonInstance::new(""),
                    Reported::serving(kbf_types::Resources::new(4_000, 8 << 30), caps),
                    outbound,
                    ServerMessage::default(),
                );
                (worker, stream, inbox)
            })
            .collect();
        Self {
            farm,
            nodes,
            beats: 0,
        }
    }

    /// Submits one request of `kind` named by `key`, with the wall time `wall`, and returns
    /// the operation it is queued as.
    fn submit(&self, key: u8, kind: LeaseKind, wall: Option<Duration>) -> OperationId {
        let operation = self.farm.lock().sched.next_operation();
        self.farm
            .submit(Submission {
                request: kbf_sched::Request {
                    key: kbf_types::ActionKey {
                        instance: "main".to_owned(),
                        action: Digest::new(kbf_types::DigestFunction::Sha256, [key; 32], 1),
                    }
                    .into(),
                    qos: kbf_types::Qos::Ci,
                    kind,
                    resources: kbf_types::Resources::new(1_000, 1 << 30),
                    hermetic: true,
                    do_not_cache: false,
                    needs: kbf_caps::Request::default(),
                },
                call: None,
                described: kbf_front::Described::default(),
                evidence: Default::default(),
                max_wall: wall,
            })
            .expect("submitted");
        operation
    }

    /// The worker that holds the lease of `operation`.
    fn holder(&self, operation: OperationId) -> WorkerId {
        let views = self.farm.lease_views(None);
        let [lease] = views
            .leases
            .iter()
            .filter(|l| l.operation == operation)
            .collect::<Vec<_>>()[..]
        else {
            panic!("{operation:?} does not hold exactly one lease");
        };
        lease.node.clone()
    }

    /// Every worker is heard from, listing the leases it holds, and the scheduler ticks:
    /// the leases stay and a placement round runs at the time now.
    async fn beat(&mut self) {
        self.beats += 1;
        for (worker, stream, _) in &self.nodes {
            let held = self
                .farm
                .lease_views(Some(worker))
                .leases
                .iter()
                .map(|l| l.lease)
                .collect();
            assert!(self.farm.heartbeat(worker, *stream, self.beats, held).await);
        }
        self.farm.tick().await;
    }

    /// Moves the clock on by `by` in steps short enough that no worker is taken for silent.
    async fn pass(&mut self, by: Duration) {
        let step = Duration::from_secs(5);
        let mut left = by;
        while !left.is_zero() {
            let now = left.min(step);
            tokio::time::advance(now).await;
            left -= now;
            self.beat().await;
        }
    }

    fn whole_state(&self, operation: OperationId) -> Option<WholeState> {
        self.farm.lock().sched.whole_state(operation)
    }

    /// Fills each of the first two workers with one action: `long` (a wall time of ten
    /// minutes) is submitted first and so placed on the first worker, `short` (one minute)
    /// next and so on the second, which the order of the assertions pins.
    fn busy(&self) -> (OperationId, OperationId) {
        let long = self.submit(1, LeaseKind::Action, Some(Duration::from_secs(600)));
        let short = self.submit(2, LeaseKind::Action, Some(Duration::from_secs(60)));
        assert_eq!(self.holder(long), self.nodes[0].0, "long on the first");
        assert_eq!(self.holder(short), self.nodes[1].0, "short on the second");
        (long, short)
    }
}

/// Catches a whole-machine request that holds the first worker by name instead of the one
/// that empties soonest (every worker's end bound is the wall time of the actions it runs,
/// which the farm gives the scheduler with each submit), one that holds before
/// `drain_after`, one that is never given up at its deadline, and a give-up that holds the
/// worker just given up, or none. The cheapest worker is the second by name, so a farm that
/// names no wall time (every bound then the same) holds the first and fails here.
#[tokio::test(start_paused = true)]
async fn a_request_holds_the_cheapest_worker_after_drain_after_and_gives_up_at_its_deadline() {
    let mut rig = Rig::new(
        &[
            "--whole-node-max=2",
            "--whole-node-keep-for-ordinary=0",
            "--whole-node-drain-after-secs=30",
        ],
        &["node-a", "node-b"],
    );
    let (_long, _short) = rig.busy();
    let whole = rig.submit(3, LeaseKind::WholeMachine, Some(Duration::from_secs(100)));
    let queued = rig.farm.now();
    assert_eq!(
        rig.whole_state(whole),
        Some(WholeState::Early {
            may_hold_at: FarmTime::from_millis(queued.as_millis() + 30_000)
        })
    );
    rig.pass(Duration::from_secs(25)).await;
    assert!(
        matches!(rig.whole_state(whole), Some(WholeState::Early { .. })),
        "held before drain_after"
    );
    rig.pass(Duration::from_secs(5)).await;
    let Some(WholeState::Holding {
        worker,
        made,
        deadline: Some(deadline),
        give_ups: 0,
        draining: true,
    }) = rig.whole_state(whole)
    else {
        panic!("not holding: {:?}", rig.whole_state(whole));
    };
    assert_eq!(worker, WorkerId::new("node-b"), "the cheapest worker");
    assert_eq!(deadline.as_millis(), made.as_millis() + 100_000);
    // One round before the deadline it still holds node-b; at it, the hold is given up and
    // node-a, the one that is left, is held (node-b is barred to this request).
    rig.pass(Duration::from_secs(95)).await;
    assert!(
        matches!(
            rig.whole_state(whole),
            Some(WholeState::Holding { give_ups: 0, .. })
        ),
        "given up before its deadline"
    );
    rig.pass(Duration::from_secs(10)).await;
    let Some(WholeState::Holding {
        worker, give_ups, ..
    }) = rig.whole_state(whole)
    else {
        panic!("not holding: {:?}", rig.whole_state(whole));
    };
    assert_eq!(worker, WorkerId::new("node-a"), "another worker");
    assert_eq!(give_ups, 1);
    assert_eq!(rig.farm.lock().sched.whole_give_ups(whole), 1);
}

/// Catches a cap that does not reach the scheduler (a second whole-machine request holds
/// a worker too), the wrong cap named for a setting, and a flag that sets another's
/// number: the ceiling at `--whole-node-max`, the workers kept for ordinary work, the
/// workers draining at once.
#[tokio::test(start_paused = true)]
async fn the_caps_stop_a_second_whole_machine_request() {
    let cases: [(&[&str], &[&str], Blocked); 3] = [
        // One whole at most, though three workers could be.
        (
            &["--whole-node-max=1", "--whole-node-keep-for-ordinary=0"],
            &["node-a", "node-b", "node-c"],
            Blocked::Ceiling,
        ),
        // Two workers, one kept: the first request takes the other.
        (
            &["--whole-node-max=2"],
            &["node-a", "node-b"],
            Blocked::KeepForOrdinary,
        ),
        // The first reservation drains a busy worker, and one drain is the most.
        (
            &[
                "--whole-node-max=3",
                "--whole-node-keep-for-ordinary=0",
                "--whole-node-max-draining=1",
            ],
            &["node-a", "node-b", "node-c"],
            Blocked::MaxDraining,
        ),
    ];
    for (flags, nodes, blocked) in cases {
        let rig = Rig::new(flags, nodes);
        for (i, node) in rig.nodes.iter().enumerate() {
            let busy = rig.submit(10 + i as u8, LeaseKind::Action, None);
            assert_eq!(rig.holder(busy), node.0, "{flags:?}");
        }
        let first = rig.submit(1, LeaseKind::WholeMachine, None);
        let second = rig.submit(2, LeaseKind::WholeMachine, None);
        assert!(
            matches!(rig.whole_state(first), Some(WholeState::Holding { .. })),
            "{flags:?}: {:?}",
            rig.whole_state(first)
        );
        assert_eq!(
            rig.whole_state(second),
            Some(WholeState::Stopped(WholeWait::Cap(blocked))),
            "{flags:?}"
        );
        let summary = rig.farm.lock().sched.whole_summary().expect("on");
        assert_eq!(summary.whole, 1, "{flags:?}");
        assert_eq!(
            summary.waiting.get(&WholeReason::Cap(blocked)),
            Some(&1),
            "{flags:?}"
        );
    }
}

/// Catches a wall time that is not given to the scheduler (a deadline of the pool's wall
/// time, which nothing here names, is none at all), one given to the wrong request, and a
/// request with none being held with a deadline: the hold is then never given up unless
/// `--whole-node-drain-deadline-secs` says when.
#[tokio::test(start_paused = true)]
async fn a_requests_wall_time_is_its_deadline() {
    let rig = Rig::new(
        &[
            "--whole-node-max=3",
            "--whole-node-keep-for-ordinary=0",
            "--whole-node-max-draining=3",
        ],
        &["node-a", "node-b", "node-c"],
    );
    for (i, _) in rig.nodes.iter().enumerate() {
        rig.submit(10 + i as u8, LeaseKind::Action, None);
    }
    let walls = [
        Some(Duration::from_secs(100)),
        Some(Duration::from_secs(400)),
        None,
    ];
    let requests: Vec<_> = walls
        .iter()
        .enumerate()
        .map(|(i, wall)| rig.submit(1 + i as u8, LeaseKind::WholeMachine, *wall))
        .collect();
    let deadlines: Vec<_> = requests
        .iter()
        .map(|op| match rig.whole_state(*op) {
            Some(WholeState::Holding {
                made,
                deadline: Some(deadline),
                ..
            }) => (
                deadline.as_millis() - made.as_millis(),
                deadline.as_millis(),
            ),
            other => panic!("{op:?} not holding with a deadline: {other:?}"),
        })
        .collect();
    assert_eq!(deadlines[0].0, 100_000);
    assert_eq!(deadlines[1].0, 400_000);
    assert_eq!(deadlines[2].1, u64::MAX, "no wall time, no deadline");
}

/// Catches a caller that joins a request already queued replacing its wall time with its
/// own (the second caller of one action asks for a shorter hold than the first, which
/// started the operation): the reservation, made after `drain_after`, has the first's.
#[tokio::test(start_paused = true)]
async fn a_caller_that_joins_keeps_the_wall_time_of_the_operation() {
    let mut rig = Rig::new(
        &[
            "--whole-node-max=1",
            "--whole-node-keep-for-ordinary=0",
            "--whole-node-drain-after-secs=10",
        ],
        &["node-a"],
    );
    rig.submit(10, LeaseKind::Action, None);
    let whole = rig.submit(1, LeaseKind::WholeMachine, Some(Duration::from_secs(100)));
    let joined = rig.submit(1, LeaseKind::WholeMachine, Some(Duration::from_secs(5)));
    assert_eq!(
        joined,
        rig.farm.lock().sched.next_operation(),
        "joined none"
    );
    rig.pass(Duration::from_secs(10)).await;
    let Some(WholeState::Holding {
        made,
        deadline: Some(deadline),
        ..
    }) = rig.whole_state(whole)
    else {
        panic!("not holding: {:?}", rig.whole_state(whole));
    };
    assert_eq!(deadline.as_millis() - made.as_millis(), 100_000);
}

/// Catches `--whole-node-drain-deadline-secs` not reaching the scheduler, or overridden by
/// the wall time (it is the deadline of the first reservations; the wall time comes after
/// `--whole-node-max-reserve-attempts` give-ups).
#[tokio::test(start_paused = true)]
async fn the_drain_deadline_comes_before_the_wall_time() {
    let rig = Rig::new(
        &[
            "--whole-node-max=1",
            "--whole-node-keep-for-ordinary=0",
            "--whole-node-drain-deadline-secs=50",
        ],
        &["node-a"],
    );
    rig.submit(10, LeaseKind::Action, None);
    let whole = rig.submit(1, LeaseKind::WholeMachine, Some(Duration::from_secs(400)));
    let Some(WholeState::Holding {
        made,
        deadline: Some(deadline),
        ..
    }) = rig.whole_state(whole)
    else {
        panic!("not holding: {:?}", rig.whole_state(whole));
    };
    assert_eq!(deadline.as_millis() - made.as_millis(), 50_000);
}

/// What a farm shows of one run: the leases and the queue at each step.
async fn run_off(flags: &[&str], walls: bool) -> Vec<String> {
    let mut rig = Rig::new(flags, &["node-a", "node-b"]);
    let wall = |secs| walls.then(|| Duration::from_secs(secs));
    rig.submit(1, LeaseKind::Action, wall(600));
    rig.submit(2, LeaseKind::Action, wall(60));
    let whole = rig.submit(3, LeaseKind::WholeMachine, wall(100));
    rig.submit(4, LeaseKind::Action, wall(30));
    let mut shown = Vec::new();
    for _ in 0..30 {
        rig.pass(Duration::from_secs(10)).await;
        let leases = rig.farm.lease_views(None);
        // The term is a process's own: it differs between two farms of one test.
        let term = leases.term.to_string();
        shown.push(
            format!("{:?} {:?}", leases.leases, rig.farm.queue_view()).replace(&term, "TERM"),
        );
        assert_eq!(rig.whole_state(whole), None);
    }
    assert_eq!(rig.farm.lock().sched.whole_summary(), None);
    shown
}

/// Catches an option that is on without being asked for, and a wall time that changes
/// what a farm with the option off does: a whole-machine request waits for an empty
/// worker, as it always has, and the leases and the queue at every step are the same with
/// the wall times of the requests given and not.
#[tokio::test(start_paused = true)]
async fn off_places_as_it_did_whatever_the_wall_times() {
    let without = run_off(&[], false).await;
    assert_eq!(run_off(&[], true).await, without);
    assert_eq!(run_off(&["--placement-load=off"], true).await, without);
}
