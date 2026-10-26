//! How fast the state store takes the farm's writes on a real Postgres server when one, two
//! and three frontends write to it at once: the writes per second they commit and the
//! latency of a commit, measured. It is a measurement, not a gate: it asserts nothing about
//! time, and the numbers are read by a person.
//!
//! To run it, point `KBF_TEST_POSTGRES_URL` at a test server (the one the other Postgres tests
//! use) and ask for the ignored test, with its output shown:
//!
//! ```text
//! KBF_TEST_POSTGRES_URL=<url of the test server> \
//!     cargo test -p kbf-state --test postgres_write_rate -- --ignored --nocapture
//! ```
//!
//! Add `--release` for the figures of an optimized build. Each handle count gets a schema of
//! its own, dropped at the end, so runs do not see each other's rows.
//!
//! A frontend is one [`PostgresStore`] handle (its own pool of connections, 8 by default),
//! driven by `WRITERS_PER_FRONTEND` writer threads. The writers run the transactions a farm
//! commits for an operation, through the same store calls the frontends use and with the
//! keys and preconditions of the farm's own writes (`Durable` in `kbf-server`):
//!
//! - the **submission** puts the `op/`, `subject/` and `queue/` rows and the `inflight/` pin,
//!   each where there is none;
//! - the **grant** deletes the `queue/` row at the version read, puts the `lease/` row where
//!   there is none and the `lease-by-op/` row, and checks the `owner/` row of the node, so
//!   a frontend that reads a row another frontend queued claims it for its own daemon, and
//!   two frontends that claim one row cannot both win;
//! - the **finish** deletes the `lease/` row and the pin at the versions read, checks the
//!   `owner/` row, puts the `done/` and `doneof/` rows, and deletes the operation's other
//!   rows.
//!
//! Every writer submits its share of the operations and, after each submission, claims
//! whichever queued row it finds (any frontend's) and finishes it. So the frontends race for
//! the queue as they do when they share one, and a claim that loses a race is counted and
//! writes nothing.
//!
//! A **write** here is one committed transaction (a submission, a grant or a finish), and its
//! latency is the time of the `Store::txn` call, which includes the wait for the commit head.
//! The reads that come before a write (the farm reads the rows it writes conditionally on)
//! are made but not timed, so the latencies are those of the commits alone. The writes per
//! second are the writes divided by the whole wall-clock time of the run, which includes
//! those reads and the scans for queued rows, so they are the rate of the mix and not of
//! the commits alone. The store's target is [`TARGET_WRITES_PER_SECOND`] in bursts, of
//! about five durable writes for each action; an operation here is three of them.
//!
//! What the run leaves out: the daemons' session renewals (one write per daemon every 20
//! seconds), the reservation of a block of ids (one write for 1024 operations), the cordon,
//! floor and size rows, and the contents of the values. The store keeps a value as bytes, so
//! the rows of the farm's own encodings (`op/`, `subject/`, `lease/`, `done/`) are filler of
//! a stand-in length (the constants below) and the rows with a plain layout are written as
//! the farm writes them.
#![cfg(feature = "postgres")]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use kbf_state::postgres::{Flaw, PostgresBackend, PostgresConfig, PostgresStore};
use kbf_state::{
    Commit, Cond, Conflict, Event, Revision, Store, TARGET_WRITES_PER_SECOND, Txn, Versioned,
};

/// The frontend counts the measurement runs with.
const FRONTENDS: [usize; 3] = [1, 2, 3];

/// The writer threads of one frontend.
const WRITERS_PER_FRONTEND: usize = 4;

/// The operations one measured run drives, whatever the number of frontends: a multiple of
/// the writer threads of every run, so each writer submits the same number of them.
const MEASURED_OPERATIONS: usize = 3000;

/// The lease term of the grants (the store's epoch in a farm).
const TERM: u64 = 1;

/// The length of the filler of the rows the farm encodes with `kbf-log`.
const REQUEST_BYTES: usize = 512;
const SUBJECT_BYTES: usize = 96;
const LEASE_BYTES: usize = 64;
const RESULT_BYTES: usize = 256;

/// The size of the action every operation runs, and the instance it runs under (the pin of
/// an operation holds the instance's name in hexadecimal).
const ACTION_SIZE: u64 = 1024;
const INSTANCE: &[u8] = b"main";
const PINNED_INSTANCE: &str = "6d61696e";

/// How long the `done/` and `doneof/` rows are kept (the farm's floor for them).
const DONE_TTL: Duration = Duration::from_secs(600);

/// The hang guard of the measurement alone: far more than a run takes. A run that reaches
/// it has hung (a lost write that nothing will finish), and the measurement panics saying
/// so. The gated test has no such limit: its writers stop when every operation is finished
/// or a writer has failed.
const HANG_GUARD: Duration = Duration::from_secs(120);

/// A page of the change feed that holds every row of a run.
const FEED_PAGE: usize = 100_000;

/// The rows no finished operation leaves behind.
const LEFT_BEHIND: [&str; 6] = [
    "queue/",
    "op/",
    "lease/",
    "lease-by-op/",
    "subject/",
    "inflight/",
];

/// A row of `queue/` as a scan read it.
type Queued = (String, Versioned);

fn url() -> String {
    kbf_testkit::database::server_url()
}

fn named(prefix: &str, op: u64) -> String {
    format!("{prefix}{op:016x}")
}

fn lease_key(seq: u64) -> String {
    format!("lease/{TERM:016x}.{seq:016x}")
}

fn pin_key(op: u64) -> String {
    format!("inflight/{PINNED_INSTANCE}/{op:064x}/{ACTION_SIZE}")
}

fn owner_key(node: &str) -> String {
    format!("owner/{node}")
}

fn worker(frontend: usize) -> String {
    format!("node-{frontend}")
}

fn frontend_id(frontend: usize) -> u64 {
    u64::try_from(frontend).expect("a few frontends") + 1
}

/// The operation a `queue/` key names.
fn op_of(queue_key: &str) -> u64 {
    let id = &queue_key["queue/".len()..];
    u64::from_str_radix(id, 16).expect("a queue key ends in an id")
}

/// Two numbers, eight bytes each and big-endian: the layout of the `owner/` and
/// `lease-by-op/` rows.
fn pair(high: u64, low: u64) -> Vec<u8> {
    [high.to_be_bytes(), low.to_be_bytes()].concat()
}

/// The `doneof/` row: a version byte, the size and hash of the action, and the instance.
fn action_row() -> Vec<u8> {
    let mut row = vec![1];
    row.extend(ACTION_SIZE.to_be_bytes());
    row.extend([0xab; 32]);
    row.extend(INSTANCE);
    row
}

fn payload(len: usize) -> Vec<u8> {
    vec![0x5a; len]
}

fn row_of(store: &PostgresStore, key: &str) -> Versioned {
    let row = store.get(key).expect("the store reads");
    row.expect("the row is there")
}

/// Runs `txn`, and says how long the call took and how it ended.
fn timed(store: &PostgresStore, txn: Txn) -> (Duration, Result<Commit, Conflict>) {
    let start = Instant::now();
    let result = store.txn(txn).expect("the store works");
    (start.elapsed(), result)
}

/// Runs `txn`, which no other writer can make fail, and says how long the call took.
fn commit(store: &PostgresStore, txn: Txn) -> Duration {
    let (took, result) = timed(store, txn);
    result.expect("no precondition fails");
    took
}

/// Queues operation `op`, as a farm does when it takes a request.
fn submit(store: &PostgresStore, op: u64) -> Duration {
    let pin = pin_key(op);
    // The farm looks for a live operation of the action before it queues one.
    assert!(store.get(&pin).expect("the store reads").is_none());
    let request = payload(REQUEST_BYTES);
    let subject = payload(SUBJECT_BYTES);
    let txn = Txn::new()
        .put(named("op/", op), request, Cond::Absent)
        .put(named("subject/", op), subject, Cond::Absent)
        .put(named("queue/", op), Vec::new(), Cond::Absent)
        .put(pin, op.to_be_bytes(), Cond::Absent);
    commit(store, txn)
}

/// How a claim ended.
enum Claim {
    /// The grant committed, in this long.
    Won(Duration),
    /// Another frontend took the row first: nothing was written.
    Lost,
}

/// Grants the operation of the `queue/` row `queued` to the node `node`, with lease `seq`.
fn claim(store: &PostgresStore, node: &str, queued: &Queued, seq: u64) -> Claim {
    let (queue_key, queue_row) = queued;
    let op = op_of(queue_key);
    let owner = row_of(store, &owner_key(node));
    let lease = payload(LEASE_BYTES);
    let txn = Txn::new()
        .check(owner_key(node), Cond::Version(owner.version))
        .delete(queue_key.clone(), Cond::Version(queue_row.version))
        .put(lease_key(seq), lease, Cond::Absent)
        .put(named("lease-by-op/", op), pair(TERM, seq), Cond::Any);
    match timed(store, txn) {
        (took, Ok(_)) => Claim::Won(took),
        (_, Err(_)) => Claim::Lost,
    }
}

/// Finishes operation `op`, which `node` holds under lease `seq`.
fn finish(store: &PostgresStore, node: &str, op: u64, seq: u64) -> Duration {
    let lease_at = lease_key(seq);
    let lease = row_of(store, &lease_at);
    let owner = row_of(store, &owner_key(node));
    let pin_at = pin_key(op);
    let pin = row_of(store, &pin_at);
    // The farm reads the request to note which action the result is of.
    let _request = row_of(store, &named("op/", op));
    let noted = named("doneof/", op);
    let done = named("done/", op);
    let result = payload(RESULT_BYTES);
    let txn = Txn::new()
        .check(owner_key(node), Cond::Version(owner.version))
        .delete(lease_at, Cond::Version(lease.version))
        .delete(pin_at, Cond::Version(pin.version))
        .put_ttl(noted, action_row(), DONE_TTL, Cond::Any)
        .delete(named("op/", op), Cond::Any)
        .delete(named("queue/", op), Cond::Any)
        .delete(named("loss/", op), Cond::Any)
        .delete(named("fate/", op), Cond::Any)
        .delete(named("subject/", op), Cond::Any)
        .delete(named("opkey/", op), Cond::Any)
        .delete(named("lease-by-op/", op), Cond::Any)
        .put_ttl(done, result, DONE_TTL, Cond::Any);
    commit(store, txn)
}

/// One schema of the test server opened by several store handles, each standing for a
/// frontend whose daemon is the node `worker(n)`. The handles come first so that their
/// connections close before the backend drops the schema.
struct Rig {
    handles: Vec<PostgresStore>,
    _backend: PostgresBackend,
}

fn open(config: &PostgresConfig) -> PostgresStore {
    PostgresStore::open(config).expect("the test database opens")
}

impl Rig {
    fn new(frontends: usize) -> Rig {
        let backend = PostgresBackend::new(&url(), Flaw::None);
        let config = backend.fresh_config();
        let handles: Vec<PostgresStore> = (0..frontends).map(|_| open(&config)).collect();
        for (n, store) in handles.iter().enumerate() {
            // The daemon's `Hello` made the frontend the owner of its node.
            let key = owner_key(&worker(n));
            let owner = pair(frontend_id(n), 1);
            commit(store, Txn::new().put(key, owner, Cond::Absent));
        }
        Rig {
            handles,
            _backend: backend,
        }
    }
}

/// What the writers of one run share.
struct Job {
    /// The operations each writer submits.
    each: usize,
    /// The operations in all.
    total: usize,
    /// The next lease sequence: no two claims use one.
    seqs: AtomicU64,
    /// The operations whose result is put.
    finished: AtomicUsize,
    /// Set when a writer panics, so the others stop waiting for work it will not finish.
    failed: AtomicBool,
    /// The writer that won the claim of each operation.
    claims: Mutex<BTreeMap<u64, usize>>,
    /// When the writers give up as hung ([`HANG_GUARD`]); `None` in the gated test, whose
    /// writers stop only when every operation is finished or one has failed.
    hang_guard: Option<Instant>,
}

impl Job {
    fn new(each: usize, writers: usize, hang_guard: Option<Duration>) -> Job {
        Job {
            each,
            total: each * writers,
            seqs: AtomicU64::new(0),
            finished: AtomicUsize::new(0),
            failed: AtomicBool::new(false),
            claims: Mutex::new(BTreeMap::new()),
            hang_guard: hang_guard.map(|limit| Instant::now() + limit),
        }
    }

    /// Whether the writers still have work: an operation is unfinished, none failed and,
    /// in the measurement, the hang guard has not run out.
    fn open(&self) -> bool {
        let done = self.finished.load(Ordering::SeqCst) >= self.total;
        let failed = self.failed.load(Ordering::SeqCst);
        !(done | failed | self.hung())
    }

    /// Whether the hang guard has run out: never in the gated test.
    fn hung(&self) -> bool {
        self.hang_guard.is_some_and(|at| Instant::now() >= at)
    }

    /// Notes that `writer` won the claim of `op`.
    fn record(&self, op: u64, writer: usize) {
        let mut claims = self.claims.lock().unwrap_or_else(PoisonError::into_inner);
        let before = claims.insert(op, writer);
        assert!(before.is_none(), "operation {op} was claimed twice");
    }

    /// How many operations have a winning claim.
    fn claimed(&self) -> usize {
        let claims = self.claims.lock().unwrap_or_else(PoisonError::into_inner);
        claims.len()
    }
}

/// Raises [`Job::failed`] when the writer that holds it unwinds.
struct Alarm<'a>(&'a AtomicBool);

impl Drop for Alarm<'_> {
    fn drop(&mut self) {
        self.0.fetch_or(thread::panicking(), Ordering::SeqCst);
    }
}

/// What a writer saw.
#[derive(Default)]
struct Seen {
    /// The time of each transaction that committed.
    latencies: Vec<Duration>,
    /// The claims that lost to another frontend's.
    lost: usize,
}

/// One writer thread of the frontend whose handle is `store` and whose node is `node`:
/// submits its operations, one at a time, and after each claims a queued one (whoever
/// queued it) and finishes it, until every operation of the run is finished.
fn drive(job: &Job, store: &PostgresStore, node: &str, writer: usize) -> Seen {
    let _alarm = Alarm(&job.failed);
    let mut seen = Seen::default();
    let mut next = 0;
    while job.open() {
        if next < job.each {
            let index = writer * job.each + next;
            let op = u64::try_from(index).expect("a small run");
            seen.latencies.push(submit(store, op));
            next += 1;
        }
        let queued = store.scan_prefix("queue/").expect("the store scans");
        let Some(row) = queued.get(writer % queued.len().max(1)) else {
            thread::yield_now();
            continue;
        };
        let seq = job.seqs.fetch_add(1, Ordering::SeqCst);
        match claim(store, node, row, seq) {
            Claim::Won(took) => {
                let op = op_of(&row.0);
                job.record(op, writer);
                seen.latencies.push(took);
                seen.latencies.push(finish(store, node, op, seq));
                job.finished.fetch_add(1, Ordering::SeqCst);
            }
            Claim::Lost => seen.lost += 1,
        }
    }
    seen
}

/// What a run measured, and the store it ran on.
struct Report {
    frontends: usize,
    writers: usize,
    operations: usize,
    elapsed: Duration,
    seen: Seen,
    claimed: usize,
    retries: u64,
    rig: Rig,
}

/// Runs `operations` through `frontends` frontends of `writers` threads each. `operations`
/// is cut down to a multiple of the threads. With a `hang_guard`, a run still unfinished
/// when it runs out panics as hung.
fn run(
    frontends: usize,
    writers: usize,
    operations: usize,
    hang_guard: Option<Duration>,
) -> Report {
    let rig = Rig::new(frontends);
    let threads = frontends * writers;
    let job = Job::new(operations / threads, threads, hang_guard);
    let started = Instant::now();
    let all: Vec<Seen> = thread::scope(|scope| {
        let spawned: Vec<_> = (0..threads)
            .map(|writer| {
                let frontend = writer % frontends;
                let (store, node, job) = (&rig.handles[frontend], worker(frontend), &job);
                scope.spawn(move || drive(job, store, &node, writer))
            })
            .collect();
        spawned
            .into_iter()
            .map(|writer| writer.join().expect("a writer did not panic"))
            .collect()
    });
    let elapsed = started.elapsed();
    let finished = job.finished.load(Ordering::SeqCst);
    if finished < job.total {
        assert!(
            !job.hung(),
            "the run hung: {finished} of {} operations finished when the hang guard ran out",
            job.total
        );
    }
    let mut seen = Seen::default();
    for one in all {
        seen.latencies.extend(one.latencies);
        seen.lost += one.lost;
    }
    let retries = rig.handles.iter().map(PostgresStore::retries).sum();
    Report {
        frontends,
        writers,
        operations: job.total,
        elapsed,
        seen,
        claimed: job.claimed(),
        retries,
        rig,
    }
}

/// The latency at `per_cent` of the sorted, non-empty `latencies`, by nearest rank.
fn percentile(latencies: &[Duration], per_cent: usize) -> Duration {
    latencies[(latencies.len() * per_cent).div_ceil(100).saturating_sub(1)]
}

/// How many feed rows under `prefix` are puts, and how many are deletes.
fn feed_counts(store: &PostgresStore, prefix: &str) -> (usize, usize) {
    let feed = store.changes(Revision::ZERO, FEED_PAGE).expect("reads");
    let mut counts = (0, 0);
    for change in feed.rows.iter().filter(|c| c.key.starts_with(prefix)) {
        match change.event {
            Event::Put(_) => counts.0 += 1,
            _ => counts.1 += 1,
        }
    }
    counts
}

impl Report {
    /// Asserts that every operation of the run was claimed once and finished: three
    /// committed transactions each, a `done/` row and nothing else of it left, and in the
    /// change feed one put and one delete of its `queue/` row and its `lease/` row, so no
    /// claim that lost wrote anything and no grant or finish was written twice.
    fn assert_finished(&self) {
        let store = &self.rig.handles[0];
        let total = self.operations;
        assert_eq!(self.claimed, total);
        assert_eq!(self.seen.latencies.len(), 3 * total);
        for prefix in LEFT_BEHIND {
            let left = store.scan_prefix(prefix).expect("the store scans");
            assert!(left.is_empty(), "{prefix} rows are left");
        }
        let ops = u64::try_from(total).expect("a small run");
        let want: Vec<String> = (0..ops).map(|op| named("done/", op)).collect();
        let done = store.scan_prefix("done/").expect("the store scans");
        let have: Vec<String> = done.into_iter().map(|(key, _)| key).collect();
        assert_eq!(have, want, "every operation has its result");
        let noted = store.scan_prefix("doneof/").expect("the store scans");
        assert_eq!(noted.len(), total);
        assert_eq!(feed_counts(store, "queue/"), (total, total));
        assert_eq!(feed_counts(store, "lease/"), (total, total));
    }

    fn print(&self) {
        let mut sorted = self.seen.latencies.clone();
        sorted.sort();
        let writes = sorted.len();
        let (p50, p99) = (percentile(&sorted, 50), percentile(&sorted, 99));
        let seconds = self.elapsed.as_secs_f64();
        let rate = writes as f64 / seconds;
        let op_rate = self.operations as f64 / seconds;
        let (frontends, writers, operations) = (self.frontends, self.writers, self.operations);
        let (elapsed, lost, retries) = (self.elapsed, self.seen.lost, self.retries);
        println!("{frontends} frontend(s) of {writers} writers, {operations} operations:");
        println!("  {writes} writes in {elapsed:.2?}: {rate:.0} writes/s over the whole run");
        println!("  {op_rate:.0} operations/s of 3 writes each");
        println!("  commit latency: p50 {p50:.2?}, p99 {p99:.2?}");
        println!("  {lost} claims lost to another frontend, {retries} batches run again");
    }
}

/// Asserts that the farm's transaction mix commits whole on one, two and three frontends
/// that share one Postgres (every operation finishes, as in the test below), and prints the
/// writes per second and the p50 and p99 commit latency of each. It asserts nothing about
/// time.
#[test]
#[ignore = "a measurement, not a gate: run it with --ignored --nocapture on a test server"]
fn the_write_rate_of_one_two_and_three_frontends() {
    println!("target: {TARGET_WRITES_PER_SECOND} writes/s in bursts (not asserted)");
    for frontends in FRONTENDS {
        let report = run(
            frontends,
            WRITERS_PER_FRONTEND,
            MEASURED_OPERATIONS,
            Some(HANG_GUARD),
        );
        report.assert_finished();
        report.print();
    }
}

/// Asserts that, on two frontends that share one Postgres, a claim of a queued row that the
/// other has already taken loses and nothing of it is stored (the store holds the one
/// `lease/` row and the one `lease-by-op/` row of the claim that won), and that on three
/// frontends whose writers claim each other's operations every queued operation is claimed
/// exactly once and finished, with no conditional write lost: each has its result, no row of
/// it is left, and the change feed holds one grant and one finish of each.
#[test]
fn a_lost_claim_writes_nothing_and_three_frontends_finish_every_operation_once() {
    let rig = Rig::new(2);
    let (first, second) = (&rig.handles[0], &rig.handles[1]);
    submit(first, 0);
    let queued = first.scan_prefix("queue/").expect("the store scans");
    assert_eq!(queued.len(), 1);
    let won = claim(first, &worker(0), &queued[0], 7);
    let lost = claim(second, &worker(1), &queued[0], 8);
    assert!(matches!(won, Claim::Won(_)));
    assert!(matches!(lost, Claim::Lost));
    let leases = first.scan_prefix("lease/").expect("the store scans");
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].0, lease_key(7));
    let by_op = first.scan_prefix("lease-by-op/").expect("the store scans");
    assert_eq!(by_op.len(), 1);

    let report = run(3, 2, 30, None);
    report.assert_finished();
    // The measurement's own report, so that the code it runs stays covered and compiling.
    report.print();
}
