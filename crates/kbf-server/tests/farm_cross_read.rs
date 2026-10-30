//! A state store written by one scheduler core, taken over by a farm of the other.
//!
//! The farm runs on the legacy core or on the js core (`--scheduler-core`), and the choice is
//! undone by restarting a frontend over the same store. So each core must read what the
//! other wrote, in the middle of its work: an operation queued, one leased, one answered, one
//! killed for memory with its floor raised, a cordoned node, and the learned-size summaries
//! the shadow keeps.
//!
//! Each test runs over the in-memory fake, a SQLite file and a Postgres schema
//! (`KBF_TEST_POSTGRES_URL` names the server; the Postgres tests fail without it). The farm's
//! clock is tokio's, paused; nothing here waits or measures time. The SQLite and Postgres stores
//! stamp their rows from their own clocks, which run in real time under the paused one; the
//! only times a row's meaning depends on here are the lease grace and the done-row retention
//! (minutes), and a test takes milliseconds, so a run could only differ if the machine stalled
//! for minutes between two lines of one test.
//!
//! The js core exists only in a build with the `farm-core-js` feature, so the file is empty
//! without it.
#![cfg(feature = "farm-core-js")]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use kbf_caps::NodeCaps;
use kbf_front::{Cache, Described, Dispatch, Finished, Stage, Submission, Ticket};
use kbf_log::{FarmBatch, FarmInput, Format, LogCommand};
use kbf_proto::google::rpc;
use kbf_proto::reapi::{ActionResult, ExecutedActionMetadata};
use kbf_proto::worker::ResultAckReason as Reason;
use kbf_proto::worker::server_message::Message;
use kbf_proto::worker::{self, CgroupUsage, MemoryKill, ResourceUsage, ServerMessage, UsageBasis};
use kbf_sched::{DaemonInstance, Request};
use kbf_server::farm::{NodeAction, Reported, StreamId};
use kbf_server::fleet::{PlacementView, ResourcesView};
use kbf_server::{EpochPolicy, Farm, LearnedSizes, SchedulerCore, StateStore};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::postgres::{Flaw as PgFlaw, PostgresBackend, PostgresStore};
use kbf_state::sqlite::{Options, SqliteStore};
use kbf_state::{Cond, ManualClock, Store, Txn};
use kbf_types::sched::usage::LeaseCost;
use kbf_types::{
    ActionKey, ControlRecord, Digest, DigestFunction, FarmTime, LeaseGrant, LeaseId, LeaseKind,
    MemoryRun, OperationId, Qos, Resources, WorkerId,
};
use tokio::sync::mpsc;
use tonic::Code;

const GIB: u64 = 1 << 30;

type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>;

/// What the store is kept in.
#[derive(Clone, Copy)]
enum Backing {
    Fake,
    Sqlite,
    Postgres,
}

/// The state store both cores work over, and what keeps it alive.
struct Disk {
    store: Arc<dyn Store>,
    _dir: Option<kbf_testkit::dir::TestDir>,
    // Declared after `store`, so the store closes first.
    _postgres: Option<PostgresBackend>,
}

impl Disk {
    fn over(backing: Backing) -> Self {
        match backing {
            Backing::Fake => Self {
                store: Arc::new(Fake::with_flaw(
                    Arc::new(ManualClock::default()),
                    Flaw::None,
                )),
                _dir: None,
                _postgres: None,
            },
            Backing::Sqlite => {
                let dir =
                    kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "kbf-cross-read");
                let store = SqliteStore::open(
                    &dir.join("state.db"),
                    Arc::new(kbf_state::SystemClock::default()),
                    Options::default(),
                )
                .expect("a SQLite store");
                Self {
                    store: Arc::new(store),
                    _dir: Some(dir),
                    _postgres: None,
                }
            }
            Backing::Postgres => {
                let url = kbf_testkit::database::server_url();
                let backend = PostgresBackend::new(&url, PgFlaw::None);
                let store = PostgresStore::open(&backend.fresh_config()).expect("a Postgres store");
                Self {
                    store: Arc::new(store),
                    _dir: None,
                    _postgres: Some(backend),
                }
            }
        }
    }

    /// The rows under `prefix`, key and value.
    fn rows(&self, prefix: &str) -> BTreeMap<String, Vec<u8>> {
        self.store
            .scan_prefix(prefix)
            .expect("scan")
            .into_iter()
            .map(|(key, row)| (key, row.value))
            .collect()
    }

    /// The leases the `lease/` rows hold, each with the cost it was granted with.
    fn lease_costs(&self) -> BTreeMap<LeaseId, LeaseCost> {
        self.rows("lease/")
            .into_iter()
            .map(|(key, bytes)| {
                let grant = grant_of(&key, &bytes);
                (grant.lease, grant.cost)
            })
            .collect()
    }

    /// The rows of the families a rollback depends on besides the ones each test names.
    fn families(&self) -> BTreeMap<String, Vec<u8>> {
        FAMILIES
            .iter()
            .flat_map(|prefix| self.rows(prefix))
            .collect()
    }

    /// The first operation id and lease sequence no frontend has reserved.
    fn counters(&self) -> (u64, u64) {
        let counter = |key: &str| {
            self.store.get(key).expect("read").map_or(0, |row| {
                u64::from_be_bytes(row.value.as_slice().try_into().expect("eight bytes"))
            })
        };
        (counter("meta/next_op"), counter("meta/next_seq"))
    }
}

/// The row families a takeover must leave as the other core wrote them: the fate of each
/// operation, the lease of each operation, the losses, who owns each node, each node's
/// session, what each operation's log line says, the names of the answered, and the
/// longest an operation may run.
const FAMILIES: [&str; 8] = [
    "fate/",
    "lease-by-op/",
    "loss/",
    "owner/",
    "session/",
    "subject/",
    "doneof/",
    "opwall/",
];

/// The lease a `lease/` row holds.
fn grant_of(key: &str, bytes: &[u8]) -> LeaseGrant {
    let decoded = kbf_log::decode(Format::V1, bytes).expect("a lease row decodes");
    let LogCommand::Farm(FarmBatch { mut inputs, .. }) = decoded else {
        panic!("{key}: not a scheduler input");
    };
    let Some(FarmInput::Committed(ControlRecord::Lease(grant))) = inputs.pop() else {
        panic!("{key}: not a lease");
    };
    grant
}

/// A farm of `core` over the store, learning sizes in shadow as the live farm does.
fn farm_on(core: SchedulerCore, disk: &Disk) -> Farm {
    Farm::new_on_core(
        core,
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_learned_sizes(LearnedSizes::Shadow, 4 * GIB)
    .with_state(Some(StateStore {
        store: disk.store.clone(),
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }))
    .expect("a farm over the store")
}

/// A node as a test holds it: dropping the inbox ends the stream.
struct Node {
    name: WorkerId,
    stream: StreamId,
    inbox: Inbox,
    beats: u64,
}

impl Node {
    /// `name`, with `cores` cores and room for the whole memory ladder, joined to `farm`.
    fn join(farm: &Farm, name: &str, cores: u64) -> Self {
        let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
            .expect("caps")
            .with_drivers(["container"]);
        let reported = Reported::serving(Resources::new(cores * 1_000, 64 * GIB), caps);
        let (outbound, inbox) = mpsc::unbounded_channel();
        let name = WorkerId::new(name);
        let stream = farm.register(
            &name,
            DaemonInstance::new(format!("daemon-{name}")),
            DaemonInstance::new(""),
            reported,
            outbound,
            ServerMessage::default(),
        );
        Self {
            name,
            stream,
            inbox,
            beats: 0,
        }
    }

    /// The leases of the `Start`s sent since the last call, each with the memory it books.
    fn starts(&mut self) -> Vec<(LeaseId, u64)> {
        let mut starts = Vec::new();
        while let Ok(message) = self.inbox.try_recv() {
            if let Some(Message::Start(start)) = message.expect("no error").message {
                let lease = start.lease_id.expect("a lease id");
                starts.push((LeaseId::new(lease.term, lease.seq), start.memory_bytes));
            }
        }
        starts
    }

    /// The one lease started since the last call.
    fn started(&mut self) -> (LeaseId, u64) {
        let starts = self.starts();
        let [start] = starts[..] else {
            panic!("one start expected, got {starts:?}");
        };
        start
    }

    /// A heartbeat naming `running`; whether the farm acknowledged it.
    async fn beat(&mut self, farm: &Farm, running: &[LeaseId]) -> bool {
        self.beats += 1;
        farm.heartbeat(&self.name, self.stream, self.beats, running.to_vec())
            .await
    }
}

fn request(tag: u8, memory_gib: u64, do_not_cache: bool) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        }
        .into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, memory_gib * GIB),
        hermetic: true,
        do_not_cache,
        needs: kbf_caps::Request::from_platform([("OSFamily", "linux")]).expect("a platform"),
        whole_worker: false,
    }
}

fn submit(farm: &Farm, request: Request) -> Ticket {
    submit_walled(farm, request, None)
}

/// [`submit`], naming the longest the action may run.
fn submit_walled(farm: &Farm, request: Request, max_wall: Option<Duration>) -> Ticket {
    farm.submit(Submission {
        request,
        call: None,
        described: Described {
            arguments: vec!["cc".to_owned()],
            ..Described::default()
        },
        evidence: Default::default(),
        max_wall,
    })
    .expect("queued")
}

fn usage(peak: u64) -> ResourceUsage {
    ResourceUsage {
        wall_micros: 2_000_000,
        basis: UsageBasis::Cgroup as i32,
        cgroup: Some(CgroupUsage {
            cpu_usage_micros: Some(1_000_000),
            memory_peak_bytes: Some(peak),
            ..CgroupUsage::default()
        }),
        ..ResourceUsage::default()
    }
}

fn wire(lease: LeaseId) -> worker::LeaseId {
    worker::LeaseId {
        term: lease.term,
        seq: lease.seq,
    }
}

/// A result of `lease`: the action ran and exited 0, and the daemon sent its usage.
fn ran(lease: LeaseId) -> worker::Result {
    worker::Result {
        lease_id: Some(wire(lease)),
        status: Some(rpc::Status::default()),
        action_result: Some(ActionResult {
            exit_code: 0,
            execution_metadata: Some(ExecutedActionMetadata {
                auxiliary_metadata: vec![usage(GIB).to_any()],
                ..ExecutedActionMetadata::default()
            }),
            ..ActionResult::default()
        }),
        ..worker::Result::default()
    }
}

/// A result of `lease`: killed for passing its own memory limit.
fn out_of_memory(lease: LeaseId) -> worker::Result {
    worker::Result {
        lease_id: Some(wire(lease)),
        status: Some(rpc::Status {
            code: Code::ResourceExhausted as i32,
            message: "oom".to_owned(),
            details: Vec::new(),
        }),
        memory_kill: MemoryKill::OwnLimit as i32,
        ..worker::Result::default()
    }
}

/// What the farm answers a daemon that sends a `Result`.
#[derive(Debug, PartialEq, Eq)]
enum Ack {
    /// Taken.
    Accepted,
    /// Refused, for this reason.
    Refused(Reason),
    /// No acknowledgement at all.
    Silent,
}

async fn ack_of(farm: &Farm, node: &WorkerId, result: worker::Result) -> Ack {
    match farm.report(node, result).await {
        None => Ack::Silent,
        Some(ack) if ack.accepted => {
            assert_eq!(Reason::try_from(ack.reason), Ok(Reason::Accepted));
            Ack::Accepted
        }
        Some(ack) => Ack::Refused(Reason::try_from(ack.reason).expect("a known reason")),
    }
}

/// Whether the farm acknowledges `result` of `node` as accepted, and says so.
async fn accepted(farm: &Farm, node: &WorkerId, result: worker::Result) -> bool {
    ack_of(farm, node, result).await == Ack::Accepted
}

fn is_done(ticket: &Ticket) -> bool {
    matches!(*ticket.stage.borrow(), Stage::Done(_))
}

fn is_answered_with_a_result(ticket: &Ticket) -> bool {
    matches!(*ticket.stage.borrow(), Stage::Done(Finished::Ran(_)))
}

/// What a farm shows of the leases it holds: the lease and the operation it runs.
fn leases_of(farm: &Farm) -> BTreeSet<(LeaseId, OperationId)> {
    farm.lease_views(None)
        .leases
        .iter()
        .map(|view| (view.lease, view.operation))
        .collect()
}

/// What a farm shows of one node: how many leases it holds, what they book, whether it
/// is cordoned.
fn node_of(farm: &Farm, name: &str) -> (u32, ResourcesView, bool) {
    let nodes = farm.nodes().nodes;
    let node = nodes
        .iter()
        .find(|node| node.node_id == name)
        .unwrap_or_else(|| panic!("{name} is not listed"));
    (
        node.lease_count,
        node.booked,
        matches!(node.placement, PlacementView::Cordoned),
    )
}

/// A legacy farm learns one finished run in shadow and writes its size summary, then ends.
/// Returns nothing: the summaries are in the store.
async fn seed_summaries(disk: &Disk) {
    let farm = farm_on(SchedulerCore::Legacy, disk);
    let mut node = Node::join(&farm, "node-a", 2);
    submit(&farm, request(9, 4, false));
    let (lease, _) = node.started();
    assert!(accepted(&farm, &node.name, ran(lease)).await);
    farm.write_behind_now();
    assert!(
        !disk.rows("est/").is_empty(),
        "the shadow wrote no size summary, so there is nothing for the other core to leave alone"
    );
}

/// Everything a farm left of its half-finished work, as the store and the farm's views show it.
struct Left {
    /// The operation names the callers hold, in the order of the operations: answered,
    /// leased, killed for memory (running again), queued.
    names: [String; 4],
    /// Every lease the farm granted, finished or not.
    issued: BTreeSet<LeaseId>,
    /// The leases it still holds, and the operations they run.
    held: BTreeSet<(LeaseId, OperationId)>,
    /// The lease the killed operation runs under now, and the memory it books.
    rerun: (LeaseId, u64),
    /// The lease of the leased operation.
    leased: LeaseId,
    epoch: u64,
    runs: BTreeMap<OperationId, Vec<MemoryRun>>,
    node_a: (u32, ResourcesView, bool),
    done: BTreeMap<String, Vec<u8>>,
    cordon: BTreeMap<String, Vec<u8>>,
    floor: BTreeMap<String, Vec<u8>>,
    estimates: BTreeMap<String, Vec<u8>>,
    costs: BTreeMap<LeaseId, LeaseCost>,
    /// The lease of the answered operation, and the first lease of the killed one (given up
    /// when it was killed): leases that ended before the takeover.
    ended: [LeaseId; 2],
    families: BTreeMap<String, Vec<u8>>,
    counters: (u64, u64),
}

/// A farm of `core` does the work of a morning and stops with a store a successor must take
/// over: one operation answered, one leased, one killed for memory and leased again at the
/// raised booking (its floor written), one queued with no room, and a cordoned node.
async fn leave_mid_flight(core: SchedulerCore, disk: &Disk) -> Left {
    seed_summaries(disk).await;
    let farm = farm_on(core, disk);
    let mut a = Node::join(&farm, "node-a", 2);
    let b = Node::join(&farm, "node-b", 2);
    farm.place(&b.name, NodeAction::Cordon).expect("cordoned");

    let answered = submit(&farm, request(1, 4, false));
    let (first, _) = a.started();
    assert!(accepted(&farm, &a.name, ran(first)).await);

    let leased = submit(&farm, request(2, 4, false));
    let (second, _) = a.started();

    let killed = submit(&farm, request(3, 4, false));
    let (third, booked) = a.started();
    assert_eq!(booked, 4 * GIB);
    assert!(accepted(&farm, &a.name, out_of_memory(third)).await);
    farm.tick().await;
    let rerun @ (rerun_lease, raised) = a.started();
    assert_eq!(raised, 16 * GIB, "the kill raised the booking");

    let queued = submit(&farm, request(4, 4, false));
    farm.tick().await;
    assert!(a.starts().is_empty(), "no room for the fourth");
    assert_eq!(farm.queued_operations(), 1);
    farm.write_behind_now();

    // A comparison of two empty families proves nothing: each of these must hold rows.
    for prefix in [
        "cordon/",
        "floor/",
        "done/",
        "est/",
        "fate/",
        "lease-by-op/",
        "owner/",
        "session/",
        "subject/",
        "doneof/",
    ] {
        assert!(
            !disk.rows(prefix).is_empty(),
            "the farm wrote no {prefix} row"
        );
    }
    let runs = farm.memory_runs();
    assert!(!runs.is_empty(), "the kill left no memory run to compare");
    Left {
        names: [answered, leased, killed, queued].map(|ticket| ticket.name),
        issued: BTreeSet::from([first, second, third, rerun_lease]),
        held: leases_of(&farm),
        rerun,
        leased: second,
        epoch: farm.lease_epoch(),
        runs,
        node_a: node_of(&farm, "node-a"),
        done: disk.rows("done/"),
        cordon: disk.rows("cordon/"),
        floor: disk.rows("floor/"),
        estimates: disk.rows("est/"),
        costs: disk.lease_costs(),
        ended: [first, third],
        families: disk.families(),
        counters: disk.counters(),
    }
}

/// A farm of `core` takes over the store `left` describes, and carries the work to its end.
///
/// Asserts that: the store is read as it was left (the same epoch, answered operation
/// still answerable, the done, cordon, floor and summary rows still there); the two
/// leases the daemon still runs are rebound to the same operations and book the same; the
/// cordoned node is still cordoned and is offered nothing; each operation is answered
/// once and the lease of the queued one is new; and the floor still raises a fresh submission.
///
/// A daemon whose acknowledgement was lost sends again the `Result` of a lease that ended
/// before the takeover. It must be refused for `replay` and answer nobody a second time.
/// Returns what node-a shows after its heartbeat.
async fn take_over(
    core: SchedulerCore,
    disk: &Disk,
    left: &Left,
    replay: Reason,
) -> (u32, ResourcesView, bool) {
    let farm = farm_on(core, disk);
    assert_eq!(
        farm.lease_epoch(),
        left.epoch,
        "the successor serves the epoch"
    );
    assert_eq!(
        disk.rows("done/"),
        left.done,
        "an answered operation was dropped"
    );
    assert_eq!(disk.rows("cordon/"), left.cordon, "a cordon was dropped");
    assert_eq!(disk.rows("floor/"), left.floor, "a floor was dropped");
    assert_eq!(
        disk.rows("est/"),
        left.estimates,
        "a size summary was changed"
    );
    assert_eq!(
        disk.families(),
        left.families,
        "a takeover changed a row family it only reads"
    );

    let tickets: Vec<Ticket> = left
        .names
        .iter()
        .map(|name| farm.wait(name).expect("the operation is still known"))
        .collect();
    assert!(
        is_answered_with_a_result(&tickets[0]),
        "the answered operation is answered again from its row"
    );
    assert!(tickets[1..].iter().all(|ticket| !is_done(ticket)));

    let mut a = Node::join(&farm, "node-a", 2);
    let mut b = Node::join(&farm, "node-b", 2);
    let running = [left.leased, left.rerun.0];
    assert!(
        a.beat(&farm, &running).await,
        "the heartbeat is acknowledged"
    );
    assert!(
        a.starts().is_empty(),
        "a lease the daemon runs was started again"
    );
    assert_eq!(
        leases_of(&farm),
        left.held,
        "the leases are not the ones left"
    );
    assert_eq!(
        node_of(&farm, "node-a"),
        left.node_a,
        "the node books what it booked"
    );
    let (_, _, cordoned) = node_of(&farm, "node-b");
    assert!(cordoned, "the cordon is not honoured");
    assert_eq!(farm.memory_runs(), left.runs, "the kill was forgotten");
    assert_eq!(farm.queued_operations(), 1);
    let booked_after = node_of(&farm, "node-a");
    farm.tick().await;
    assert!(b.starts().is_empty(), "a cordoned node was offered work");

    // The Results of leases that ended before the takeover are sent again.
    for (lease, result) in [
        (left.ended[0], ran(left.ended[0])),
        (left.ended[1], out_of_memory(left.ended[1])),
    ] {
        assert_eq!(
            ack_of(&farm, &a.name, result).await,
            Ack::Refused(replay),
            "{lease:?}"
        );
    }
    assert_eq!(
        disk.rows("done/"),
        left.done,
        "a replay changed the answers"
    );
    assert!(is_answered_with_a_result(&tickets[0]));
    assert!(!is_done(&tickets[2]), "the killed operation was answered");
    assert_eq!(
        farm.memory_runs(),
        left.runs,
        "a replayed kill counted again"
    );
    assert_eq!(farm.queued_operations(), 1);
    assert_eq!(leases_of(&farm), left.held);

    // The two running leases end; the queued operation is given the room.
    let mut fresh = Vec::new();
    for lease in running {
        assert!(accepted(&farm, &a.name, ran(lease)).await, "{lease:?}");
        farm.tick().await;
        fresh.extend(a.starts());
    }
    let [(queued_lease, _)] = fresh[..] else {
        panic!("one lease for the queued operation expected, got {fresh:?}");
    };
    assert!(
        !left.issued.contains(&queued_lease) && Some(&queued_lease) > left.issued.last(),
        "{queued_lease:?} reuses or precedes a lease granted before {:?}",
        left.issued
    );
    assert!(accepted(&farm, &a.name, ran(queued_lease)).await);

    // Each is answered once: the same result again is refused.
    for lease in running.into_iter().chain([queued_lease]) {
        assert_eq!(
            ack_of(&farm, &a.name, ran(lease)).await,
            Ack::Refused(Reason::Duplicate),
            "{lease:?} twice"
        );
    }
    for name in &left.names {
        let ticket = farm.wait(name).expect("still known");
        assert!(is_answered_with_a_result(&ticket), "{name}");
    }
    let done_now = disk.rows("done/");
    assert!(
        left.done.keys().all(|key| done_now.contains_key(key)),
        "an answered operation's row went before its retention"
    );
    assert_eq!(done_now.len(), left.done.len() + 3);
    let summaries_now = disk.rows("est/");
    assert!(
        left.estimates
            .keys()
            .all(|key| summaries_now.contains_key(key)),
        "a size summary was dropped"
    );
    for prefix in ["op/", "queue/", "lease/"] {
        assert!(
            disk.rows(prefix).is_empty(),
            "{prefix} rows of finished work"
        );
    }

    // The floor the kill raised is offered to a fresh submission of the same action.
    submit(&farm, request(3, 4, true));
    let (fresh, booked) = a.started();
    assert_eq!(booked, 16 * GIB, "the floor was not offered");
    assert!(Some(&fresh) > left.issued.last());
    let (next_op, next_seq) = disk.counters();
    assert!(
        next_op >= left.counters.0 && next_seq >= left.counters.1,
        "an id counter went back: {:?} after {:?}",
        (next_op, next_seq),
        left.counters
    );
    booked_after
}

/// Runs each test over the fake, a SQLite file and a Postgres schema.
macro_rules! over_each_store {
    ($($name:ident),* $(,)?) => {$(
        mod $name {
            #[tokio::test(start_paused = true)]
            async fn over_the_fake() {
                super::$name(super::Backing::Fake).await;
            }

            #[tokio::test(start_paused = true)]
            async fn over_sqlite() {
                super::$name(super::Backing::Sqlite).await;
            }

            #[tokio::test(start_paused = true)]
            async fn over_postgres() {
                super::$name(super::Backing::Postgres).await;
            }
        }
    )*};
}

over_each_store!(
    a_js_farm_takes_over_the_store_a_legacy_farm_left_mid_flight,
    a_legacy_farm_takes_over_the_store_a_js_farm_left_mid_flight,
    a_row_written_before_the_cost_field_recovers_at_its_request,
    a_legacy_walled_operation_finished_by_the_js_core_deletes_its_opwall_row,
);

/// A rollback is forward only if the other direction works too: this is the flip itself. A
/// legacy farm leaves an answered, a leased, a memory-killed, a queued operation, a cordon
/// and its summaries; a js farm over the same store takes all of it up.
///
/// Catches: a row of the legacy farm the js core cannot read (a refused start), an
/// operation answered twice or never, a lease id handed out again, a done row dropped before
/// its retention, a cordon or a floor lost, and a lease booked differently after the
/// takeover. The legacy lease rows carry no cost, so the js core must take them up as
/// leases charged their request; a js core that reads them as free would book them at zero.
async fn a_js_farm_takes_over_the_store_a_legacy_farm_left_mid_flight(backing: Backing) {
    let disk = Disk::over(backing);
    let left = leave_mid_flight(SchedulerCore::Legacy, &disk).await;
    assert!(
        left.costs
            .values()
            .all(|cost| *cost == LeaseCost::default()),
        "the legacy farm writes a lease row with a cost: {:?}",
        left.costs
    );
    assert_eq!(left.costs.len(), 2);
    // Neither core remembers a lease that ended before its process started (the legacy core
    // keeps the answered ones in memory, the js core in its recovered state), so a replay after
    // any restart, legacy to legacy included, is an unknown lease and not a duplicate. The
    // daemon logs a warning and drops the Result; nobody is answered twice.
    take_over(SchedulerCore::Js, &disk, &left, Reason::UnknownLease).await;
}

/// The rollback: a js farm leaves the same work, its lease rows carrying the cost
/// admission charged each lease (the rerun up the memory ladder is flagged as one), and a
/// legacy farm over the same store takes it up.
///
/// Catches: a row of the js core the legacy core cannot read (the cost field must be skipped,
/// not refused), a lease the legacy farm drops or books differently, an operation answered
/// twice or never, a lease id handed out again, and the summaries of the shadow lost: the js
/// core does not write them, so the legacy farm must still find the ones it wrote before.
async fn a_legacy_farm_takes_over_the_store_a_js_farm_left_mid_flight(backing: Backing) {
    let disk = Disk::over(backing);
    let left = leave_mid_flight(SchedulerCore::Js, &disk).await;
    assert_eq!(left.costs.len(), 2);
    for (lease, cost) in &left.costs {
        assert_ne!(*cost, LeaseCost::default(), "{lease:?} was granted free");
    }
    assert!(
        left.costs[&left.rerun.0].retry,
        "the rerun up the ladder is not flagged as one"
    );
    assert!(!left.costs[&left.leased].retry);
    // A legacy process knows the leases it granted itself and those it recovered running; one
    // that ended before it started is not among them.
    let (_, booked, _) = take_over(SchedulerCore::Legacy, &disk, &left, Reason::UnknownLease).await;

    // The accepted divergence of a rollback: the legacy core keeps no per-lease cost, so it
    // books each recovered lease at its request. Here that is more than the js core charged
    // (the lease with no prediction is charged no memory, only its limit), and never less:
    // a rolled-back farm admits no more than the js one did.
    let leased = &left.costs[&left.leased];
    assert!(
        leased.memory_bytes < leased.limit_bytes,
        "the js core charged this lease its whole limit, so the two cores agree and nothing is shown"
    );
    let charged_cpu: u64 = left.costs.values().map(|cost| cost.cpu_millis).sum();
    let charged_memory: u64 = left.costs.values().map(|cost| cost.memory_bytes).sum();
    assert!(
        charged_cpu <= booked.cpu_millis,
        "{charged_cpu} cores charged"
    );
    assert!(
        charged_memory < booked.memory_bytes,
        "{charged_memory} bytes charged, {} booked",
        booked.memory_bytes
    );
}

/// A lease row as it was written before the cost field existed is taken up by the js core as
/// a lease charged its request.
///
/// The row is a js row with its cost field cut out: the log's lease record without field 4,
/// the bytes every binary wrote before the field. (The legacy writer's rows decode to the same
/// grant, but they say nothing about the bytes: an absent field and a zero cost read alike.)
///
/// Catches: a row decoded as a lease with a zero cost (booked at nothing, so the node is
/// offered more work than it has room for), or one the js core refuses.
async fn a_row_written_before_the_cost_field_recovers_at_its_request(backing: Backing) {
    let disk = Disk::over(backing);
    let writer = farm_on(SchedulerCore::Js, &disk);
    let mut node = Node::join(&writer, "node-a", 1);
    let held = submit(&writer, request(1, 4, false)).name;
    let (lease, booked) = node.started();
    let queued = submit(&writer, request(2, 4, false)).name;
    assert!(node.starts().is_empty(), "no room for the second");
    let before = node_of(&writer, "node-a");
    drop(node);
    drop(writer);

    let (key, with_cost) = disk.rows("lease/").into_iter().next().expect("a lease row");
    let mut grant = grant_of(&key, &with_cost);
    assert_ne!(grant.cost, LeaseCost::default(), "the js row has no cost");
    grant.cost = LeaseCost::default();
    let old = kbf_log::encode(
        Format::V1,
        &LogCommand::Farm(FarmBatch {
            now: FarmTime::default(),
            wall: UNIX_EPOCH,
            inputs: vec![FarmInput::Committed(ControlRecord::Lease(grant))],
        }),
    )
    .expect("a lease row encodes");
    assert!(old.len() < with_cost.len(), "the cost field is still there");
    disk.store
        .txn(Txn::new().put(key.clone(), old.clone(), Cond::Any))
        .expect("a store")
        .expect("a write");
    assert_eq!(disk.rows("lease/").get(&key), Some(&old));
    assert_eq!(disk.lease_costs().get(&lease), Some(&LeaseCost::default()));

    let farm = farm_on(SchedulerCore::Js, &disk);
    let mut node = Node::join(&farm, "node-a", 1);
    assert!(node.beat(&farm, &[lease]).await);
    let after = node_of(&farm, "node-a");
    assert_eq!(after, before, "the lease books what the request asked");
    assert_eq!(
        (after.0, after.1.cpu_millis, after.1.memory_bytes),
        (1, 1_000, booked)
    );
    assert!(
        node.starts().is_empty(),
        "the node has no room for the queued one"
    );
    assert_eq!(farm.queued_operations(), 1);

    assert!(accepted(&farm, &node.name, ran(lease)).await);
    farm.tick().await;
    let (next, _) = node.started();
    assert!(next > lease);
    assert!(accepted(&farm, &node.name, ran(next)).await);
    for name in [held, queued] {
        let ticket = farm.wait(&name).expect("known");
        assert!(is_answered_with_a_result(&ticket), "{name}");
    }
}

/// A legacy farm keeps the longest an action may run in an `opwall/` row beside the
/// operation's, and deletes it with them. The js core keeps the same row (it takes
/// `max_wall` at submit and restores it at start), so an operation the legacy farm walled and
/// the js core finishes goes with its `opwall/` row: the answer is delivered and every row of
/// the operation is deleted, and a legacy farm started over the store afterwards (the
/// rollback) opens it all the same.
///
/// Catches: a js core that cannot finish an operation whose wall is stored, a wall row left
/// behind by the js core, and a leftover row that stops the legacy core starting.
async fn a_legacy_walled_operation_finished_by_the_js_core_deletes_its_opwall_row(
    backing: Backing,
) {
    let disk = Disk::over(backing);
    let writer = farm_on(SchedulerCore::Legacy, &disk);
    let mut node = Node::join(&writer, "node-a", 1);
    let name = submit_walled(
        &writer,
        request(1, 4, false),
        Some(Duration::from_secs(600)),
    )
    .name;
    let (lease, _) = node.started();
    assert_eq!(
        disk.rows("opwall/").len(),
        1,
        "the legacy farm wrote no wall"
    );
    drop(node);
    drop(writer);

    let farm = farm_on(SchedulerCore::Js, &disk);
    let mut node = Node::join(&farm, "node-a", 1);
    assert!(node.beat(&farm, &[lease]).await);
    assert!(accepted(&farm, &node.name, ran(lease)).await);
    assert!(is_answered_with_a_result(&farm.wait(&name).expect("known")));
    for prefix in ["op/", "queue/", "lease/", "lease-by-op/"] {
        assert!(
            disk.rows(prefix).is_empty(),
            "{prefix} rows of finished work"
        );
    }
    assert_eq!(
        disk.rows("opwall/").len(),
        0,
        "the js core deletes the wall row with the operation's"
    );
    drop(node);
    drop(farm);

    let rolled_back = farm_on(SchedulerCore::Legacy, &disk);
    assert!(is_answered_with_a_result(
        &rolled_back.wait(&name).expect("still answerable")
    ));
}
