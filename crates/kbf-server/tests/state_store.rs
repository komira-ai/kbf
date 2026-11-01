//! The farm over a state store: what it writes before it acknowledges, and what a farm
//! started over the same store takes up again.
//!
//! The store is the in-memory fake, which keeps a durable copy a restart reopens, and
//! the farm's clock is tokio's, paused: nothing here waits. The tests that restart the
//! farm over its store run a second time over a real SQLite file, closed and reopened, and
//! a third time over a real Postgres schema (`KBF_TEST_POSTGRES_URL` names the server; those
//! tests fail without it).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use kbf_caps::NodeCaps;
use kbf_front::{Cache, Described, Dispatch, Finished, Stage, Submission};
use kbf_proto::google::rpc;
use kbf_proto::worker::{self, ServerMessage, server_message};
use kbf_sched::fence::START_GRACE;
use kbf_sched::{DaemonInstance, Request};
use kbf_server::farm::{Reported, StreamId};
use kbf_server::{EpochPolicy, Farm, ID_BLOCK, Listeners, ServeError, StateError, StateStore};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::postgres::{Flaw as PgFlaw, PostgresBackend, PostgresStore};
use kbf_state::sqlite::{Options, SqliteStore};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::{
    ActionKey, Digest, DigestFunction, LeaseId, LeaseKind, MemoryRun, OperationId, Qos, Resources,
    WorkerId,
};
use tokio::sync::mpsc;
use tonic::{Code, Status};

#[path = "state_store/ack_reasons.rs"]
mod ack_reasons;
#[path = "state_store/budgets.rs"]
mod budgets;
#[path = "state_store/conditional.rs"]
mod conditional;
#[path = "state_store/fate.rs"]
mod fate;
#[path = "state_store/floors.rs"]
mod floors;
#[path = "state_store/handover.rs"]
mod handover;
#[path = "state_store/intake.rs"]
mod intake;
#[path = "state_store/owner.rs"]
mod owner;
#[path = "state_store/result_ack.rs"]
mod result_ack;
#[path = "state_store/results.rs"]
mod results;
#[path = "state_store/shared.rs"]
mod shared;
#[path = "state_store/subject.rs"]
mod subject;
#[path = "state_store/takeover.rs"]
mod takeover;
mod thread_log;

const GIB: u64 = 1 << 30;

type TestFarm = Farm;
type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, Status>>;

/// A store whose writes can be made to fail, over the fake (which a restart reopens from
/// what was synced).
struct Flaky {
    inner: Inner,
    down: AtomicBool,
    fenced: AtomicBool,
    /// How many more writes take before the store goes down; negative for no limit.
    allow: AtomicI64,
    /// How many writes have been tried, taken or refused.
    attempts: AtomicUsize,
    /// Every write applies, then fails `Failed`: the store took it and said it did not.
    lie: AtomicBool,
    /// As `lie`, for writes that put a `lease/` row only (a grant).
    lie_lease: AtomicBool,
    /// How many more reads of a `queue/` row fail before they succeed.
    fail_queue_get: AtomicI64,
    /// How many times every `lease/` row has been read by a scan.
    lease_scans: AtomicUsize,
    /// The writes that applied, oldest first, as the transaction prints.
    applied: std::sync::Mutex<Vec<String>>,
    /// The fake's clock, which a test moves by hand; a real store's clock runs by itself.
    clock: Option<Arc<ManualClock>>,
}

/// What the store is kept in.
#[derive(Clone, Copy)]
enum Backing {
    /// The in-memory fake.
    Fake,
    /// A SQLite file in a directory of the test's own.
    Sqlite,
    /// A schema of its own in the Postgres database `KBF_TEST_POSTGRES_URL` names.
    Postgres,
}

enum Inner {
    Fake(Fake),
    Sqlite(Tracked<SqliteStore>),
    Postgres(Box<Tracked<PostgresStore>>),
}

/// A store that records when it is closed, so a test can tell a restart that closed the
/// store and opened it again from one that kept the same object.
struct Tracked<S> {
    store: S,
    closed: Arc<AtomicBool>,
}

impl<S> Tracked<S> {
    fn new(store: S) -> Self {
        Self {
            store,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl<S> Drop for Tracked<S> {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

impl Inner {
    fn store(&self) -> &dyn Store {
        match self {
            Inner::Fake(store) => store,
            Inner::Sqlite(tracked) => &tracked.store,
            Inner::Postgres(tracked) => &tracked.store,
        }
    }
}

/// A directory removed when dropped.
struct Scratch(kbf_testkit::dir::TestDir);

impl Scratch {
    fn new() -> Self {
        Self(kbf_testkit::dir::TestDir::under(
            env!("CARGO_TARGET_TMPDIR"),
            "kbf-state-store",
        ))
    }
}

fn open_sqlite(path: &std::path::Path) -> SqliteStore {
    SqliteStore::open(
        path,
        Arc::new(kbf_state::SystemClock::default()),
        Options::default(),
    )
    .expect("a SQLite store")
}

impl Flaky {
    fn new() -> Self {
        let clock = Arc::new(ManualClock::default());
        let inner = Fake::with_flaw(Arc::clone(&clock), Flaw::None);
        Self {
            clock: Some(clock),
            ..Self::over(Inner::Fake(inner))
        }
    }

    fn over(inner: Inner) -> Self {
        Self {
            inner,
            down: AtomicBool::new(false),
            fenced: AtomicBool::new(false),
            allow: AtomicI64::new(-1),
            attempts: AtomicUsize::new(0),
            lie: AtomicBool::new(false),
            lie_lease: AtomicBool::new(false),
            fail_queue_get: AtomicI64::new(0),
            lease_scans: AtomicUsize::new(0),
            applied: std::sync::Mutex::new(Vec::new()),
            clock: None,
        }
    }

    /// The store refuses writes as a superseded handle's: every write fails `Fenced`.
    fn set_fenced(&self) {
        self.fenced.store(true, Ordering::SeqCst);
    }

    /// Set once the SQLite store this wraps is dropped; none over the fake.
    fn closed(&self) -> Option<Arc<AtomicBool>> {
        match &self.inner {
            Inner::Sqlite(tracked) => Some(Arc::clone(&tracked.closed)),
            Inner::Postgres(tracked) => Some(Arc::clone(&tracked.closed)),
            Inner::Fake(_) => None,
        }
    }

    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
        if !down {
            self.allow.store(-1, Ordering::SeqCst);
        }
    }

    /// The store takes `writes` more writes, then goes down.
    fn fail_after(&self, writes: i64) {
        self.allow.store(writes, Ordering::SeqCst);
    }

    /// A crash and reopen: what the store acknowledged survives.
    fn restart(self) -> Self {
        let clock = self.clock.clone();
        let inner = match self.inner {
            Inner::Fake(fake) => Inner::Fake(fake.restart()),
            Inner::Sqlite(tracked) => {
                // Dropping the store ends its writer and releases the file's lock; the
                // file is then opened again by its path, as a new process does.
                let path = tracked.store.path().to_owned();
                drop(tracked);
                Inner::Sqlite(Tracked::new(open_sqlite(&path)))
            }
            Inner::Postgres(tracked) => {
                // The connections are dropped and opened again on the same schema, as a
                // new process does; what the old store committed is what it finds.
                let config = tracked.store.config().clone();
                drop(tracked);
                let reopened = PostgresStore::open(&config).expect("the Postgres store reopens");
                Inner::Postgres(Box::new(Tracked::new(reopened)))
            }
        };
        Self {
            clock,
            ..Self::over(inner)
        }
    }
}

impl Store for Flaky {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.allow.load(Ordering::SeqCst) == 0 {
            self.down.store(true, Ordering::SeqCst);
        } else if self.allow.load(Ordering::SeqCst) > 0 {
            self.allow.fetch_sub(1, Ordering::SeqCst);
        }
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError::Failed("the store is down".to_owned()));
        }
        if self.fenced.load(Ordering::SeqCst) {
            return Err(StoreError::Fenced);
        }
        let text = format!("{batch:?}");
        let applied = self.inner.store().apply_batch(batch)?;
        self.applied.lock().expect("log").push(text.clone());
        let lies = self.lie.load(Ordering::SeqCst)
            || (text.contains("lease/") && self.lie_lease.load(Ordering::SeqCst));
        if lies {
            return Err(StoreError::Failed(
                "applied, but not acknowledged".to_owned(),
            ));
        }
        Ok(applied)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        if key.starts_with("queue/") && self.fail_queue_get.load(Ordering::SeqCst) > 0 {
            self.fail_queue_get.fetch_sub(1, Ordering::SeqCst);
            return Err(StoreError::Failed("the read failed".to_owned()));
        }
        self.inner.store().get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        let scans = &self.lease_scans;
        scans.fetch_add(usize::from(prefix == "lease/"), Ordering::SeqCst);
        self.inner.store().scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.store().scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.store().changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.store().reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.store().commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.store().now()
    }
}

/// The store, and farms started over it one after another.
struct Disk {
    store: Arc<Flaky>,
    /// Kept for the SQLite file, and removed after the store is closed.
    _dir: Option<Scratch>,
    /// Kept for the Postgres schema, dropped with it after the store is closed. Declared
    /// after `store`, so the store goes first.
    _postgres: Option<PostgresBackend>,
}

impl Disk {
    fn new() -> Self {
        Self::over(Backing::Fake)
    }

    fn over(backing: Backing) -> Self {
        let (inner, dir) = match backing {
            Backing::Fake => (None, None),
            Backing::Sqlite => {
                let dir = Scratch::new();
                let store = open_sqlite(&dir.0.join("state.db"));
                (Some(Inner::Sqlite(Tracked::new(store))), Some(dir))
            }
            Backing::Postgres => (None, None),
        };
        let postgres = (matches!(backing, Backing::Postgres)).then(|| {
            let url = kbf_testkit::database::server_url();
            PostgresBackend::new(&url, PgFlaw::None)
        });
        let inner = match &postgres {
            Some(backend) => {
                let store = PostgresStore::open(&backend.fresh_config()).expect("a Postgres store");
                Some(Inner::Postgres(Box::new(Tracked::new(store))))
            }
            None => inner,
        };
        let store = inner.map_or_else(Flaky::new, Flaky::over);
        Self {
            store: Arc::new(store),
            _dir: dir,
            _postgres: postgres,
        }
    }

    fn farm(&self, epoch: EpochPolicy) -> TestFarm {
        self.try_farm(epoch).expect("a farm over the store")
    }

    fn try_farm(&self, epoch: EpochPolicy) -> Result<TestFarm, StateError> {
        let store: Arc<dyn Store> = self.store.clone();
        Farm::new(
            Arc::new(Cache::memory()),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        )
        .with_state(Some(StateStore {
            store,
            epoch,
            members: Default::default(),
            history: None,
        }))
    }

    /// [`Disk::farm`] with `EpochPolicy::Keep`, keeping at most `bound` memory floors.
    fn farm_bounded(&self, bound: usize) -> TestFarm {
        let store: Arc<dyn Store> = self.store.clone();
        Farm::new(
            Arc::new(Cache::memory()),
            kbf_sched::UNSERVABLE_WAIT,
            kbf_sched::FINISHED_RETENTION,
        )
        .with_memory_floors(bound)
        .with_state(Some(StateStore {
            store,
            epoch: EpochPolicy::Keep,
            members: Default::default(),
            history: None,
        }))
        .expect("a farm over the store")
    }

    /// The process dies: its farm is gone, and the store reopens as the disk left it.
    /// Over SQLite the old store must be closed and the file opened again (not the same
    /// object kept), and the new store must read every row the old one held.
    fn crash(&mut self, farm: TestFarm) {
        drop(farm);
        let closed = self.store.closed();
        let rows = closed.as_ref().map(|_| self.all_rows());
        // The placeholder holds no file: a SQLite store must be closed before it reopens.
        let store = Arc::into_inner(std::mem::replace(&mut self.store, Arc::new(Flaky::new())));
        self.store = Arc::new(store.expect("nothing else holds the store").restart());
        if let (Some(closed), Some(rows)) = (closed, rows) {
            assert!(
                closed.load(Ordering::SeqCst),
                "the restart kept the store open"
            );
            let reopened = self.store.closed().expect("a SQLite store");
            assert!(!Arc::ptr_eq(&closed, &reopened), "the same store object");
            assert!(!reopened.load(Ordering::SeqCst), "the new store is open");
            assert!(!rows.is_empty(), "the test wrote nothing before the crash");
            assert_eq!(
                self.all_rows(),
                rows,
                "the reopened file lost what was written"
            );
        }
    }

    fn all_rows(&self) -> Vec<(String, Versioned)> {
        self.store.scan_prefix("").expect("scan")
    }

    fn rows(&self, prefix: &str) -> usize {
        self.store.scan_prefix(prefix).expect("scan").len()
    }

    /// Lets `by` pass on the store's clock: the fake's is moved by hand, and a real store's
    /// runs by itself, so the test waits.
    fn pass(&self, by: Duration) {
        let waits = || std::thread::sleep(by);
        self.store
            .clock
            .as_ref()
            .map_or_else(waits, |clock| clock.advance(by));
    }
}

/// The digest of the action `request(tag)` asks to run.
fn action_of(tag: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag))
}

fn request(tag: u8) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: action_of(tag),
        }
        .into(),
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources: Resources::new(1_000, GIB),
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
        whole_worker: false,
    }
}

fn submit(farm: &TestFarm, request: Request) -> Result<(), Status> {
    farm.submit(Submission {
        request,
        call: None,
        described: Described::default(),
        evidence: Default::default(),
        max_wall: None,
    })
    .map(drop)
}

fn node() -> WorkerId {
    WorkerId::new("node-a")
}

/// Registers `node-a` with room for four actions, as a daemon opening a stream does.
fn register(farm: &TestFarm) -> (StreamId, Inbox) {
    register_as(farm, &node())
}

/// Registers `worker` the same way.
fn register_as(farm: &TestFarm, worker: &WorkerId) -> (StreamId, Inbox) {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let reported = Reported::serving(Resources::new(4_000, 8 * GIB), caps);
    let (outbound, inbox) = mpsc::unbounded_channel();
    let stream = farm.register(
        worker,
        DaemonInstance::new("daemon"),
        DaemonInstance::new(""),
        reported,
        outbound,
        ServerMessage::default(),
    );
    (stream, inbox)
}

/// The leases of the `Start`s waiting in `inbox`, oldest first.
fn starts(inbox: &mut Inbox) -> Vec<LeaseId> {
    let mut leases = Vec::new();
    while let Ok(message) = inbox.try_recv() {
        if let Some(server_message::Message::Start(start)) = message.expect("no error").message {
            let lease = start.lease_id.expect("a lease id");
            leases.push(LeaseId::new(lease.term, lease.seq));
        }
    }
    leases
}

/// A result of `lease` the farm accepts without a cache write: INVALID_ARGUMENT.
fn invalid(lease: LeaseId) -> worker::Result {
    worker::Result {
        lease_id: Some(worker::LeaseId {
            term: lease.term,
            seq: lease.seq,
        }),
        status: Some(rpc::Status {
            code: Code::InvalidArgument as i32,
            message: "test".to_owned(),
            details: Vec::new(),
        }),
        action_result: None,
        action_digest: None,
        memory_kill: 0,
        lost: 0,
        job_outcome: None,
    }
}

/// Whether the farm answers a result of `lease` with an ack that says accepted. A result
/// the store has not taken is not acknowledged at all (`Farm::report` is `None`), which is
/// not accepted either.
async fn accepted(farm: &TestFarm, lease: LeaseId) -> bool {
    let ack = farm.report(&node(), invalid(lease)).await;
    ack.is_some_and(|ack| ack.accepted)
}

/// Catches: a farm that forgets a claim it acknowledged (the daemon runs a lease the
/// restarted farm has never heard of and refuses its result), and one that accepts it
/// for another reason than the store (the control, a farm with no store, refuses it).
async fn a_restart_replays_the_claims_it_acknowledged(backing: Backing) {
    let mut disk = Disk::over(backing);
    let before = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&before);
    submit(&before, request(1)).expect("submitted");
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    assert_eq!(
        disk.rows("session/"),
        1,
        "the registration is a session row"
    );
    disk.crash(before);

    let control = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    let (stream, _inbox) = register(&control);
    assert!(control.heartbeat(&node(), stream, 1, vec![lease]).await);
    assert!(
        !accepted(&control, lease).await,
        "a farm with no store knows the lease"
    );

    let after = disk.farm(EpochPolicy::Keep);
    assert_eq!(
        after.lease_epoch(),
        lease.term,
        "the store's epoch is the lease epoch"
    );
    let (stream, _inbox) = register(&after);
    assert!(after.heartbeat(&node(), stream, 1, vec![lease]).await);
    assert!(accepted(&after, lease).await);
    assert_eq!(
        disk.rows("op/"),
        0,
        "the finished operation is gone from the store"
    );
    assert_eq!(disk.rows("lease/"), 0);
}

/// Catches: a scheduler rebuilt from the store that loses the running lease the daemon
/// still holds (the run is wasted), keeps one the daemon never received (the operation
/// never runs), grants a lease id again (the old run's result taken for the new one), or
/// accepts a result of the lease it gave up.
async fn after_a_kill_running_leases_are_rebound_and_undelivered_ones_requeued(backing: Backing) {
    let mut disk = Disk::over(backing);
    let before = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&before);
    submit(&before, request(1)).expect("submitted");
    submit(&before, request(2)).expect("submitted");
    let [running, undelivered] = starts(&mut inbox)[..] else {
        panic!("two leases expected");
    };
    disk.crash(before);

    let after = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&after);
    // The daemon received the first `Start`, not the second.
    assert!(after.heartbeat(&node(), stream, 1, vec![running]).await);
    let regranted = starts(&mut inbox);
    assert_eq!(
        regranted,
        [LeaseId::new(running.term, ID_BLOCK)],
        "a new lease for the second only, numbered in the farm's own block"
    );
    assert!(
        !accepted(&after, undelivered).await,
        "the given-up lease's result"
    );
    assert!(accepted(&after, running).await, "the re-bound run's result");
    assert!(accepted(&after, regranted[0]).await);
    assert_eq!(disk.rows("op/"), 0);
}

/// Asserts that the epoch follows the policy a start is given (kept by one that keeps it,
/// raised by one by one that bumps it) and that the leases are granted under the term
/// `Welcome` names, which is the epoch.
async fn the_epoch_follows_the_policy_and_names_the_leases(backing: Backing) {
    let mut disk = Disk::over(backing);
    let first = disk.farm(EpochPolicy::Keep);
    let epoch = first.lease_epoch();
    assert_eq!(first.term(), epoch);
    submit(&first, request(1)).expect("queued, no worker yet");
    disk.crash(first);

    let kept = disk.farm(EpochPolicy::Keep);
    assert_eq!(kept.lease_epoch(), epoch);
    disk.crash(kept);

    let bumped = disk.farm(EpochPolicy::Bump);
    assert_eq!(bumped.lease_epoch(), epoch + 1);
    assert_eq!(bumped.term(), epoch + 1);
    let (_, mut inbox) = register(&bumped);
    // Each of the three starts reserved a block of lease sequences: the third numbers its
    // leases from the third block.
    assert_eq!(starts(&mut inbox), [LeaseId::new(epoch + 1, 2 * ID_BLOCK)]);
}

/// Catches: a request acknowledged to its client that the store did not take (a restart
/// would forget work the client believes queued): the submission must fail UNAVAILABLE
/// and queue nothing.
#[tokio::test(start_paused = true)]
async fn a_request_the_store_refuses_is_not_queued() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    disk.store.set_down(true);
    let refused = submit(&farm, request(1)).expect_err("refused");
    assert_eq!(refused.code(), Code::Unavailable);
    disk.store.set_down(false);
    let (_, mut inbox) = register(&farm);
    assert!(
        starts(&mut inbox).is_empty(),
        "a refused request was queued"
    );
    assert_eq!(disk.rows("op/"), 0);
}

/// Catches: a `Start` sent for a grant the store did not take (the daemon would run a
/// lease a restart forgets), and a farm that does not recover from it: the grant that
/// never started is given up after the start grace and granted again, once the store
/// takes it. (The submission's write takes; the grant's, the next, does not.)
#[tokio::test(start_paused = true)]
async fn a_grant_the_store_refuses_is_not_started_and_is_granted_again() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&farm);
    disk.store.fail_after(1);
    submit(&farm, request(1)).expect("queued");
    assert!(
        starts(&mut inbox).is_empty(),
        "a Start went out for an unwritten grant"
    );
    disk.store.set_down(false);

    tokio::time::advance(kbf_sched::fence::START_GRACE + Duration::from_secs(1)).await;
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    let regranted = starts(&mut inbox);
    assert_eq!(regranted, [LeaseId::new(farm.lease_epoch(), 1)]);
    assert_eq!(disk.rows("lease/"), 1);
    assert_eq!(disk.rows("queue/"), 0);
}

/// Catches: a grant made while the store is known to be down (it cannot be written, so
/// its `Start` is dropped round after round, each burning a lease sequence): nothing is
/// placed from the first refused write until one takes, and the work is granted at once
/// then.
#[tokio::test(start_paused = true)]
async fn nothing_is_granted_while_the_store_is_down() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    submit(&farm, request(1)).expect("queued, no worker yet");
    disk.store.set_down(true);
    let (stream, mut inbox) = register(&farm);
    assert!(starts(&mut inbox).is_empty());
    farm.tick().await;
    disk.store.set_down(false);
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    assert_eq!(
        starts(&mut inbox),
        [LeaseId::new(farm.lease_epoch(), 0)],
        "no lease sequence was spent while the store was down"
    );
    assert_eq!(disk.rows("lease/"), 1);
}

/// Catches: a superseded scheduler (its writes fail Fenced) treated as a store outage
/// (pinging forever, answering UNAVAILABLE so clients retry it) or still granting work: it
/// must answer FAILED_PRECONDITION, grant nothing, and stay refusing when writes could
/// again succeed.
#[tokio::test(start_paused = true)]
async fn a_superseded_scheduler_refuses_work_and_does_not_wait_for_the_store() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&farm);
    disk.store.set_fenced();
    let refused = submit(&farm, request(1)).expect_err("refused");
    assert_eq!(refused.code(), Code::FailedPrecondition);
    farm.tick().await;
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    assert!(
        starts(&mut inbox).is_empty(),
        "a superseded scheduler granted"
    );
    assert_eq!(disk.rows("op/"), 0);
    assert_eq!(disk.rows("lease/"), 0);
}

/// Catches: a lease given up while the store is never told of.
/// The store would still hold the old lease and no queue row, so the grant that follows
/// is refused for want of a queue row, its `Start` is dropped, and the operation never
/// runs until a restart. Once the store is back, the given-up lease is written and the
/// operation is granted again.
#[tokio::test(start_paused = true)]
async fn a_requeue_the_store_refused_is_written_when_it_returns() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&farm);
    submit(&farm, request(1)).expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1);
    disk.store.set_down(true);
    tokio::time::advance(kbf_sched::fence::LEASE_GRACE + Duration::from_secs(1)).await;
    farm.tick().await;
    assert_eq!(
        disk.rows("lease/"),
        1,
        "the store was down: it holds the lease"
    );
    disk.store.set_down(false);

    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    let regranted = starts(&mut inbox);
    assert_eq!(
        regranted,
        [LeaseId::new(farm.lease_epoch(), 1)],
        "the operation was not granted again"
    );
    assert_eq!((disk.rows("lease/"), disk.rows("queue/")), (1, 0));
    assert!(accepted(&farm, regranted[0]).await);
    assert_eq!(disk.rows("op/"), 0);
}

/// Pins (it passes without the repair above): a start that bumps the epoch over a store
/// holding a granted lease takes the lease up, and when the daemon, which kills the runs
/// of an earlier epoch, does not list it, requeues it at once under the new term. The
/// old lease's result is refused.
async fn a_bumped_epoch_requeues_the_old_term_leases_the_daemon_killed(backing: Backing) {
    let mut disk = Disk::over(backing);
    let before = disk.farm(EpochPolicy::Keep);
    let epoch = before.lease_epoch();
    let (_, mut inbox) = register(&before);
    submit(&before, request(1)).expect("submitted");
    let [old] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    assert_eq!(old.term, epoch);
    disk.crash(before);

    let after = disk.farm(EpochPolicy::Bump);
    assert_eq!(after.lease_epoch(), epoch + 1);
    let (stream, mut inbox) = register(&after);
    assert!(after.heartbeat(&node(), stream, 1, Vec::new()).await);
    assert_eq!(starts(&mut inbox), [LeaseId::new(epoch + 1, ID_BLOCK)]);
    assert!(!accepted(&after, old).await, "the old term's result");
    assert_eq!(disk.rows("lease/"), 1);
}

/// Catches: an outage that makes every lease look expired the moment the store returns.
/// The farm could not write, so it could not hear its workers either; the lease must not
/// be given up (and, here, requeued in the store) until the grace after the store's return
/// has passed, and a lease nobody renewed goes after it.
#[tokio::test(start_paused = true)]
async fn an_outage_requeues_nothing_inside_the_grace_and_the_unrenewed_lease_after_it() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_stream, mut inbox) = register(&farm);
    submit(&farm, request(1)).expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1);

    disk.store.set_down(true);
    let refused = submit(&farm, request(2)).expect_err("the outage is noticed");
    assert_eq!(refused.code(), Code::Unavailable);
    // The farm's clock runs on through the outage; nobody is heard meanwhile.
    tokio::time::advance(kbf_sched::fence::LEASE_GRACE * 10).await;
    farm.tick().await;
    disk.store.set_down(false);
    farm.tick().await;
    assert_eq!(
        (disk.rows("lease/"), disk.rows("queue/")),
        (1, 0),
        "the lease was given up the moment the store returned"
    );

    tokio::time::advance(kbf_sched::fence::LEASE_GRACE - Duration::from_secs(5)).await;
    farm.tick().await;
    assert_eq!((disk.rows("lease/"), disk.rows("queue/")), (1, 0));
    assert!(starts(&mut inbox).is_empty());

    tokio::time::advance(Duration::from_secs(10)).await;
    farm.tick().await;
    assert_eq!(
        (disk.rows("lease/"), disk.rows("queue/")),
        (0, 1),
        "the unrenewed lease outlived the grace"
    );
}

/// Catches: renewal per lease (a heartbeat of a node holding three leases writing three
/// rows, or any write per heartbeat) instead of one row per daemon session per interval;
/// a renewal that touches a lease row (the fencing token is written at the grant only);
/// and a session row that never expires (a daemon that left would look live for ever).
#[tokio::test(start_paused = true)]
async fn a_daemon_session_is_renewed_once_per_interval_not_per_lease() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&farm);
    for tag in 1..=3 {
        submit(&farm, request(tag)).expect("submitted");
    }
    let leases = starts(&mut inbox);
    assert_eq!(leases.len(), 3);
    let session = || {
        let row = disk.store.get("session/node-a").expect("get");
        row.expect("a session row")
    };
    let first = session();
    assert!(first.expires_at.is_some(), "a session row must expire");
    let lease_rows = disk.store.scan_prefix("lease/").expect("scan");
    let writes = disk.store.commit_stats().txns;

    let mut seq = 0;
    for _ in 0..5 {
        tokio::time::advance(Duration::from_secs(1)).await;
        seq += 1;
        assert!(farm.heartbeat(&node(), stream, seq, leases.clone()).await);
    }
    assert_eq!(
        disk.store.commit_stats().txns,
        writes,
        "a heartbeat inside the interval wrote"
    );

    tokio::time::advance(kbf_server::SESSION_RENEWAL).await;
    seq += 1;
    assert!(farm.heartbeat(&node(), stream, seq, leases).await);
    assert_eq!(
        disk.store.commit_stats().txns,
        writes + 1,
        "one write renews the session, whatever it holds"
    );
    let renewed = session();
    assert!(renewed.version > first.version);
    assert!(renewed.expires_at > first.expires_at);
    assert_eq!(disk.store.scan_prefix("lease/").expect("scan"), lease_rows);
}

/// Catches: a write refused as stale (its row is gone: the store working) taken for an
/// outage, which would hold every lease for the grace: a result whose lease row was
/// deleted behind the farm's back is dropped, and a silent worker's other lease is
/// still given up at the lease grace.
#[tokio::test(start_paused = true)]
async fn a_write_refused_as_stale_is_not_an_outage() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_stream, mut inbox) = register(&farm);
    submit(&farm, request(1)).expect("submitted");
    submit(&farm, request(2)).expect("submitted");
    let [first, _second] = starts(&mut inbox)[..] else {
        panic!("two leases expected");
    };
    let stale = Txn::new().delete(
        format!("lease/{:016x}.{:016x}", first.term, first.seq),
        kbf_state::Cond::Any,
    );
    disk.store.txn(stale).expect("store").expect("delete");
    assert!(
        !accepted(&farm, first).await,
        "a result whose lease row is gone was acknowledged"
    );
    assert_eq!(disk.rows("op/"), 2);

    tokio::time::advance(kbf_sched::fence::LEASE_GRACE + Duration::from_secs(1)).await;
    farm.tick().await;
    assert_eq!(
        disk.rows("queue/"),
        1,
        "the silent worker's lease was held for an outage's grace"
    );
}

/// Catches: a session renewal the store refused that is not tried again (the row would
/// expire under a daemon that heartbeats): the next heartbeat writes it.
#[tokio::test(start_paused = true)]
async fn a_session_renewal_the_store_refused_is_tried_at_the_next_heartbeat() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, _inbox) = register(&farm);
    let session = || {
        disk.store
            .get("session/node-a")
            .expect("get")
            .expect("a row")
    };
    let first = session();
    tokio::time::advance(kbf_server::SESSION_RENEWAL + Duration::from_secs(1)).await;
    disk.store.set_down(true);
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    assert_eq!(session().version, first.version);
    disk.store.set_down(false);
    assert!(farm.heartbeat(&node(), stream, 2, Vec::new()).await);
    assert!(
        session().version > first.version,
        "the refused renewal was not tried again"
    );
}

/// Loses operation 1's lease as a daemon that reconnected without it does, on `farm`: a
/// new session of the daemon, and a heartbeat that does not list the lease. Returns the
/// new session's stream and inbox.
async fn lose_lease(farm: &TestFarm) -> (StreamId, Inbox) {
    let (stream, inbox) = register(farm);
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    (stream, inbox)
}

/// Catches: a loss count that a restart forgets (an action that kills the scheduler's
/// host starts from none each time and loops for ever): two losses are written, survive a
/// crash, and the third, the recovered lease's, refuses the operation, whose rows then go.
async fn lost_leases_are_counted_across_a_restart_and_the_third_is_refused(backing: Backing) {
    let mut disk = Disk::over(backing);
    let before = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&before);
    submit(&before, request(1)).expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1);
    for lost in 1..=2u8 {
        let (_, mut inbox) = lose_lease(&before).await;
        assert_eq!(
            starts(&mut inbox).len(),
            1,
            "granted again after loss {lost}"
        );
        assert_eq!(disk.rows("loss/"), 1, "the count is written");
    }
    disk.crash(before);
    assert_eq!(disk.rows("loss/"), 1, "the count survived the restart");
    assert_eq!(disk.rows("op/"), 1, "the operation survived the restart");

    let after = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&after);
    assert!(after.heartbeat(&node(), stream, 1, Vec::new()).await);
    assert!(
        starts(&mut inbox).is_empty(),
        "a third lost lease was granted again"
    );
    assert_eq!(disk.rows("op/"), 0, "the refused operation was kept");
    assert_eq!(disk.rows("loss/"), 0, "its count was kept");
    assert_eq!((disk.rows("lease/"), disk.rows("queue/")), (0, 0));
}

/// Catches: a refusal that leaves the caller waiting, or answers with another code than
/// FAILED_PRECONDITION (the code of every refusal), or with no reason.
#[tokio::test(start_paused = true)]
async fn the_third_lost_lease_answers_the_caller_with_the_reason() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let ticket = farm
        .submit(Submission {
            request: request(1),
            call: None,
            described: Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1);
    for _ in 1..=2 {
        let (_, mut inbox) = lose_lease(&farm).await;
        assert_eq!(starts(&mut inbox).len(), 1);
    }
    assert!(!matches!(*ticket.stage.borrow(), Stage::Done(_)));
    let (_, mut inbox) = lose_lease(&farm).await;
    assert!(starts(&mut inbox).is_empty());
    let Stage::Done(Finished::Failed(status)) = ticket.stage.borrow().clone() else {
        panic!("the caller was not answered");
    };
    assert_eq!(status.code, Code::FailedPrecondition as i32);
    assert!(status.message.contains("lost"), "{}", status.message);
}

/// Asserts that a lease given up for silence and a refusal are removed from the store: no
/// lease is left as held (a restart would re-bind a run nobody holds), and no refused
/// operation is left (it would be refused again, or run). The silent daemon's session row
/// has expired by the time the operation is refused (the fake's clock stands still, so
/// the test deletes it): a session row that is there is a daemon that could run the
/// operation.
#[tokio::test(start_paused = true)]
async fn a_given_up_lease_and_a_refusal_are_written() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    submit(&farm, request(1)).expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1);
    assert_eq!((disk.rows("lease/"), disk.rows("queue/")), (1, 0));
    tokio::time::advance(kbf_sched::fence::LEASE_GRACE + Duration::from_secs(1)).await;
    farm.tick().await;
    assert_eq!(
        (disk.rows("lease/"), disk.rows("queue/")),
        (0, 1),
        "the lease was kept"
    );

    let expired = Txn::new().delete("session/node-a", kbf_state::Cond::Any);
    disk.store.txn(expired).expect("store").expect("delete");
    tokio::time::advance(kbf_sched::UNSERVABLE_WAIT + Duration::from_secs(1)).await;
    farm.tick().await;
    assert_eq!(disk.rows("op/"), 0, "the refused operation was kept");
    assert_eq!(disk.rows("queue/"), 0);
}

/// Catches: a second caller for the same action written as another operation (the work
/// would run twice after a restart), and a networked action's twin joined (those runs are
/// not interchangeable).
#[tokio::test(start_paused = true)]
async fn a_twin_joins_and_writes_no_operation() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    submit(&farm, request(1)).expect("first");
    submit(&farm, request(1)).expect("twin");
    assert_eq!(disk.rows("op/"), 1);
    let networked = Request {
        hermetic: false,
        ..request(1)
    };
    submit(&farm, networked.clone()).expect("networked");
    submit(&farm, networked).expect("networked again");
    assert_eq!(disk.rows("op/"), 3);
}

/// Catches: a store holding a row the scheduler did not write taken as empty (work
/// silently lost at a start): the farm must refuse to start over it.
#[tokio::test(start_paused = true)]
async fn a_store_with_an_unreadable_row_refuses_the_farm() {
    let disk = Disk::new();
    let put = Txn::new().put("op/0000000000000001", vec![0xff], kbf_state::Cond::Any);
    disk.store.txn(put).expect("store").expect("put");
    let refused = disk.try_farm(EpochPolicy::Keep).expect_err("refused");
    assert!(matches!(refused, StateError::Corrupt { .. }), "{refused:?}");
}

use conditional::{
    a_daemon_of_another_frontend_holds_back_the_refusal,
    a_refusal_loses_to_a_grant_made_by_another_frontend,
    a_result_reaching_the_frontend_a_daemon_left_finishes_nothing,
    a_second_give_up_queues_nothing_beside_the_new_lease,
    daemon_sessions_that_cannot_be_read_hold_back_a_refusal,
};
use intake::{
    a_client_waiting_on_one_frontend_gets_the_result_another_recorded,
    a_cordon_set_through_one_frontend_stops_grants_on_the_other,
    a_drain_deadline_is_on_the_stores_clock_and_applied_by_the_other,
    a_frontend_killed_mid_run_loses_nothing,
    a_lost_grant_race_forgets_the_placement_and_the_operation_runs_once,
    an_operation_queued_through_one_frontend_runs_on_a_daemon_of_another,
    starting_a_frontend_leaves_the_leases_of_the_others_alone,
    the_epoch_an_operator_raised_is_reported_by_the_other_frontend,
};
use owner::{
    a_claim_the_store_refused_is_made_when_it_returns,
    a_daemon_that_moves_to_another_frontend_keeps_its_lease,
    a_give_up_for_a_node_the_daemon_left_is_dropped_once,
    a_hello_to_the_frontend_that_holds_the_lease_takes_nothing_in,
    a_joined_submit_finds_its_lease_through_the_index,
    a_new_process_that_moves_waits_before_its_unlisted_lease_is_lost,
    a_result_for_a_lease_numbered_below_the_one_taken_up_is_never_refused,
    acks_stop_when_session_renewals_fail, acks_stop_when_the_daemon_moves,
    an_ack_waits_for_the_session_row,
    an_owner_row_that_cannot_be_read_blocks_the_claim_until_the_next_hello,
    the_frontend_a_daemon_left_drops_only_that_nodes_writes,
    the_same_process_that_moves_loses_an_unlisted_lease_at_once,
    two_frontends_never_both_grant_to_the_same_worker,
};
use results::{
    a_client_waiting_through_another_frontend_gets_the_result,
    a_joiner_of_a_leased_operation_never_deletes_its_lease,
    a_name_in_the_new_form_is_answered_by_every_frontend_at_every_stage,
    an_old_form_name_is_answered_by_the_issuing_frontend,
};
use shared::{
    a_second_frontend_started_over_the_store_keeps_its_epoch,
    frontends_opening_an_empty_store_together_keep_one_epoch,
    frontends_over_one_store_never_hand_out_the_same_id, the_operator_bump_raises_the_epoch_once,
};

/// Runs each restart test over the fake, over a SQLite file and over a Postgres schema: what the fake keeps
/// across a crash is what the real store must.
macro_rules! restarts {
    ($($name:ident),* $(,)?) => {$(
        mod $name {
            use super::*;

            #[tokio::test(start_paused = true)]
            async fn over_the_fake() {
                super::$name(Backing::Fake).await;
            }

            #[tokio::test(start_paused = true)]
            async fn over_sqlite() {
                super::$name(Backing::Sqlite).await;
            }

            #[tokio::test(start_paused = true)]
            async fn over_postgres() {
                super::$name(Backing::Postgres).await;
            }
        }
    )*};
}

restarts!(
    a_restart_replays_the_claims_it_acknowledged,
    after_a_kill_running_leases_are_rebound_and_undelivered_ones_requeued,
    the_epoch_follows_the_policy_and_names_the_leases,
    a_bumped_epoch_requeues_the_old_term_leases_the_daemon_killed,
    frontends_over_one_store_never_hand_out_the_same_id,
    a_second_frontend_started_over_the_store_keeps_its_epoch,
    the_operator_bump_raises_the_epoch_once,
    frontends_opening_an_empty_store_together_keep_one_epoch,
    a_second_give_up_queues_nothing_beside_the_new_lease,
    a_result_reaching_the_frontend_a_daemon_left_finishes_nothing,
    a_daemon_of_another_frontend_holds_back_the_refusal,
    a_refusal_loses_to_a_grant_made_by_another_frontend,
    daemon_sessions_that_cannot_be_read_hold_back_a_refusal,
    a_daemon_that_moves_to_another_frontend_keeps_its_lease,
    a_give_up_for_a_node_the_daemon_left_is_dropped_once,
    a_hello_to_the_frontend_that_holds_the_lease_takes_nothing_in,
    the_frontend_a_daemon_left_drops_only_that_nodes_writes,
    two_frontends_never_both_grant_to_the_same_worker,
    a_joined_submit_finds_its_lease_through_the_index,
    acks_stop_when_session_renewals_fail,
    an_ack_waits_for_the_session_row,
    acks_stop_when_the_daemon_moves,
    a_new_process_that_moves_waits_before_its_unlisted_lease_is_lost,
    a_result_for_a_lease_numbered_below_the_one_taken_up_is_never_refused,
    the_same_process_that_moves_loses_an_unlisted_lease_at_once,
    a_claim_the_store_refused_is_made_when_it_returns,
    an_owner_row_that_cannot_be_read_blocks_the_claim_until_the_next_hello,
    lost_leases_are_counted_across_a_restart_and_the_third_is_refused,
    a_crash_at_any_write_of_a_finish_leaves_the_operation_or_its_result,
    a_crash_at_any_write_of_a_refusal_leaves_the_operation_or_its_refusal,
    an_operation_queued_through_one_frontend_runs_on_a_daemon_of_another,
    a_lost_grant_race_forgets_the_placement_and_the_operation_runs_once,
    a_client_waiting_on_one_frontend_gets_the_result_another_recorded,
    a_frontend_killed_mid_run_loses_nothing,
    a_cordon_set_through_one_frontend_stops_grants_on_the_other,
    a_drain_deadline_is_on_the_stores_clock_and_applied_by_the_other,
    starting_a_frontend_leaves_the_leases_of_the_others_alone,
    the_epoch_an_operator_raised_is_reported_by_the_other_frontend,
    a_client_waiting_through_another_frontend_gets_the_result,
    a_name_in_the_new_form_is_answered_by_every_frontend_at_every_stage,
    a_joiner_of_a_leased_operation_never_deletes_its_lease
);

/// `an_old_form_name_is_answered_by_the_issuing_frontend` over each store, as `restarts!` runs
/// a test, written out so that the js core's difference can be marked on it.
mod an_old_form_name_is_answered_by_the_issuing_frontend {
    use super::*;

    #[tokio::test(start_paused = true)]
    #[cfg_attr(
        feature = "farm-core-js",
        ignore = "known-different-on-js: a name of the old form operations/<term>-<waiter> is not answered"
    )]
    async fn over_the_fake() {
        super::an_old_form_name_is_answered_by_the_issuing_frontend(Backing::Fake).await;
    }

    #[tokio::test(start_paused = true)]
    #[cfg_attr(
        feature = "farm-core-js",
        ignore = "known-different-on-js: a name of the old form operations/<term>-<waiter> is not answered"
    )]
    async fn over_sqlite() {
        super::an_old_form_name_is_answered_by_the_issuing_frontend(Backing::Sqlite).await;
    }

    #[tokio::test(start_paused = true)]
    #[cfg_attr(
        feature = "farm-core-js",
        ignore = "known-different-on-js: a name of the old form operations/<term>-<waiter> is not answered"
    )]
    async fn over_postgres() {
        super::an_old_form_name_is_answered_by_the_issuing_frontend(Backing::Postgres).await;
    }
}

/// A state store opened as a start opens it: from the flag, over `dir`.
fn store_from_flag(dir: &std::path::Path) -> Result<StateStore, StateError> {
    let uri = format!("--state-store=sqlite://{}", dir.join("state.db").display());
    let args = <kbf_server::Args as clap::Parser>::parse_from(["kbf-server", &uri]);
    args.open_state_store()
        .map(|state| state.expect("the flag is given"))
}

fn farm_over(state: StateStore) -> TestFarm {
    Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_state(Some(state))
    .expect("a farm over the store")
}

/// Catches: `--state-store=sqlite:///...` parsed and then not opened (the scheduler runs
/// in memory while the operator believes its queue is kept): an operation submitted
/// through a farm over the flag's store is granted by the farm a restart starts over the
/// same file, with the term the file holds: a SQLite start keeps the epoch.
#[tokio::test(start_paused = true)]
async fn an_operation_submitted_over_the_flag_survives_a_restart_of_the_file() {
    let dir = Scratch::new();
    let before = farm_over(store_from_flag(&dir.0).expect("a first start"));
    let epoch = before.lease_epoch();
    submit(&before, request(1)).expect("queued, no worker yet");
    drop(before);

    let after = farm_over(store_from_flag(&dir.0).expect("a second start"));
    assert_eq!(after.lease_epoch(), epoch, "a SQLite start moved the epoch");
    let (_, mut inbox) = register(&after);
    let leases = starts(&mut inbox);
    assert_eq!(leases.len(), 1, "the queued operation was recovered");
    assert_eq!(leases[0].term, epoch);
}

/// Catches: a second frontend started on a state file another holds (two schedulers
/// would hand out the same leases): refused naming the lock, and the file is free again
/// once the first goes.
#[test]
fn a_second_frontend_on_the_same_sqlite_file_is_refused() {
    let dir = Scratch::new();
    let first = store_from_flag(&dir.0).expect("a first start");
    let refused = store_from_flag(&dir.0).err().expect("the file is held");
    assert!(matches!(refused, StateError::Open(_)), "{refused:?}");
    let said = refused.to_string();
    assert!(said.contains("another kbf process"), "{said}");
    assert!(said.contains("state.db.lock"), "{said}");
    drop(first);
    store_from_flag(&dir.0).expect("free again");
}

fn listeners() -> Listeners {
    let loopback = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
    Listeners {
        reapi: loopback,
        worker: loopback,
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: Duration::from_secs(1),
        hello_wait: Duration::from_secs(1),
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        default_platform: kbf_front::DefaultPlatform::NONE,
        qos_allowed: kbf_front::QosAllow::DEFAULT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        memory_climb: true,
        vm_host_floor: kbf_server::HostFloor::default(),
        learned_sizes: kbf_server::LearnedSizes::Off,
        exclusive_actions: false,
        scheduler_core: kbf_server::SchedulerCore::under_test(),
        placement_load: kbf_server::PlacementLoad::Off,
        placement_predictions: kbf_server::PlacementPredictions::Off,
        usage_profiles: kbf_server::UsageProfiles::Off,
        whole_node: None,
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

/// Catches: a server that binds with a state store and ignores it (the scheduler would
/// run in memory while the operator believes it durable), and one that starts over a
/// store it cannot read.
#[tokio::test]
async fn a_server_binds_over_a_state_store_or_refuses_it() {
    let disk = Disk::new();
    let state = |disk: &Disk| StateStore {
        store: disk.store.clone(),
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    };
    let bind = |state| {
        kbf_server::bind_server_with_state(
            Arc::new(Cache::memory()),
            listeners(),
            None,
            kbf_auth::Policy::allow_all(),
            Some(state),
            std::future::pending::<()>(),
        )
    };
    let bound = bind(state(&disk)).expect("binds over an empty store");
    drop(bound);
    assert_eq!(
        disk.rows("meta/epoch"),
        1,
        "the server settled the epoch in its store"
    );

    let put = Txn::new().put("op/0000000000000001", vec![0xff], kbf_state::Cond::Any);
    disk.store.txn(put).expect("store").expect("put");
    let refused = bind(state(&disk)).err().expect("refused");
    assert!(
        matches!(refused, ServeError::State(StateError::Corrupt { .. })),
        "{refused:?}"
    );
    let said = refused.to_string();
    assert!(
        said.starts_with("--state-store: ") && said.contains("op/0000000000000001"),
        "{said}"
    );
    assert!(
        std::error::Error::source(&refused).is_some(),
        "the cause is kept"
    );
}

/// How a caller's ticket ended, once it is done.
fn ended(ticket: &kbf_front::Ticket) -> Finished {
    let Stage::Done(finished) = ticket.stage.borrow().clone() else {
        panic!("the caller was not answered");
    };
    finished
}

/// Catches: a result that lives only in the process that finished the operation (a
/// process that kept a caller's stream open across a handover has nothing to answer it
/// with): once a worker's result is accepted, the store holds the result the caller was
/// sent, as a `done/` row, and nothing else of the operation.
#[tokio::test(start_paused = true)]
async fn a_finished_operation_leaves_its_result_in_the_store() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let ticket = farm
        .submit(Submission {
            request: request(1),
            call: None,
            described: Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("submitted");
    let leases = starts(&mut inbox);
    assert!(
        farm.ended(kbf_types::OperationId(0))
            .expect("read")
            .is_none()
    );
    assert!(accepted(&farm, leases[0]).await);
    let sent = ended(&ticket);
    assert!(matches!(&sent, Finished::Failed(s) if s.code == Code::InvalidArgument as i32));
    assert_eq!(disk.rows("done/"), 1);
    assert_eq!(disk.rows("op/"), 0, "the operation itself is finished");
    assert_eq!(
        farm.ended(kbf_types::OperationId(0)).expect("read"),
        Some(sent)
    );
}

/// Catches: a refusal that leaves no row (a stream kept open by an old process would wait
/// on an operation that was refused before the handover): the row holds the refusal the
/// caller was sent.
#[tokio::test(start_paused = true)]
async fn a_refused_operation_leaves_its_refusal_in_the_store() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let ticket = farm
        .submit(Submission {
            request: request(1),
            call: None,
            described: Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("submitted");
    assert_eq!(starts(&mut inbox).len(), 1);
    for _ in 0..2 {
        let (_, mut inbox) = lose_lease(&farm).await;
        assert_eq!(starts(&mut inbox).len(), 1);
    }
    let (_, _inbox) = lose_lease(&farm).await;
    let sent = ended(&ticket);
    assert!(matches!(&sent, Finished::Failed(s) if s.code == Code::FailedPrecondition as i32));
    assert_eq!(disk.rows("done/"), 1);
    assert_eq!(
        farm.ended(kbf_types::OperationId(0)).expect("read"),
        Some(sent)
    );
}

/// A caller's ticket and the lease its operation holds, on a farm with one node.
async fn running(disk: &Disk) -> (TestFarm, kbf_front::Ticket, LeaseId) {
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let ticket = farm
        .submit(Submission {
            request: request(1),
            call: None,
            described: Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("submitted");
    let leases = starts(&mut inbox);
    assert_eq!(leases.len(), 1);
    (farm, ticket, leases[0])
}

/// The rows a cold start over `disk` finds of operation 0: `(unfinished, ended)`, where
/// `unfinished` counts its `op/` rows and `ended` is whether a farm started over the store
/// has its result. Read before the successor starts, which may write.
fn standing(disk: &Disk) -> (usize, bool) {
    let unfinished = disk.rows("op/");
    let successor = disk.farm(EpochPolicy::Keep);
    let ended = successor.ended(OperationId(0)).expect("read").is_some();
    assert_eq!(ended, disk.rows("done/") == 1, "done/ rows and ended agree");
    (unfinished, ended)
}

/// Catches: a finish written as two transactions, the operation's rows removed first and
/// its `done/` row after. A process killed between the two leaves the result in no row:
/// callers and followers never get it, and a cold start neither reruns the action (its
/// `op/` row is gone) nor finds the result. The store takes `writes` writes of the
/// finishing report and then the process dies: whatever the cut, the store holds exactly
/// one of the operation's unfinished rows and its result.
async fn a_crash_at_any_write_of_a_finish_leaves_the_operation_or_its_result(backing: Backing) {
    for writes in 0..4 {
        let mut disk = Disk::over(backing);
        let (farm, _ticket, lease) = running(&disk).await;
        disk.store.fail_after(writes);
        let _ = accepted(&farm, lease).await;
        disk.crash(farm);
        let (unfinished, ended) = standing(&disk);
        assert_eq!(
            unfinished + usize::from(ended),
            1,
            "after {writes} writes: {unfinished} op rows, result {ended}: the result is lost \
             or the operation both runs again and has an answer"
        );
    }
}

/// Catches: the same two-transaction gap for a refusal (the operation's rows removed, then
/// the `done/` row): whatever write the process dies at, the store holds the operation or
/// its refusal.
async fn a_crash_at_any_write_of_a_refusal_leaves_the_operation_or_its_refusal(backing: Backing) {
    for writes in 0..4 {
        let mut disk = Disk::over(backing);
        let (farm, _ticket, _lease) = running(&disk).await;
        for _ in 0..2 {
            let (_, mut inbox) = lose_lease(&farm).await;
            assert_eq!(starts(&mut inbox).len(), 1);
        }
        // The new session registers while the store takes writes (the frontend becomes the
        // node's owner); the store then takes `writes` writes of the heartbeat that loses
        // the lease for the third time, and the heartbeat may go unacknowledged.
        let (stream, _inbox) = register(&farm);
        disk.store.fail_after(writes);
        let _ = farm.heartbeat(&node(), stream, 1, Vec::new()).await;
        disk.crash(farm);
        let (unfinished, ended) = standing(&disk);
        assert_eq!(
            unfinished + usize::from(ended),
            1,
            "after {writes} writes: {unfinished} op rows, refusal {ended}"
        );
    }
}

/// Catches: a result written in a transaction of its own after the operation's rows are
/// removed (the store takes the first and refuses the second: the caller is answered from
/// memory but nothing in the store holds the result). The store takes one write of the
/// finish and then goes down: that write is the whole finish, so the operation's rows are
/// gone, the result is there, and a farm started over the store reads it, with no tick
/// or later write in between.
#[tokio::test(start_paused = true)]
async fn a_finish_the_store_takes_leaves_the_result_with_no_later_write() {
    let mut disk = Disk::new();
    let (farm, ticket, lease) = running(&disk).await;
    disk.store.fail_after(1);
    assert!(accepted(&farm, lease).await);
    let sent = ended(&ticket);
    assert_eq!(
        (disk.rows("lease/"), disk.rows("op/"), disk.rows("done/")),
        (0, 0, 1)
    );
    disk.crash(farm);
    let successor = disk.farm(EpochPolicy::Keep);
    assert_eq!(
        successor.ended(OperationId(0)).expect("read"),
        Some(sent),
        "a successor over the store does not find the result"
    );
}

/// Catches: a committed result whose rows the store refused, dropped. The scheduler has
/// finished the operation in memory, so nothing ever asks the store again: the lease and
/// operation rows stay as if the lease still ran, the caller is never answered, and a
/// successor would run the action a second time. Held back while the store is down, the
/// answer (with the report's own detail, not a stand-in) and the rows follow when it
/// takes writes.
#[tokio::test(start_paused = true)]
async fn a_committed_result_the_store_refused_is_written_and_answered_when_it_returns() {
    let disk = Disk::new();
    let (farm, ticket, lease) = running(&disk).await;
    assert_eq!((disk.rows("lease/"), disk.rows("op/")), (1, 1));
    disk.store.set_down(true);
    assert!(
        !accepted(&farm, lease).await,
        "an answer the store has not taken was acknowledged"
    );
    for _ in 0..3 {
        farm.tick().await;
    }
    assert!(
        !matches!(*ticket.stage.borrow(), Stage::Done(_)),
        "the caller was answered before the store took the result"
    );
    assert_eq!((disk.rows("lease/"), disk.rows("op/")), (1, 1));

    disk.store.set_down(false);
    farm.tick().await;
    let Finished::Failed(status) = ended(&ticket) else {
        panic!("the caller was answered with something else");
    };
    assert_eq!(status.code, Code::InvalidArgument as i32);
    assert_eq!(status.message, "test", "the report's detail was lost");
    assert_eq!(
        (disk.rows("lease/"), disk.rows("op/"), disk.rows("queue/")),
        (0, 0, 0),
        "the result's rows were never written again"
    );
    assert_eq!(disk.rows("done/"), 1);
    assert_eq!(
        farm.ended(OperationId(0)).expect("read"),
        Some(Finished::Failed(status)),
        "the row is not what the caller was sent"
    );
}

/// Catches: a request for an action whose finish the store has not taken yet that joins
/// the finished operation. The scheduler finished it in memory, but its `op/` row and
/// `inflight/` pin stay until the finish lands, so the submit finds a pin whose
/// operation exists and takes it up again: the finished operation is queued afresh over
/// itself, granted a second time (two runs), and the held answer and the new grant
/// disagree. The request is refused UNAVAILABLE while the finish is held, no second
/// `Start` goes out, and a request after the finish landed queues a fresh operation.
#[tokio::test(start_paused = true)]
async fn a_request_while_the_finish_is_held_does_not_retake_the_finished_operation() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let ticket = farm
        .submit(Submission {
            request: request(1),
            call: None,
            described: Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("submitted");
    let leases = starts(&mut inbox);
    assert_eq!(leases.len(), 1);
    disk.store.set_down(true);
    assert!(!accepted(&farm, leases[0]).await, "the store was down");
    assert_eq!((disk.rows("op/"), disk.rows("inflight/")), (1, 1));

    let again = submit(&farm, request(1));
    assert_eq!(
        again.err().map(|status| status.code()),
        Some(Code::Unavailable),
        "the request joined an operation the scheduler finished"
    );

    disk.store.set_down(false);
    for _ in 0..3 {
        farm.tick().await;
    }
    let Finished::Failed(status) = ended(&ticket) else {
        panic!("the caller was answered with something else");
    };
    assert_eq!(status.message, "test");
    assert!(
        starts(&mut inbox).is_empty(),
        "the action ran a second time"
    );
    assert_eq!(
        (disk.rows("op/"), disk.rows("inflight/"), disk.rows("done/")),
        (0, 0, 1)
    );

    submit(&farm, request(1)).expect("a request after the finish landed");
    assert_eq!(starts(&mut inbox).len(), 1, "a fresh run is granted");
    assert_eq!((disk.rows("op/"), disk.rows("inflight/")), (1, 1));
}

/// Catches: a held finish that outlives the scheduler's retention of finished operations.
/// The scheduler retires a finished operation after `FINISHED_RETENTION` whether or not its
/// finish was written; a request after that finds the store's pin, no operation in the
/// scheduler, and takes the rows up again: a second run, and the held finish then deletes
/// the new operation's rows. The request is refused UNAVAILABLE for as long as the finish
/// is held, no second `Start` goes out, and the rows are right once the store is back.
#[tokio::test(start_paused = true)]
async fn a_request_after_the_retention_of_a_held_finish_does_not_retake_the_operation() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let ticket = farm
        .submit(Submission {
            request: request(1),
            call: None,
            described: Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("submitted");
    let leases = starts(&mut inbox);
    assert_eq!(leases.len(), 1);
    disk.store.set_down(true);
    assert!(!accepted(&farm, leases[0]).await, "the store was down");

    // The scheduler retires its finished operation while the finish is still held: a
    // feed after the retention moves its clock past it, the store still refusing.
    tokio::time::advance(kbf_sched::FINISHED_RETENTION + Duration::from_secs(1)).await;
    farm.tick().await;
    assert_eq!(disk.rows("op/"), 1, "the finish was written");
    disk.store.set_down(false);
    let held = submit(&farm, request(1));
    assert_eq!(
        held.err().map(|status| status.code()),
        Some(Code::Unavailable),
        "the request re-took an operation whose finish is held"
    );
    assert!(
        starts(&mut inbox).is_empty(),
        "the action ran a second time"
    );

    for _ in 0..3 {
        farm.tick().await;
    }
    let Finished::Failed(status) = ended(&ticket) else {
        panic!("the caller was answered with something else");
    };
    assert_eq!(status.message, "test");
    assert_eq!(
        (disk.rows("op/"), disk.rows("inflight/"), disk.rows("done/")),
        (0, 0, 1)
    );
}

/// Catches: a retry that goes on after a newer writer holds the store (the old scheduler
/// would write its stale rows over its successor's the moment a write landed, or keep the
/// store busy for ever): the first refusal that says `Fenced` ends it. No write is tried
/// after that, whatever feeds the farm, and none lands.
#[tokio::test(start_paused = true)]
async fn a_fenced_store_ends_the_retry_of_a_refused_result() {
    let disk = Disk::new();
    let (farm, ticket, lease) = running(&disk).await;
    disk.store.set_down(true);
    assert!(!accepted(&farm, lease).await);
    farm.tick().await;
    disk.store.set_down(false);
    disk.store.set_fenced();
    // The farm learns it was superseded from the write it retries.
    farm.tick().await;
    let rows = disk.all_rows();
    let tried = disk.store.attempts.load(Ordering::SeqCst);
    for _ in 0..3 {
        farm.tick().await;
        let _ = farm.report(&node(), invalid(lease)).await;
    }
    assert_eq!(
        disk.store.attempts.load(Ordering::SeqCst),
        tried,
        "a write was tried after the store fenced the farm"
    );
    assert_eq!(disk.all_rows(), rows, "a write landed after the fence");
    assert!(!matches!(*ticket.stage.borrow(), Stage::Done(_)));
}

/// Two operations running on one node, and the leases they hold.
async fn running_two(disk: &Disk) -> (TestFarm, [kbf_front::Ticket; 2], [LeaseId; 2]) {
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let tickets = [1, 2].map(|tag| {
        farm.submit(Submission {
            request: request(tag),
            call: None,
            described: Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("submitted")
    });
    let leases = starts(&mut inbox);
    assert_eq!(leases.len(), 2);
    (farm, tickets, [leases[0], leases[1]])
}

fn applied_at(disk: &Disk, lease: LeaseId) -> usize {
    let key = format!(
        "Delete {{ key: \"lease/{:016x}.{:016x}\"",
        lease.term, lease.seq
    );
    let log = disk.store.applied.lock().expect("log");
    log.iter()
        .position(|w| w.contains(&key))
        .expect("the lease row was deleted")
}

/// Catches: a write the store applied but reported failed, whose retry finds its rows
/// gone (`Missing`) and drops the record's Answer: the caller is never answered. The
/// retry must take that as its own work done and answer, with the report's detail.
#[tokio::test(start_paused = true)]
async fn an_answer_the_store_applied_but_did_not_acknowledge_is_still_delivered() {
    let disk = Disk::new();
    let (farm, ticket, lease) = running(&disk).await;
    disk.store.lie.store(true, Ordering::SeqCst);
    assert!(!accepted(&farm, lease).await);
    assert_eq!(
        (disk.rows("lease/"), disk.rows("op/")),
        (0, 0),
        "it applied"
    );
    disk.store.lie.store(false, Ordering::SeqCst);
    for _ in 0..3 {
        farm.tick().await;
    }
    let sent = ended(&ticket);
    assert!(matches!(&sent, Finished::Failed(s) if s.message == "test"));
    assert_eq!(disk.rows("done/"), 1);
    assert_eq!(
        farm.ended(OperationId(0)).expect("read"),
        Some(sent),
        "the row is not what the caller was sent"
    );
    assert_eq!(farm.dropped_writes(), 0);
}

/// Catches: the same loss for a grant: the store took it, said it did not, and the retry
/// found the queue row gone and dropped the `Start`, so the daemon never runs the lease.
#[tokio::test(start_paused = true)]
async fn a_grant_the_store_applied_but_did_not_acknowledge_is_still_started() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (stream, mut inbox) = register(&farm);
    disk.store.lie_lease.store(true, Ordering::SeqCst);
    submit(&farm, request(1)).expect("queued");
    assert!(starts(&mut inbox).is_empty());
    assert_eq!(
        (disk.rows("lease/"), disk.rows("queue/")),
        (1, 0),
        "it applied"
    );
    disk.store.lie_lease.store(false, Ordering::SeqCst);
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    farm.tick().await;
    assert_eq!(starts(&mut inbox).len(), 1, "the Start was dropped");
    assert_eq!(farm.dropped_writes(), 0);
}

/// Catches: held records written out of order (a refused front batch rotated behind the
/// next): the leases are finished in the order the results were taken.
#[tokio::test(start_paused = true)]
async fn held_records_are_written_in_the_order_they_committed() {
    let disk = Disk::new();
    let (farm, tickets, leases) = running_two(&disk).await;
    disk.store.set_down(true);
    assert!(!accepted(&farm, leases[0]).await);
    assert!(!accepted(&farm, leases[1]).await);
    disk.store.set_down(false);
    farm.tick().await;
    for ticket in &tickets {
        let _ = ended(ticket);
    }
    assert!(applied_at(&disk, leases[0]) < applied_at(&disk, leases[1]));
}

/// Catches: held records written after a fence: with two held, the first refusal says
/// `Fenced` and the second is never tried.
#[tokio::test(start_paused = true)]
async fn a_fence_ends_the_pass_over_the_held_records() {
    let disk = Disk::new();
    let (farm, _tickets, leases) = running_two(&disk).await;
    disk.store.set_down(true);
    assert!(!accepted(&farm, leases[0]).await);
    assert!(!accepted(&farm, leases[1]).await);
    disk.store.set_down(false);
    disk.store.set_fenced();
    let before = disk.store.attempts.load(Ordering::SeqCst);
    farm.tick().await;
    assert_eq!(
        disk.store.attempts.load(Ordering::SeqCst) - before,
        2,
        "the outage ping, then the first record only"
    );
    assert_eq!((disk.rows("lease/"), disk.rows("op/")), (2, 2));
    assert_eq!(disk.rows("done/"), 0, "a result landed after the fence");
}

/// Catches: an unbounded backlog: past the cap a new record is dropped, counted.
#[tokio::test(start_paused = true)]
async fn records_held_past_the_cap_are_dropped_and_counted() {
    let disk = Disk::new();
    let (farm, _tickets, leases) = running_two(&disk).await;
    let farm = farm.with_held_cap(1);
    disk.store.set_down(true);
    assert!(!accepted(&farm, leases[0]).await);
    assert_eq!(farm.dropped_writes(), 0);
    assert!(!accepted(&farm, leases[1]).await);
    assert_eq!(farm.dropped_writes(), 1);
}

/// Catches: an answer held back by a refusing store that is released by another
/// operation's report and settled with that report's detail, or not at all (the first
/// caller stays unanswered, or gets the stand-in INTERNAL): both callers get their own
/// report's detail.
#[tokio::test(start_paused = true)]
async fn an_answer_released_by_another_report_keeps_its_own_detail() {
    let disk = Disk::new();
    let (farm, tickets, leases) = running_two(&disk).await;
    disk.store.set_down(true);
    assert!(!accepted(&farm, leases[0]).await);
    disk.store.set_down(false);
    assert!(accepted(&farm, leases[1]).await);
    for ticket in &tickets {
        let Finished::Failed(status) = ended(ticket) else {
            panic!("a caller was not answered");
        };
        assert_eq!(status.message, "test");
    }
    assert_eq!(farm.dropped_writes(), 0);
}

/// Catches: the detail of a report dropped between the write of its answer and the
/// settling of it. The answer is taken by the store, and a later record of the same feed
/// (the grant the freed capacity lets through) is refused as stale: that must not forget
/// the details of answers already written and waiting to be settled. Their callers would
/// get the stand-in INTERNAL and no cache write, while the `done/` row holds the real
/// result. The caller is answered with the report's own detail, as the store holds it.
#[tokio::test(start_paused = true)]
async fn a_stale_record_in_the_feed_does_not_drop_a_written_answers_detail() {
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    let tickets: Vec<_> = (1..=5)
        .map(|tag| {
            farm.submit(Submission {
                request: request(tag),
                call: None,
                described: Described::default(),
                evidence: Default::default(),
                max_wall: None,
            })
            .expect("submitted")
        })
        .collect();
    let leases = starts(&mut inbox);
    assert_eq!(leases.len(), 4, "the fifth waits for capacity");
    // The answer is held while the store is down; the freed room goes to the fifth, whose
    // queue row is gone from under it by the time the store takes writes: its grant, in
    // the feed that writes the answer, is refused as stale.
    disk.store.set_down(true);
    assert!(!accepted(&farm, leases[0]).await);
    disk.store.set_down(false);
    let gone = Txn::new().delete(format!("queue/{:016x}", 4), kbf_state::Cond::Any);
    disk.store.txn(gone).expect("store").expect("delete");
    farm.tick().await;
    let Finished::Failed(status) = ended(&tickets[0]) else {
        panic!("the caller was answered with something else");
    };
    assert_eq!(status.code, Code::InvalidArgument as i32);
    assert_eq!(status.message, "test", "the report's detail was dropped");
    assert!(farm.dropped_writes() >= 1, "the grant was not refused");
    let row = farm.ended(OperationId(0)).expect("read").expect("a row");
    assert_eq!(row, Finished::Failed(status), "the row is what was sent");
}

/// Asserts that a finish is not written when the store refuses the farm's writes as a
/// superseded handle's: the farm takes the worker's result, and its transaction (the
/// operation's rows removed, the result put) is refused as a whole. The store keeps the
/// operation's lease and op rows and no result.
#[tokio::test(start_paused = true)]
async fn a_fenced_farm_cannot_finish_an_operation() {
    let disk = Disk::new();
    let old = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&old);
    submit(&old, request(1)).expect("submitted");
    let [lease] = starts(&mut inbox)[..] else {
        panic!("one lease expected");
    };
    disk.store.set_fenced();
    assert!(
        !accepted(&old, lease).await,
        "a fenced farm took the result"
    );
    assert_eq!(disk.rows("done/"), 0, "a result landed past the fence");
    assert_eq!((disk.rows("lease/"), disk.rows("op/")), (1, 1));
}

/// Rows written so far, as the store's applied log prints them.
fn writes(disk: &Disk) -> usize {
    disk.store.applied.lock().expect("log").len()
}

/// Catches: the release of a committed record on an input that delivers no answers
/// (a submit), in each of its three states. (a) Nothing is held: the submit's own grant
/// is written and started by that very call, not at the next tick. (b) Batches are held
/// (two answers the store refused): the submit's grant is held behind them, nothing is
/// written out of order and no `Start` is sent. (c) The next input that answers (a tick)
/// writes the held answers first, in the order they committed, and the grant after them.
/// The order is driven by the store's refusals, never by timing.
#[tokio::test(start_paused = true)]
async fn a_submit_releases_its_grant_only_when_no_batch_is_held() {
    // (a) nothing held.
    let disk = Disk::new();
    let farm = disk.farm(EpochPolicy::Keep);
    let (_, mut inbox) = register(&farm);
    submit(&farm, request(1)).expect("queued");
    assert_eq!(
        starts(&mut inbox).len(),
        1,
        "a submit with nothing held did not start its grant at once"
    );

    // (b) two answers held, then a submit.
    let disk = Disk::new();
    let (farm, tickets, leases) = running_two(&disk).await;
    disk.store.set_down(true);
    assert!(!accepted(&farm, leases[0]).await);
    assert!(!accepted(&farm, leases[1]).await);
    disk.store.set_down(false);
    let (stream, mut inbox) = register(&farm);
    let before = writes(&disk);
    submit(&farm, request(3)).expect("queued");
    // The submit writes its own queue row; nothing about a lease, held or new.
    {
        let log = disk.store.applied.lock().expect("log");
        assert!(
            log[before..].iter().all(|w| !w.contains("lease/")),
            "a submit wrote a lease row past the held answers: {:?}",
            &log[before..]
        );
    }
    assert!(
        starts(&mut inbox).is_empty(),
        "a grant overtook the held answers"
    );
    for ticket in &tickets {
        assert!(!matches!(*ticket.stage.borrow(), Stage::Done(_)));
    }

    // (c) an answering input releases them in order, the grant last.
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    farm.tick().await;
    for ticket in &tickets {
        let _ = ended(ticket);
    }
    assert!(applied_at(&disk, leases[0]) < applied_at(&disk, leases[1]));
    let started = starts(&mut inbox);
    assert_eq!(started.len(), 1, "the held grant was not started once");
    let grant = format!("lease/{:016x}.{:016x}", started[0].term, started[0].seq);
    let log = disk.store.applied.lock().expect("log");
    let put = log
        .iter()
        .position(|w| w.contains(&grant))
        .expect("the grant was written");
    drop(log);
    assert!(
        put > applied_at(&disk, leases[1]),
        "the grant was written before a held answer"
    );
}

/// Catches: a grant carried out ahead of an earlier held record (or released by an input
/// that delivers no answers), and a held `Start` written after the scheduler gave its
/// lease up (it would put the lease back in the store over the requeue): the grant waits
/// behind the held answer; once its lease is given up the old `Start` is never written
/// and only the regrant starts.
#[tokio::test(start_paused = true)]
async fn a_grant_waits_behind_a_held_answer_and_a_given_up_lease_is_not_written() {
    let disk = Disk::new();
    let (farm, ticket, lease) = running(&disk).await;
    disk.store.set_down(true);
    assert!(!accepted(&farm, lease).await);
    disk.store.set_down(false);
    let (stream, mut inbox) = register(&farm);
    submit(&farm, request(2)).expect("queued");
    assert!(
        starts(&mut inbox).is_empty(),
        "a grant overtook the held answer"
    );
    assert!(!matches!(*ticket.stage.borrow(), Stage::Done(_)));
    tokio::time::advance(kbf_sched::fence::START_GRACE + Duration::from_secs(1)).await;
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    let _ = ended(&ticket);
    farm.tick().await;
    let started = starts(&mut inbox);
    // Sequence 0 is the first operation's lease and 1 the given-up grant.
    assert_eq!(
        started,
        [LeaseId::new(farm.lease_epoch(), 2)],
        "the given-up lease's Start was written and sent"
    );
    assert_eq!(disk.rows("lease/"), 1);
    let key = format!("lease/{:016x}.{:016x}", started[0].term, started[0].seq);
    assert!(disk.store.get(&key).expect("read").is_some(), "{key}");
}

/// Asserts that a stale write is dropped and not delivered. The old bug read a "row is gone"
/// as already applied for an effect that was never tried: the held `Start` that was
/// refused is purged when its lease is given up, and the answer behind it, whose rows
/// really are gone, was never written; it must be dropped, not delivered.
#[tokio::test(start_paused = true)]
async fn a_stale_refusal_of_a_never_tried_effect_drops_it() {
    let disk = Disk::new();
    let (farm, ticket, lease) = running(&disk).await;
    let (stream, mut inbox) = register(&farm);
    // The answer's rows are gone from under it: its write is stale, not applied.
    let gone = Txn::new().delete(
        format!("lease/{:016x}.{:016x}", lease.term, lease.seq),
        kbf_state::Cond::Any,
    );
    disk.store.txn(gone).expect("store").expect("delete");
    // A grant the store refuses is held and tried again (its queue row is still there).
    disk.store.fail_after(1);
    submit(&farm, request(2)).expect("queued");
    // The answer waits behind it, never tried.
    assert!(!accepted(&farm, lease).await);
    disk.store.set_down(false);
    tokio::time::advance(kbf_sched::fence::START_GRACE + Duration::from_secs(1)).await;
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    assert!(
        !matches!(*ticket.stage.borrow(), Stage::Done(_)),
        "a stale answer that was never written was delivered"
    );
    assert!(farm.dropped_writes() >= 1);
    drop(starts(&mut inbox));
}

/// Catches: a report that lands in the feed that expires its lease, with a healthy
/// store. The scheduler lets the worker's result decide the operation, but the farm's own
/// requeue had already deleted the lease row, so the answer's write read "row is gone" as
/// stale: the caller was never answered and the op and queue rows stayed behind, for a
/// restart to run the finished operation again. The caller is answered and no row is left.
#[tokio::test(start_paused = true)]
async fn a_report_in_the_feed_that_expires_its_lease_is_answered_and_leaves_no_rows() {
    let disk = Disk::new();
    let (farm, ticket, lease) = running(&disk).await;
    tokio::time::advance(kbf_sched::fence::LEASE_GRACE + Duration::from_secs(1)).await;
    assert!(accepted(&farm, lease).await);
    let Finished::Failed(status) = ended(&ticket) else {
        panic!("the caller was not answered");
    };
    assert_eq!(status.message, "test");
    assert_eq!(farm.dropped_writes(), 0);
    assert_eq!(
        (disk.rows("lease/"), disk.rows("op/"), disk.rows("queue/")),
        (0, 0, 0),
        "rows of a finished operation were left"
    );
}

/// Catches: the same report with the store down, so its answer is held and the requeue
/// is written first when the store returns: the retry read "row is gone" as its work
/// done and delivered the answer without writing it, leaving the op and queue rows (the
/// operation runs again after a restart). The answer is written for real: the caller is
/// answered once, from the original report, no second `Start` goes out, no row is left.
#[tokio::test(start_paused = true)]
async fn a_held_answer_for_a_lease_the_farm_gave_up_is_written_and_delivered_once() {
    let disk = Disk::new();
    let (farm, ticket, lease) = running(&disk).await;
    let (stream, mut inbox) = register(&farm);
    tokio::time::advance(kbf_sched::fence::LEASE_GRACE + Duration::from_secs(1)).await;
    disk.store.set_down(true);
    assert!(!accepted(&farm, lease).await);
    assert!(
        !matches!(*ticket.stage.borrow(), Stage::Done(_)),
        "answered before the store took the result"
    );
    disk.store.set_down(false);
    for _ in 0..3 {
        farm.tick().await;
    }
    assert!(farm.heartbeat(&node(), stream, 1, Vec::new()).await);
    farm.tick().await;
    let sent = ended(&ticket);
    assert!(
        matches!(&sent, Finished::Failed(s) if s.message == "test"),
        "not the original report"
    );
    assert!(starts(&mut inbox).is_empty(), "the operation was run again");
    assert_eq!(farm.dropped_writes(), 0);
    assert_eq!(
        (disk.rows("lease/"), disk.rows("op/"), disk.rows("queue/")),
        (0, 0, 0),
        "rows of a finished operation were left"
    );
    assert_eq!(
        farm.ended(OperationId(0)).expect("read"),
        Some(sent),
        "the row is not what the caller was sent"
    );
}

/// Catches: a failed read of the queue row, made while judging an answer whose lease row
/// the farm's own give-up deleted, taken for "the row is gone": the answer was dropped as
/// stale and the caller never answered. A read that fails is the store being unreachable:
/// the answer is held and written when the read works, the caller answered once.
#[tokio::test(start_paused = true)]
async fn a_failed_read_while_judging_an_answer_holds_it_and_does_not_drop_it() {
    let disk = Disk::new();
    let (farm, ticket, lease) = running(&disk).await;
    tokio::time::advance(kbf_sched::fence::LEASE_GRACE + Duration::from_secs(1)).await;
    disk.store.fail_queue_get.store(1, Ordering::SeqCst);
    assert!(!accepted(&farm, lease).await);
    for _ in 0..3 {
        farm.tick().await;
    }
    let Finished::Failed(status) = ended(&ticket) else {
        panic!("the caller was not answered");
    };
    assert_eq!(status.message, "test");
    assert_eq!(farm.dropped_writes(), 0, "the answer was dropped as stale");
    assert_eq!(
        (disk.rows("lease/"), disk.rows("op/"), disk.rows("queue/")),
        (0, 0, 0)
    );
}

/// Catches: a farm with no state store pretending to answer from one: it has no result
/// for any operation.
#[test]
fn a_farm_without_a_store_has_no_stored_results() {
    let farm = Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    );
    assert!(
        farm.ended(kbf_types::OperationId(0))
            .expect("read")
            .is_none()
    );
}

/// Asserts that a name the first process gave names, in a process started over the same
/// store that kept the epoch (a daemon keeps its runs), the operation it named and no other:
/// the names are the store's operation ids, which no process takes twice, so a client
/// holding the name is never handed whichever operation of the second process has the same
/// number (issue #154). A new operation of the second process has a name of its own, and a
/// name nobody gave names nothing.
#[tokio::test(start_paused = true)]
async fn a_kept_epoch_does_not_reuse_operation_names() {
    let dir = Scratch::new();
    let first = farm_over(store_from_flag(&dir.0).expect("a first start"));
    let epoch = first.lease_epoch();
    let name = first
        .submit(Submission {
            request: request(1),
            call: None,
            described: Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("submitted")
        .name;
    assert!(first.wait(&name).is_some());
    drop(first);

    let kept = StateStore {
        epoch: EpochPolicy::Keep,
        ..store_from_flag(&dir.0).expect("a second start")
    };
    let second = farm_over(kept);
    assert_eq!(second.lease_epoch(), epoch, "the epoch was kept");
    let same = second.wait(&name).expect("the store holds the operation");
    assert_eq!(same.action, action_of(1), "another operation");
    let other = second
        .submit(Submission {
            request: request(2),
            call: None,
            described: Described::default(),
            evidence: Default::default(),
            max_wall: None,
        })
        .expect("submitted")
        .name;
    assert_ne!(other, name, "a name was given twice");
    let nobody = second.wait("operations/9999999999");
    assert!(nobody.is_none(), "a name nobody gave names an operation");
}
