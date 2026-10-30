//! `--whole-node-max`: the whole-node settings reach the farm's scheduler, the wall time of
//! a request reaches it with its submit, and a farm that is not given the option places as
//! it did. Time is tokio's paused clock, which the tests move.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use kbf_front::{Cache, Submission};
use kbf_sched::{Blocked, WholeReason, WholeState, WholeWait};
use kbf_state::fake::Fake;
use kbf_types::{LeaseKind, OperationId};

use super::*;
use crate::Args;
use crate::state_store::StateStore;

type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>;

/// The farm a server builds from `flags`, as `serve` and `frontend` both build it.
fn farm_with(flags: &[&str]) -> Farm {
    let args = Args::parse_from(std::iter::once("kbf-server").chain(flags.iter().copied()));
    let listeners = args.listeners().expect("listeners");
    Farm::from_listeners(Arc::new(Cache::memory()), &listeners)
}

/// A farm of `nodes` workers of four cores, each serving actions and whole-machine leases.
pub(super) struct Rig {
    pub(super) farm: Farm,
    nodes: Vec<(WorkerId, StreamId, Inbox)>,
    beats: u64,
}

impl Rig {
    fn new(flags: &[&str], nodes: &[&str]) -> Self {
        Self::on(farm_with(flags), nodes)
    }

    /// [`Rig::new`] over the state store `store`: what an earlier rig over it queued is
    /// taken up before the nodes register.
    pub(super) fn over(flags: &[&str], nodes: &[&str], store: &Arc<Fake>) -> Self {
        let args = Args::parse_from(std::iter::once("kbf-server").chain(flags.iter().copied()));
        let listeners = args.listeners().expect("listeners");
        let farm = Farm::from_listeners(Arc::new(Cache::memory()), &listeners)
            .with_state(Some(StateStore {
                store: store.clone(),
                epoch: EpochPolicy::Keep,
                members: Default::default(),
                history: None,
            }))
            .expect("a farm over the store");
        Self::on(farm, nodes)
    }

    fn on(farm: Farm, nodes: &[&str]) -> Self {
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
    pub(super) fn submit(&self, key: u8, kind: LeaseKind, wall: Option<Duration>) -> OperationId {
        self.submit_needing(key, kind, wall, kbf_caps::Request::default())
    }

    /// [`Rig::submit`] of a request that asks for `needs` of a worker.
    fn submit_needing(
        &self,
        key: u8,
        kind: LeaseKind,
        wall: Option<Duration>,
        needs: kbf_caps::Request,
    ) -> OperationId {
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
                    needs,
                    whole_worker: false,
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
    pub(super) async fn pass(&mut self, by: Duration) {
        let step = Duration::from_secs(5);
        let mut left = by;
        while !left.is_zero() {
            let now = left.min(step);
            tokio::time::advance(now).await;
            left -= now;
            self.beat().await;
        }
    }

    pub(super) fn whole_state(&self, operation: OperationId) -> Option<WholeState> {
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

/// The setups in which a second whole-machine request is stopped by a cap, with the cap.
#[allow(clippy::type_complexity)]
fn cap_cases() -> [(&'static [&'static str], &'static [&'static str], Blocked); 3] {
    [
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
    ]
}

/// Catches a cap that does not reach the scheduler (a second whole-machine request holds
/// a worker too), the wrong cap named for a setting, and a flag that sets another's
/// number: the ceiling at `--whole-node-max`, the workers kept for ordinary work, the
/// workers draining at once.
#[tokio::test(start_paused = true)]
async fn the_caps_stop_a_second_whole_machine_request() {
    let cases = cap_cases();
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

use serde_json::{Value, json};

/// The `GET /v1/queue` document now.
fn document(rig: &Rig) -> Value {
    crate::http_sched::queue_document(&rig.farm)
}

/// The request of the document's `whole_node` section that names `operation`.
fn shown(document: &Value, operation: OperationId) -> &Value {
    let name = operation.to_string();
    document["whole_node"]["requests"]
        .as_array()
        .expect("requests")
        .iter()
        .find(|r| r["operation"] == name)
        .unwrap_or_else(|| panic!("{name} is not shown in {document}"))
}

/// The item of a request named by `key` and `operation`, in the state `state` and the
/// reason `reason`, with the fields `changes` names set and every other one at its blank.
fn item(operation: OperationId, key: u8, state: &str, reason: &str, changes: Value) -> Value {
    let mut item = json!({
        "operation": operation.to_string(),
        "instance": "main",
        "digest": Digest::new(kbf_types::DigestFunction::Sha256, [key; 32], 1).to_string(),
        "state": state,
        "reason": reason,
        "may_hold_in_ms": null,
        "worker": null,
        "held_for_ms": null,
        "deadline_in_ms": null,
        "give_ups": null,
        "draining": null,
        "barred": [],
        "unservable": null,
    });
    for (field, value) in changes.as_object().expect("changes are an object") {
        item[field] = value.clone();
    }
    item
}

/// The farm's `kbf_sched_whole_*` lines.
fn whole_series(rig: &Rig) -> Vec<String> {
    rig.farm
        .cache
        .render_metrics()
        .lines()
        .filter(|l| l.starts_with("kbf_sched_whole_"))
        .map(str::to_owned)
        .collect()
}

/// Catches an option that is off but shows a `whole_node` section or the gauges (the
/// document keeps its six keys and a scrape has no `kbf_sched_whole_` line), and a
/// whole-node view that exists without settings.
#[tokio::test(start_paused = true)]
async fn off_shows_no_whole_node_section_and_no_gauges() {
    let mut rig = Rig::new(&[], &["node-a", "node-b"]);
    rig.submit(1, LeaseKind::Action, None);
    rig.submit(2, LeaseKind::Action, None);
    rig.submit(3, LeaseKind::WholeMachine, Some(Duration::from_secs(100)));
    rig.pass(Duration::from_secs(10)).await;
    let document = document(&rig);
    let mut keys: Vec<&String> = document.as_object().expect("object").keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "avoided",
            "avoided_total",
            "by_platform",
            "depth",
            "oldest_age_ms",
            "waiting"
        ]
    );
    assert!(rig.farm.whole_view().is_none());
    assert!(rig.farm.queue_views().1.is_none());
    assert_eq!(whole_series(&rig), Vec::<String>::new());
}

/// Catches a state that is shown as another, a time shown from the wrong origin (the
/// farm's zero, not now), a field that is not the state's, and a request that loses its
/// place: one node busy with a long action, two whole-machine requests asking to hold it
/// for twenty seconds, `--whole-node-drain-after-secs=30`. Both are early; the first then
/// holds the node and the second, which has nothing to hold, is queued; the first gives its
/// hold up at its deadline and is barred from the node, which the second holds.
#[tokio::test(start_paused = true)]
async fn the_document_shows_early_holding_queued_and_barred_requests() {
    let mut rig = Rig::new(
        &[
            "--whole-node-max=1",
            "--whole-node-keep-for-ordinary=0",
            "--whole-node-drain-after-secs=30",
        ],
        &["node-a"],
    );
    rig.submit(10, LeaseKind::Action, Some(Duration::from_secs(600)));
    let first = rig.submit(1, LeaseKind::WholeMachine, Some(Duration::from_secs(20)));
    let second = rig.submit(2, LeaseKind::WholeMachine, Some(Duration::from_secs(20)));
    let early = |op, key| item(op, key, "early", "early", json!({"may_hold_in_ms": 30_000}));
    let section = document(&rig);
    assert_eq!(shown(&section, first), &early(first, 1));
    assert_eq!(shown(&section, second), &early(second, 2));
    assert_eq!(section["whole_node"]["requests_total"], 2);

    rig.pass(Duration::from_secs(25)).await;
    let section = document(&rig);
    assert_eq!(
        shown(&section, first)["may_hold_in_ms"],
        5_000,
        "measured from now"
    );

    rig.pass(Duration::from_secs(5)).await;
    let section = document(&rig);
    assert_eq!(
        shown(&section, first),
        &item(
            first,
            1,
            "holding",
            "holding",
            json!({
                "worker": "node-a", "held_for_ms": 0, "deadline_in_ms": 20_000,
                "give_ups": 0, "draining": true
            })
        )
    );
    assert_eq!(
        shown(&section, second),
        &item(second, 2, "queued", "queued", json!({}))
    );

    rig.pass(Duration::from_secs(20)).await;
    let section = document(&rig);
    assert_eq!(
        shown(&section, first),
        &item(
            first,
            1,
            "barred",
            "barred",
            json!({
                "give_ups": 1,
                "barred": [{"worker": "node-a", "until_in_ms": 300_000}]
            })
        )
    );
    assert_eq!(
        shown(&section, second),
        &item(
            second,
            2,
            "holding",
            "holding",
            json!({
                "worker": "node-a", "held_for_ms": 0, "deadline_in_ms": 20_000,
                "give_ups": 0, "draining": true
            })
        )
    );
    rig.pass(Duration::from_secs(10)).await;
    let section = document(&rig);
    assert_eq!(shown(&section, second)["held_for_ms"], 10_000);
    assert_eq!(shown(&section, second)["deadline_in_ms"], 10_000);
    assert_eq!(shown(&section, first)["barred"][0]["until_in_ms"], 290_000);
}

/// Catches a stop that shows no reason or another cap's: each cap of the setups that stop
/// a second request is its `reason`, the state is `stopped`, and the first request, which
/// holds a worker, is `holding`.
#[tokio::test(start_paused = true)]
async fn the_document_names_the_cap_that_stops_a_request() {
    let names = ["cap_ceiling", "cap_keep_for_ordinary", "cap_max_draining"];
    for ((flags, nodes, _), name) in cap_cases().into_iter().zip(names) {
        let rig = Rig::new(flags, nodes);
        for (i, node) in rig.nodes.iter().enumerate() {
            let busy = rig.submit(10 + i as u8, LeaseKind::Action, None);
            assert_eq!(rig.holder(busy), node.0);
        }
        let first = rig.submit(1, LeaseKind::WholeMachine, None);
        let second = rig.submit(2, LeaseKind::WholeMachine, None);
        let section = document(&rig);
        assert_eq!(shown(&section, first)["state"], "holding", "{flags:?}");
        assert_eq!(
            shown(&section, second),
            &item(second, 2, "stopped", name, json!({})),
            "{flags:?}"
        );
        assert_eq!(section["whole_node"]["waiting"][name], 1, "{flags:?}");
    }
}

/// Catches a request nobody can run that is shown with another state, and its reason
/// changed on the way to the document: the text is the scheduler's own, character for
/// character, and the same one the document's `waiting` list gives. Also a request the
/// last-capable rule stops: one node full of ordinary work with an older ordinary request
/// queued, which a whole-machine request must not take the node from.
#[tokio::test(start_paused = true)]
async fn the_document_shows_unservable_text_verbatim_and_the_last_capable_stop() {
    let mut rig = Rig::new(
        &[
            "--whole-node-max=1",
            "--whole-node-keep-for-ordinary=0",
            "--whole-node-drain-after-secs=30",
        ],
        &["node-a"],
    );
    for key in 10..15 {
        rig.submit(key, LeaseKind::Action, Some(Duration::from_secs(600)));
    }
    let last = rig.submit(1, LeaseKind::WholeMachine, Some(Duration::from_secs(20)));
    let macos = kbf_caps::Request::from_platform([("OSFamily", "macos")]).expect("platform");
    let unservable = rig.submit_needing(2, LeaseKind::WholeMachine, None, macos);
    let text = rig
        .farm
        .lock()
        .sched
        .waiting(unservable)
        .expect("unservable")
        .to_owned();
    assert!(text.contains("macos"), "{text}");
    let section = document(&rig);
    assert_eq!(
        shown(&section, unservable),
        &item(
            unservable,
            2,
            "unservable",
            "unservable",
            json!({ "unservable": text })
        ),
        "unservable before drain_after too"
    );
    assert_eq!(section["waiting"][0]["reason"], text.as_str());
    rig.pass(Duration::from_secs(30)).await;
    let section = document(&rig);
    assert_eq!(
        shown(&section, last),
        &item(last, 1, "stopped", "last_capable", json!({}))
    );
    assert_eq!(shown(&section, unservable)["unservable"], text.as_str());
}

/// Catches a deadline shown for a hold that has none (a request with no wall time is
/// held with the end of time as its deadline, which must read `null`, not a huge number)
/// and a deadline of another request's wall time.
#[tokio::test(start_paused = true)]
async fn a_hold_with_no_wall_time_shows_no_deadline() {
    let rig = Rig::new(
        &[
            "--whole-node-max=3",
            "--whole-node-keep-for-ordinary=0",
            "--whole-node-max-draining=3",
        ],
        &["node-a", "node-b", "node-c"],
    );
    for i in 0..3 {
        rig.submit(10 + i, LeaseKind::Action, None);
    }
    let walls = [Some(100), Some(400), None];
    let requests: Vec<_> = walls
        .iter()
        .enumerate()
        .map(|(i, wall)| {
            let wall = wall.map(Duration::from_secs);
            rig.submit(1 + i as u8, LeaseKind::WholeMachine, wall)
        })
        .collect();
    let section = document(&rig);
    let deadlines: Vec<&Value> = requests
        .iter()
        .map(|op| &shown(&section, *op)["deadline_in_ms"])
        .collect();
    assert_eq!(deadlines, [&json!(100_000), &json!(400_000), &Value::Null]);
}

/// Catches the list growing without bound, its total, and the summary's counts: 101
/// requests nobody can run show the first hundred, the total says 101, and the count of
/// the reason is all of them.
#[tokio::test(start_paused = true)]
async fn the_list_of_requests_is_capped_and_its_total_is_not() {
    let rig = Rig::new(&["--whole-node-max=1"], &["node-a"]);
    let macos = kbf_caps::Request::from_platform([("OSFamily", "macos")]).expect("platform");
    let ops: Vec<_> = (0..101)
        .map(|key| rig.submit_needing(key, LeaseKind::WholeMachine, None, macos.clone()))
        .collect();
    let section = document(&rig);
    let shown_ops: Vec<&Value> = section["whole_node"]["requests"]
        .as_array()
        .expect("list")
        .iter()
        .map(|r| &r["operation"])
        .collect();
    assert_eq!(shown_ops.len(), kbf_sched::MAX_AVOIDED);
    assert_eq!(*shown_ops[0], json!(ops[0].to_string()), "queue order");
    assert_eq!(section["whole_node"]["requests_total"], 101);
    assert_eq!(section["whole_node"]["waiting"]["unservable"], 101);
}

/// The document's summary and the gauges, compared with the scheduler's own
/// `whole_summary`; returns it.
fn agree(rig: &Rig) -> kbf_sched::WholeSummary {
    let summary = rig.farm.lock().sched.whole_summary().expect("on");
    let document = document(rig);
    let section = &document["whole_node"];
    let metrics = rig.farm.cache.metrics();
    for (name, value, gauge) in [
        ("held", summary.held, Some(metrics.sched_whole.held())),
        ("marks", summary.marks, Some(metrics.sched_whole.marks())),
        ("whole", summary.whole, None),
        (
            "draining",
            summary.draining,
            Some(metrics.sched_whole.draining()),
        ),
        (
            "ceiling",
            summary.ceiling,
            Some(metrics.sched_whole.ceiling()),
        ),
        ("max_whole_node", summary.max_whole_node, None),
        ("capable_hosts", summary.capable_hosts, None),
        ("keep_for_ordinary", summary.keep_for_ordinary, None),
        ("max_draining", summary.max_draining, None),
    ] {
        assert_eq!(section[name], value, "{name}");
        if let Some(gauge) = gauge {
            assert_eq!(gauge, value as u64, "gauge {name}");
        }
    }
    for reason in kbf_sched::WHOLE_REASONS {
        let count = summary.waiting.get(&reason).copied().unwrap_or(0);
        let name = kbf_sched::whole_reason_name(reason);
        assert_eq!(section["waiting"][name], count, "{name}");
        assert_eq!(
            metrics.sched_whole.waiting(reason),
            count as u64,
            "gauge {name}"
        );
    }
    summary
}

/// Catches a summary in the document or a gauge that differs from the scheduler's own
/// `whole_summary`, and a gauge that does not move when requests change state or does not
/// fall back to zero: at each step of one node, two whole-machine requests (early, one
/// holding and one queued, one barred and one holding) the numbers and the count of every
/// reason are compared with the scheduler's, and the counts are what the states are.
#[tokio::test(start_paused = true)]
async fn the_summary_and_the_gauges_follow_the_scheduler() {
    let mut rig = Rig::new(
        &[
            "--whole-node-max=1",
            "--whole-node-keep-for-ordinary=0",
            "--whole-node-drain-after-secs=30",
        ],
        &["node-a"],
    );
    rig.submit(10, LeaseKind::Action, Some(Duration::from_secs(600)));
    rig.submit(1, LeaseKind::WholeMachine, Some(Duration::from_secs(20)));
    rig.submit(2, LeaseKind::WholeMachine, Some(Duration::from_secs(20)));
    let counts =
        |rig: &Rig| -> Vec<(WholeReason, usize)> { agree(rig).waiting.into_iter().collect() };
    assert_eq!(counts(&rig), [(WholeReason::Early, 2)]);
    rig.pass(Duration::from_secs(30)).await;
    assert_eq!(
        counts(&rig),
        [(WholeReason::Holding, 1), (WholeReason::Queued, 1)]
    );
    let summary = agree(&rig);
    assert_eq!(
        (
            summary.held,
            summary.marks,
            summary.draining,
            summary.ceiling
        ),
        (0, 1, 1, 1)
    );
    assert_eq!(
        whole_series(&rig)[..4],
        [
            "kbf_sched_whole_held 0",
            "kbf_sched_whole_marks 1",
            "kbf_sched_whole_draining 1",
            "kbf_sched_whole_ceiling 1"
        ]
    );
    rig.pass(Duration::from_secs(20)).await;
    assert_eq!(
        counts(&rig),
        [(WholeReason::Holding, 1), (WholeReason::Barred, 1)]
    );
    assert_eq!(
        rig.farm
            .cache
            .metrics()
            .sched_whole
            .waiting(WholeReason::Queued),
        0,
        "the queued count fell"
    );
}

/// Catches `held` that does not count a granted whole-machine lease: on an idle node the
/// request is granted at the next round, no request waits, and the document lists none.
#[tokio::test(start_paused = true)]
async fn a_granted_whole_machine_lease_is_held_not_waiting() {
    let mut rig = Rig::new(
        &["--whole-node-max=1", "--whole-node-keep-for-ordinary=0"],
        &["node-a"],
    );
    let granted = rig.submit(1, LeaseKind::WholeMachine, None);
    rig.pass(Duration::from_secs(5)).await;
    rig.holder(granted);
    let summary = agree(&rig);
    assert_eq!((summary.held, summary.whole, summary.marks), (1, 1, 0));
    assert!(summary.waiting.is_empty());
    assert_eq!(document(&rig)["whole_node"]["requests"], json!([]));
    assert!(whole_series(&rig).contains(&"kbf_sched_whole_held 1".to_owned()));
}

/// Catches a document `ceiling` taken from `--whole-node-max` instead of the scheduler's
/// (the capable workers less those kept for ordinary work, when that is fewer): three
/// workers, a maximum of three and one kept, so the ceiling is two.
#[tokio::test(start_paused = true)]
async fn the_ceiling_is_the_schedulers_not_the_setting() {
    let mut rig = Rig::new(
        &["--whole-node-max=3", "--whole-node-keep-for-ordinary=1"],
        &["node-a", "node-b", "node-c"],
    );
    rig.beat().await;
    let summary = agree(&rig);
    assert_eq!((summary.ceiling, summary.max_whole_node), (2, 3));
    let section = document(&rig);
    assert_eq!(section["whole_node"]["ceiling"], 2);
    assert_eq!(section["whole_node"]["max_whole_node"], 3);
}

/// Catches a hold that is never given up shown with a deadline: after
/// `--whole-node-max-reserve-attempts` give-ups the next hold has none, which the document
/// shows as `null` (not 0, which would read as due now), with the give-up counted.
#[tokio::test(start_paused = true)]
async fn a_hold_that_is_not_given_up_again_has_a_null_deadline() {
    let mut rig = Rig::new(
        &[
            "--whole-node-max=1",
            "--whole-node-keep-for-ordinary=0",
            "--whole-node-max-reserve-attempts=1",
            "--whole-node-reserve-backoff-secs=1",
        ],
        &["node-a"],
    );
    rig.submit(10, LeaseKind::Action, Some(Duration::from_secs(600)));
    let whole = rig.submit(1, LeaseKind::WholeMachine, Some(Duration::from_secs(20)));
    rig.pass(Duration::from_secs(10)).await;
    assert_eq!(shown(&document(&rig), whole)["deadline_in_ms"], 10_000);
    rig.pass(Duration::from_secs(50)).await;
    let section = document(&rig);
    let request = shown(&section, whole);
    assert_eq!(request["state"], "holding", "{request}");
    assert_eq!(request["give_ups"], 1);
    assert_eq!(request["deadline_in_ms"], Value::Null);
}

/// Catches a back-off with no end shown as a number: with a back-off of the longest
/// possible length the bar's end is the end of time, and `until_in_ms` is `null`.
#[tokio::test(start_paused = true)]
async fn a_bar_with_no_end_shows_a_null_until() {
    let mut rig = Rig::new(
        &[
            "--whole-node-max=1",
            "--whole-node-keep-for-ordinary=0",
            "--whole-node-reserve-backoff-secs=18446744073709551615",
        ],
        &["node-a"],
    );
    rig.submit(10, LeaseKind::Action, Some(Duration::from_secs(600)));
    let whole = rig.submit(1, LeaseKind::WholeMachine, Some(Duration::from_secs(20)));
    rig.pass(Duration::from_secs(30)).await;
    let section = document(&rig);
    let request = shown(&section, whole);
    assert_eq!(request["state"], "barred", "{request}");
    assert_eq!(
        request["barred"],
        json!([{"worker": "node-a", "until_in_ms": null}])
    );
}

/// Asserts that the update node step's look counts the leases each node holds and, of those,
/// none undelivered once their `Start`s went out. Catches a look that counts no leases on a
/// busy node (the update step would restart it under its builds), and one that counts a
/// lease whose `Start` was sent as undelivered.
#[tokio::test(start_paused = true)]
async fn the_update_look_counts_each_nodes_leases_and_none_undelivered_once_sent() {
    let rig = Rig::new(&[], &["node-a", "node-b"]);
    rig.busy();
    let looks: Vec<_> = rig
        .farm
        .update_look()
        .into_iter()
        .map(|look| (look.id.as_str().to_owned(), look.leases, look.undelivered))
        .collect();
    assert_eq!(
        looks,
        [("node-a".to_owned(), 1, 0), ("node-b".to_owned(), 1, 0)]
    );
}
