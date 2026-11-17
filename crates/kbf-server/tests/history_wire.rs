//! Build history wired into the server (`docs/design/ui-data.md`, section 5, phase U3,
//! the writer): what a build's attempts leave in the history chunks, the run id a row
//! names its run by, and that nothing about history ever blocks the recorder. Over a server
//! whose records are fed the events the executor tells the recorder (the executor's own events
//! are held to its record by `tests/executor/record.rs`), a state store and an object store the
//! tests can stall, fail and park. Each test names the defect it catches.

mod pages;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use clap::Parser as _;
use kbf_history::records::RunOutcome;
use kbf_history::{
    HistoryIndex, HistoryRoot, IndexRoot, Inline, Limits, QueueLimits, Reader, RunId, RunView,
};
use kbf_invocation::{ActionShape, Event, InvocationId, Outcome, Owner, RecordKey, WallTime};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_server::history::{HistoryError, HistoryFlags, Options, Stats, over, with_history};
use kbf_server::stores::StoreUri;
use kbf_server::{Args, StateStore};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, ManualClock, Op, Revision, ScanPage, ScanRange, Store,
    StoreError, StoreTime, Txn, Versioned,
};
use kbf_types::Digest;
use pages::{Server, digest, granted, later, queued, requeued, started};

/// The build's id.
const BUILD: &str = "0b8c2f0e-6d1a-4c55-9a7e-2f43d1c0a9b4";

fn key(id: &str) -> RecordKey {
    RecordKey::owner(InvocationId::new(id))
}

fn root() -> HistoryRoot {
    HistoryRoot::under(&KeyPrefix::new("kbf/").expect("a prefix")).expect("a root")
}

/// An object store in memory that records every key it is asked about, and can be made
/// to stall or fail its writes.
#[derive(Clone)]
struct Objects(Arc<ObjectsInner>);

struct ObjectsInner {
    memory: MemoryStore,
    /// Writes never return.
    stall_puts: AtomicBool,
    /// Writes fail at once.
    fail_puts: AtomicBool,
    /// Writes asked for, stalled and failed ones included.
    puts: AtomicU64,
    /// Every key any call named, in order.
    asked: Mutex<Vec<String>>,
}

impl Objects {
    fn new() -> Self {
        Self(Arc::new(ObjectsInner {
            memory: MemoryStore::new(Capabilities {
                conditional_put: true,
                object_lock: false,
            }),
            stall_puts: AtomicBool::new(false),
            fail_puts: AtomicBool::new(false),
            puts: AtomicU64::new(0),
            asked: Mutex::new(Vec::new()),
        }))
    }

    fn note(&self, key: &str) {
        self.0
            .asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(key.to_owned());
    }

    fn asked(&self) -> Vec<String> {
        self.0
            .asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl ObjectStore for Objects {
    fn capabilities(&self) -> Capabilities {
        self.0.memory.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.note(key.as_str());
        self.0.puts.fetch_add(1, Ordering::SeqCst);
        if self.0.stall_puts.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if self.0.fail_puts.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Protocol("injected failure".into()));
        }
        self.0.memory.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.note(key.as_str());
        self.0.memory.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.note(key.as_str());
        self.0.memory.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.note(prefix.as_str());
        self.0.memory.list(prefix, after, max_keys).await
    }
}

/// A state store over the fake whose transactions on `ui/` rows park their thread while
/// it is closed, as a store that stops answering would.
struct Gated {
    inner: Fake,
    closed: Mutex<bool>,
    opened: Condvar,
    /// Transactions parked so far.
    held: AtomicU64,
}

impl Gated {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None),
            closed: Mutex::new(false),
            opened: Condvar::new(),
            held: AtomicU64::new(0),
        })
    }

    fn close(&self) {
        *self.closed.lock().unwrap_or_else(PoisonError::into_inner) = true;
    }

    fn open(&self) {
        *self.closed.lock().unwrap_or_else(PoisonError::into_inner) = false;
        self.opened.notify_all();
    }

    /// Waits here while the store is closed.
    fn park(&self) {
        let mut closed = self.closed.lock().unwrap_or_else(PoisonError::into_inner);
        if *closed {
            self.held.fetch_add(1, Ordering::SeqCst);
        }
        while *closed {
            closed = self
                .opened
                .wait(closed)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

fn on_index(txn: &Txn) -> bool {
    txn.ops().iter().any(|op| match op {
        Op::Put { key, .. } | Op::Delete { key, .. } | Op::Check { key, .. } => {
            key.starts_with("ui/")
        }
        Op::Next { counter } => counter.starts_with("ui/"),
    })
}

impl Store for Gated {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        if batch.iter().any(on_index) {
            self.park();
        }
        self.inner.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.inner.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        self.inner.trim_feed(before)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// A server with history over [`Objects`] and a [`Gated`] state store.
struct Rig {
    server: Server,
    objects: Objects,
    state: Arc<Gated>,
    stats: Arc<Stats>,
}

/// Options for a writer that steps often and cuts a chunk as soon as it has a record.
fn quick() -> Options {
    Options {
        step_every: Duration::from_millis(20),
        limits: Limits {
            chunk_age_ms: 0,
            ..Limits::default()
        },
        ..Options::default()
    }
}

impl Rig {
    async fn start(options: Options) -> Self {
        Self::start_over(Objects::new(), options, false).await
    }

    /// A server whose state store is closed before the writer's first step.
    async fn start_parked(options: Options) -> Self {
        Self::start_over(Objects::new(), options, true).await
    }

    async fn start_over(objects: Objects, options: Options, closed: bool) -> Self {
        let state = Gated::new();
        if closed {
            state.close();
        }
        let store: Arc<dyn Store> = state.clone();
        let history = over(
            objects.clone(),
            root(),
            store.clone(),
            IndexRoot::default(),
            options,
        );
        let stats = history.stats();
        let server = Server::start_over(
            kbf_auth::Policy::from_json(pages::OPEN).expect("a policy"),
            kbf_front::DEFAULT_INVOCATION_MEMORY,
            Some(StateStore {
                store,
                members: Default::default(),
                history: Some(history),
            }),
        );
        Self {
            server,
            objects,
            state,
            stats,
        }
    }

    fn reader(&self) -> Reader<Objects> {
        let store: Arc<dyn Store> = self.state.clone();
        let index = HistoryIndex::new(store, IndexRoot::default(), Arc::new(Inline));
        Reader::new(self.objects.clone(), root(), index)
    }

    /// The writer's counters and gauges as `/metrics` renders them.
    fn metrics(&self) -> String {
        metrics_of(&self.stats)
    }
}

/// The value of the sample `series` (name and labels) in `body`.
fn sample(body: &str, series: &str) -> u64 {
    let values: Vec<&str> = body
        .lines()
        .filter_map(|l| l.strip_prefix(series)?.strip_prefix(' '))
        .collect();
    assert_eq!(values.len(), 1, "{series} once in\n{body}");
    values[0].parse().expect("an integer sample")
}

/// An operation number no other of this test binary has.
fn next_op() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A moment after any server's start, `n` milliseconds in.
fn moment(n: u64) -> WallTime {
    later(n * 1_000)
}

/// The events the executor tells the recorder of an operation of invocation `id` that ran
/// `action` as `jobs` jobs in turn, each but the last lost with its host (requeued for an
/// infrastructure reason), and the last accepted, exit 0: the call and its Command (with a
/// shape), then for each attempt `n` its grant (the worker is the job's id, `<op>-<n>`), its
/// start and its end.
fn run_events(id: &str, action: Digest, op: u64, jobs: u64) -> Vec<Event> {
    let shape = ActionShape {
        key: 0x0123_4567_89ab_cdef,
        cfg: None,
        target: None,
        test: false,
    };
    let mut events = vec![
        queued(id, "main", moment(1), action, op),
        Event::Command {
            id: InvocationId::new(id),
            owner: Owner::default(),
            action,
            command: digest(900),
            shape: Some(Box::new(shape)),
            evidence: None,
        },
    ];
    for n in 0..jobs {
        let job = format!("job-{op}-{n}");
        events.push(granted(op, n, &job, moment(2 + 10 * n)));
        events.push(started(op, n, moment(3 + 10 * n)));
        if n + 1 < jobs {
            events.push(requeued(
                op,
                n,
                kbf_invocation::RequeueReason::Infra,
                moment(4 + 10 * n),
            ));
        }
    }
    events.push(pages::finished(
        op,
        jobs - 1,
        Outcome::Ok { exit_code: 0 },
        None,
        moment(5 + 10 * jobs),
    ));
    events
}

/// A new action, run to an OK end as invocation `id`; its digest.
async fn run_one(server: &Server, id: &str) -> Digest {
    let action = digest(next_op() + 1_000);
    run_again(server, id, action, 1).await;
    action
}

/// The operation of `action` as invocation `id`, after `jobs` jobs, is told to the recorder
/// and applied.
async fn run_again(server: &Server, id: &str, action: Digest, jobs: u64) {
    server.feed(run_events(id, action, next_op(), jobs)).await;
}

/// The run ids of the attempts of the row of `action` in `id`'s record, once the recorder has
/// applied every event so far.
async fn run_ids(server: &Server, id: &str, action: Digest) -> Vec<Option<[u8; 12]>> {
    server.recorder.caught_up().await;
    let record = server.recorder.get(&key(id)).expect("a record");
    let row = record
        .rows()
        .iter()
        .find(|r| r.action == action)
        .expect("a row");
    row.attempts.iter().map(|a| a.run_id).collect()
}

/// The run `id` as history holds it, once the writer has put its chunk and its rows
/// there.
async fn resolved(reader: &Reader<Objects>, id: [u8; 12]) -> RunView {
    let id = RunId::from_bytes(id);
    for _ in 0..500 {
        if let Ok(Some(view)) = reader.run(id, &|_| true).await {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("run {id} never reached history");
}

/// Catches: a ran action that never reaches history (no tap, a tap on the wrong
/// recorder, a writer that never starts), a run id on the row that does not resolve to
/// that run (minted for another attempt, or written to the wrong row), and a record that
/// drops what the row holds: its shape, the job it was granted to, its lease, outcome or
/// any of its times.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ran_action_lands_in_a_chunk_and_its_run_id_resolves() {
    let rig = Rig::start(quick()).await;
    for _ in 0..3 {
        run_one(&rig.server, BUILD).await;
    }
    let record = rig.server.recorder.get(&key(BUILD)).expect("a record");
    assert_eq!(record.rows().len(), 3);
    let reader = rig.reader();
    for row in record.rows() {
        assert_eq!(row.attempts.len(), 1);
        let attempt = &row.attempts[0];
        let id = attempt.run_id.expect("an id minted for the attempt");
        let view = resolved(&reader, id).await;
        let run = &view.run;
        assert_eq!(run.run_id().to_bytes(), id);
        assert_eq!(run.instance, "main");
        assert_eq!(
            run.action.as_ref().expect("an action").hash,
            row.action.hash.to_vec()
        );
        assert_eq!(run.node, attempt.worker.as_str());
        assert_eq!(
            (run.lease_epoch, run.lease_seq),
            (attempt.lease.term, attempt.lease.seq)
        );
        let shape = row.shape.as_deref().expect("a shape");
        assert_ne!(shape.key, 0);
        assert_eq!(run.shape, shape.key);
        assert_eq!(run.outcome(), RunOutcome::Ok);
        let t = run.timestamps.as_ref().expect("times");
        assert_ne!(t.queued_us, 0, "the call's own time is there");
        assert_eq!(t.granted_us, attempt.granted.unix_micros());
    }
}

/// Catches: one id for a whole row (a requeued action's attempts sharing a run), ids
/// out of order within a stream (a reader's chunk lookup would miss), an attempt given
/// up for the farm's reason left without a record, and an action run twice in one
/// invocation reusing the first run's id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_attempt_gets_its_own_run_id() {
    let rig = Rig::start(quick()).await;
    let flaky = digest(777);
    run_again(&rig.server, BUILD, flaky, 3).await;
    let ids: Vec<RunId> = run_ids(&rig.server, BUILD, flaky)
        .await
        .into_iter()
        .map(|id| RunId::from_bytes(id.expect("every attempt has an id")))
        .collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(
        ids.iter().map(|id| id.seq()).collect::<Vec<_>>(),
        [0, 1, 2],
        "in the order the attempts ended"
    );
    assert!(ids.iter().all(|id| id.stream() == ids[0].stream()));
    assert!(ids.windows(2).all(|w| w[0].time_ms() <= w[1].time_ms()));
    let reader = rig.reader();
    for id in &ids {
        resolved(&reader, id.to_bytes()).await;
    }

    let again = run_one(&rig.server, BUILD).await;
    let first = run_ids(&rig.server, BUILD, again).await;
    run_again(&rig.server, BUILD, again, 1).await;
    let second = run_ids(&rig.server, BUILD, again).await;
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_ne!(first, second, "a second run of the digest is a second id");
    for id in [first[0], second[0]] {
        resolved(&reader, id.expect("an id")).await;
    }
}

/// Catches: a tap or a writer that the recorder waits for. The object store never answers a
/// write: every run is still applied to its record (the recorder's task runs the tap), the
/// writer's queue holds what it cannot take and drops, counted, past its bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_object_store_never_stops_the_recorder() {
    let rig = Rig::start(Options {
        queue: QueueLimits {
            events: 8,
            bytes: 1 << 20,
        },
        ..quick()
    })
    .await;
    rig.objects.0.stall_puts.store(true, Ordering::SeqCst);
    run_one(&rig.server, BUILD).await;
    // The writer is now inside the write that never returns, and takes no more events.
    for _ in 0..500 {
        if rig.objects.0.puts.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rig.objects.0.puts.load(Ordering::SeqCst), 1);
    for _ in 0..12 {
        run_one(&rig.server, BUILD).await;
    }
    let text = rig.metrics();
    assert_eq!(sample(&text, "kbf_history_queue_events"), 8, "{text}");
    assert_eq!(
        sample(&text, "kbf_history_dropped_total{stage=\"queue\"}"),
        4,
        "{text}"
    );
}

/// Catches: a writer whose failed writes are not counted (a lost record nobody sees)
/// or that stops the recorder for them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_object_store_counts_its_drops_and_the_recorder_goes_on() {
    let rig = Rig::start(quick()).await;
    rig.objects.0.fail_puts.store(true, Ordering::SeqCst);
    run_one(&rig.server, BUILD).await;
    for _ in 0..500 {
        if rig.stats.dropped("put") > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(rig.stats.dropped("put") > 0, "{}", rig.metrics());
    run_one(&rig.server, BUILD).await;
}

/// Catches: an index write the recorder waits for. The state store parks every
/// transaction on a history row (the writer's first, a stream block), so the writer
/// takes no event: the recorder applies every run and every event waits in the queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_blocked_state_store_never_stops_the_recorder() {
    let rig = Rig::start_parked(quick()).await;
    for _ in 0..500 {
        if rig.state.held.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        rig.state.held.load(Ordering::SeqCst),
        1,
        "the writer is parked"
    );
    for _ in 0..12 {
        run_one(&rig.server, BUILD).await;
    }
    let text = rig.metrics();
    assert_eq!(sample(&text, "kbf_history_queue_events"), 12, "{text}");
    assert_eq!(rig.stats.dropped("queue"), 0);
    rig.state.open();
}

/// Catches: a queue with no bound, a drop that is not counted, a drop that stops the
/// recorder, and an id kept on an attempt whose record was dropped (a run id that names
/// nothing).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_queue_drops_and_counts_and_the_recorder_goes_on() {
    let rig = Rig::start(Options {
        // One step at start, then none: the writer takes nothing while the test runs.
        step_every: Duration::from_secs(3600),
        queue: QueueLimits {
            events: 4,
            bytes: 1 << 20,
        },
        ..quick()
    })
    .await;
    let mut actions = Vec::new();
    for _ in 0..10 {
        actions.push(run_one(&rig.server, BUILD).await);
    }
    assert_eq!(rig.stats.dropped("queue"), 6, "{}", rig.metrics());
    let mut ids = Vec::new();
    for action in &actions {
        ids.push(run_ids(&rig.server, BUILD, *action).await[0].is_some());
    }
    assert_eq!(
        ids,
        [
            true, true, true, true, false, false, false, false, false, false
        ],
        "only a record that was kept names its run"
    );
}

/// Catches: history reaching a key outside `kbf/history/` in the bucket: the writer is
/// given a store that also holds a blob under `kbf/cas/`, and every call it makes, its
/// reads and writes alike, names a history key; the blob is as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_touches_only_keys_under_history() {
    let objects = Objects::new();
    let canary = ObjectKey::new("kbf/cas/canary").expect("a key");
    objects
        .0
        .memory
        .put_new(&canary, Bytes::from_static(b"blob"), None)
        .await
        .expect("a blob");
    let rig = Rig::start_over(objects, quick(), false).await;
    let action = run_one(&rig.server, BUILD).await;
    let id = run_ids(&rig.server, BUILD, action).await[0].expect("an id");
    resolved(&rig.reader(), id).await;
    let asked = rig.objects.asked();
    assert!(!asked.is_empty());
    assert!(
        asked.iter().all(|k| k.starts_with("kbf/history/")),
        "{asked:?}"
    );
    let all = ByteRange::new(0, 16).expect("a range");
    let blob = rig.objects.0.memory.get_range(&canary, all).await;
    assert_eq!(blob.expect("still there"), Bytes::from_static(b"blob"));
}

/// Catches: `/metrics` that names history when the server keeps none (a scraper would
/// see five families it can never move), and history whose counters are not served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_name_history_only_when_it_is_on() {
    let rig = Rig::start(quick()).await;
    let with = rig.server.get("/metrics").await;
    assert_eq!(with.code, 200, "{}", with.head);
    let with = with.text();
    for series in [
        "kbf_history_dropped_total{stage=\"queue\"}",
        "kbf_history_dropped_total{stage=\"buffer\"}",
        "kbf_history_dropped_total{stage=\"put\"}",
        "kbf_history_dropped_total{stage=\"index\"}",
        "kbf_history_dropped_total{stage=\"invocations\"}",
        "kbf_history_queue_events",
        "kbf_history_queue_bytes",
        "kbf_history_pending_rows",
        "kbf_history_open_invocations",
    ] {
        assert_eq!(sample(&with, series), 0, "{series}");
    }
    let store: Arc<dyn Store> = Gated::new();
    let off = Server::start_over(
        kbf_auth::Policy::from_json(pages::OPEN).expect("a policy"),
        kbf_front::DEFAULT_INVOCATION_MEMORY,
        Some(StateStore {
            store,
            members: Default::default(),
            history: None,
        }),
    );
    let without = off.get("/metrics").await;
    assert_eq!(without.code, 200, "{}", without.head);
    let without = without.text();
    assert!(!without.contains("kbf_history"), "{without}");
    assert!(without.contains("kbf_bytestream_write_requests_total"));
    off.stop().await;
    rig.server.stop().await;
}

/// Catches: a graceful stop that loses what the writer holds. The chunk of a run whose
/// age cut is two minutes away is cut by the stop's last step, with its rows, so the run
/// resolves after the server is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_flushes_what_the_writer_holds() {
    let options = Options {
        step_every: Duration::from_millis(20),
        ..Options::default()
    };
    let Rig {
        server,
        objects,
        state,
        stats,
    } = Rig::start(options).await;
    let action = run_one(&server, BUILD).await;
    let id = run_ids(&server, BUILD, action).await[0].expect("an id");
    // The writer has taken the record into its buffer, and has cut nothing.
    for _ in 0..500 {
        if sample(&metrics_of(&stats), "kbf_history_open_invocations") == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(objects.0.puts.load(Ordering::SeqCst), 0);
    server.stop().await;
    let store: Arc<dyn Store> = state;
    let index = HistoryIndex::new(store, IndexRoot::default(), Arc::new(Inline));
    let reader = Reader::new(objects.clone(), root(), index);
    let view = resolved(&reader, id).await;
    assert_eq!(view.run.instance, "main");
}

fn metrics_of(stats: &Stats) -> String {
    let mut text = String::new();
    stats.render(&mut text);
    text
}

/// Catches: a start that takes `--history-objects` without the state store its index
/// rows go to (a flag silently inert), by the command line or without it.
#[test]
fn history_objects_needs_a_state_store() {
    let args = Args::try_parse_from(["kbf-server", "--history-objects", "file:///var/history"])
        .expect("parses: the state store is checked when the server starts");
    assert_eq!(
        args.history.history_objects,
        Some(StoreUri::File(PathBuf::from("/var/history")))
    );
    assert!(args.state_store.is_none());
    for refused in [
        args.history.open(None).expect_err("refused"),
        with_history(None, &args.history).err().expect("refused"),
    ] {
        assert!(
            matches!(refused, HistoryError::NeedsStateStore),
            "{refused}"
        );
        assert!(refused.to_string().contains("--state-store"), "{refused}");
    }
}

/// Catches: a Postgres state store accepted beside file history (a file store would be
/// durable local state of a farm whose state is elsewhere): Postgres is not a state
/// store this build takes at all, so the pair is refused, without quoting the URL.
#[test]
fn file_objects_with_postgres_is_a_usage_error() {
    let said = Args::try_parse_from([
        "kbf-server",
        "--state-store",
        "postgres://user:hunter2@db.example/kbf",
        "--history-objects",
        "file:///var/history",
    ])
    .expect_err("refused")
    .to_string();
    assert!(said.contains("--state-store"), "{said}");
    assert!(!said.contains("hunter2"), "{said}");
}

/// Catches: an index root that is not `ui/` under an optional prefix (rows that
/// another farm's history, or the scheduler's own, would share), accepted; and one given
/// without history to use it.
#[test]
fn the_index_root_is_ui_under_a_prefix() {
    let with = |root: &str| {
        Args::try_parse_from([
            "kbf-server",
            "--state-store",
            "sqlite:///var/state.db",
            "--history-objects",
            "file:///var/history",
            "--history-index-root",
            root,
        ])
    };
    let args = with("acme/ui/").expect("a prefixed root");
    assert_eq!(
        args.history.history_index_root,
        Some(IndexRoot::under("acme/").expect("a root"))
    );
    assert_eq!(
        with("ui/").expect("the default").history.history_index_root,
        Some(IndexRoot::default())
    );
    for (refused, why) in [
        ("acme/", "must end in `ui/`"),
        ("ui", "must end in `ui/`"),
        ("", "must end in `ui/`"),
        ("tenant-ui/", "is not an index prefix"),
    ] {
        let said = with(refused).expect_err(refused).to_string();
        assert!(said.contains(why), "{refused}: {said}");
    }
    let said = Args::try_parse_from([
        "kbf-server",
        "--state-store",
        "sqlite:///var/state.db",
        "--history-index-root",
        "ui/",
    ])
    .expect_err("no objects")
    .to_string();
    assert!(said.contains("--history-objects"), "{said}");
}

/// Catches: a start without the flag that has history anyway (today's behaviour is
/// the server with none: no store opened, nothing in the state).
#[test]
fn without_the_flag_there_is_no_history() {
    let args = Args::try_parse_from(["kbf-server"]).expect("parses");
    assert_eq!(args.history.history_objects, None);
    assert!(args.history.open(None).expect("no history").is_none());
    let store: Arc<dyn Store> = Gated::new();
    let state = StateStore {
        store,
        members: Default::default(),
        history: None,
    };
    let got = with_history(Some(state), &args.history)
        .expect("no history")
        .expect("the state store is kept");
    assert!(got.history.is_none());
    assert!(
        with_history(None, &HistoryFlags::default())
            .expect("no history")
            .is_none()
    );
}

/// A scratch directory unique to this call, removed when it drops.
fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("history-wire-{name}"))
}

/// Catches: a `file://` history directory that is not opened and locked (two servers
/// would write one directory), and an error that is not told.
#[tokio::test]
async fn a_file_history_store_is_opened_and_held_by_one_server() {
    let dir = scratch("file");
    let args = Args::try_parse_from([
        "kbf-server",
        "--state-store",
        "sqlite:///var/state.db",
        "--history-objects",
        &format!("file://{}", dir.display()),
    ])
    .expect("parses");
    let store: Arc<dyn Store> = Gated::new();
    let state = StateStore {
        store,
        members: Default::default(),
        history: None,
    };
    let first = args.history.open(Some(&state)).expect("opens");
    assert!(first.is_some());
    let second = args.history.open(Some(&state)).expect_err("held");
    assert!(matches!(second, HistoryError::Open(_)), "{second}");
    assert!(
        second.to_string().starts_with("--history-objects: "),
        "{second}"
    );
}

/// Catches: an `s3://` history store that reads no key pair from the environment (a
/// writer that cannot sign), or one that needs a connection to be made.
#[tokio::test]
async fn an_s3_history_store_takes_its_key_pair_from_the_environment() {
    let args = Args::try_parse_from([
        "kbf-server",
        "--state-store",
        "sqlite:///var/state.db",
        "--history-objects",
        "s3://bucket/farm?endpoint=http://127.0.0.1:1",
    ])
    .expect("parses");
    let store: Arc<dyn Store> = Gated::new();
    let state = StateStore {
        store,
        members: Default::default(),
        history: None,
    };
    let none = |_: &str| None;
    let refused = args
        .history
        .open_with(Some(&state), &none)
        .expect_err("no key");
    assert!(
        refused.to_string().contains("AWS_ACCESS_KEY_ID"),
        "{refused}"
    );
    let keys = |name: &str| Some(format!("{name}-value"));
    let opened = args.history.open_with(Some(&state), &keys).expect("opens");
    assert!(opened.is_some());

    // `open` reads the process environment: it opens exactly when that holds the pair.
    let held = ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"]
        .iter()
        .all(|name| std::env::var(name).is_ok());
    let real = args.history.open(Some(&state));
    assert_eq!(real.is_ok(), held, "{:?}", real.err());
    // The flags and their error show as what they are.
    if let Err(refused) = &real {
        let shown = format!("{:?} {refused:?}", args.history.clone());
        assert!(
            shown.contains("history_objects") && shown.contains("Store"),
            "{shown}"
        );
    }
}
