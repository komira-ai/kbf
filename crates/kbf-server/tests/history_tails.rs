//! What a failed run's tails cost the server (`docs/design/ui-data.md` 5.3 and Q21, phase U3):
//! nothing the recorder or another run waits for. Over a server whose records are fed the events
//! the executor tells the recorder, and a blob source whose reads can be made never to answer.
//! Each test names the defect it catches.

mod pages;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_history::records::TailOmitted;
use kbf_history::{HistoryIndex, HistoryRoot, IndexRoot, Inline, Limits, Reader, RunId, RunView};
use kbf_invocation::{InvocationId, RecordKey};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStore,
    ObjectStoreError, PageSize,
};
use kbf_proto::reapi::ActionResult;
use kbf_server::StateStore;
use kbf_server::history::{BlobReadError, BlobReads, Options, TailOptions, over};
use kbf_server::http_blobs::ReadBudget;
use kbf_state::ManualClock;
use kbf_state::Store;
use kbf_state::fake::{Fake, Flaw};
use kbf_types::Digest;
use pages::Server;

const BUILD: &str = "0b8c2f0e-6d1a-4c55-9a7e-2f43d1c0a9b4";

fn root() -> HistoryRoot {
    HistoryRoot::under(&KeyPrefix::new("kbf/").expect("a prefix")).expect("a root")
}

/// An object store in memory the writer and the test's reader share.
#[derive(Clone)]
struct Objects(Arc<MemoryStore>);

impl ObjectStore for Objects {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.0.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.0.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.0.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.0.list(prefix, after, max_keys).await
    }
}

/// Blobs the tail workers read in place of the cache: those of 100 bytes never answer; the
/// rest are not held.
struct StallsOnHundred;

impl BlobReads for StallsOnHundred {
    fn read<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, BlobReadError>> {
        Box::pin(async move {
            if digest.size_bytes == 100 {
                std::future::pending::<()>().await;
            }
            Err(BlobReadError("not held".to_owned()))
        })
    }
}

/// Blobs the tail workers read slowly: every read answers, after a pause, with the same log.
struct Slow(Duration);

impl BlobReads for Slow {
    fn read<'a>(&'a self, _: &'a Digest) -> BoxFuture<'a, Result<Bytes, BlobReadError>> {
        Box::pin(async move {
            tokio::time::sleep(self.0).await;
            Ok(Bytes::from_static(b"a slow log"))
        })
    }
}

/// A server over `state`, whose records the tests feed.
fn server_over(state: StateStore) -> Server {
    let policy = kbf_auth::Policy::from_json(pages::OPEN).expect("a policy");
    Server::start_over(policy, kbf_front::DEFAULT_INVOCATION_MEMORY, Some(state))
}

/// Runs a new job `text` as invocation `BUILD`, whose result exits `exit_code` and names
/// `stdout` for its log, as the executor tells the recorder of it; the action and its run id.
async fn run(server: &Server, text: &str, stdout: &[u8], exit_code: i32) -> (Digest, [u8; 12]) {
    static NEXT_OP: AtomicU64 = AtomicU64::new(1);
    let op = NEXT_OP.fetch_add(1, Ordering::Relaxed);
    let action = pages::digest(5_000 + op);
    let log = server.store(stdout).await;
    let result = ActionResult {
        exit_code,
        stdout_digest: Some(kbf_front::digest_to_proto(&log)),
        ..Default::default()
    };
    let at = |ms: u64| pages::later(ms * 1_000);
    server
        .feed([
            pages::queued(BUILD, "main", at(1), action, op),
            pages::granted(op, 0, &format!("job-{op}-0-{text}"), at(2)),
            pages::started(op, 0, at(3)),
            pages::finished(
                op,
                0,
                kbf_invocation::Outcome::Ok { exit_code },
                Some(kbf_front::listing(None, &result)),
                at(4),
            ),
        ])
        .await;
    // A recorder stuck behind the tap would never catch up.
    tokio::time::timeout(Duration::from_secs(10), server.recorder.caught_up())
        .await
        .expect("the recorder keeps up");
    let record = server
        .recorder
        .get(&RecordKey::owner(InvocationId::new(BUILD)))
        .expect("a record");
    let row = record
        .rows()
        .iter()
        .find(|r| r.action == action)
        .expect("a row");
    let id = row.attempts[0]
        .run_id
        .expect("an id minted for the attempt");
    (action, id)
}

/// The run `id` as history holds it, once the writer has put its chunk there.
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

/// Catches (defect "read the tails inline in the tap"): a log read that never answers
/// stopping the recorder (its task runs the tap: no later run of any build would be
/// recorded), a Result held for it, a run held back for it, and a read the budget cannot
/// serve in time waiting for ever or failing the run. Of two failed runs, the first reads a
/// log that never answers and holds the whole read budget; the second finds the budget busy.
/// Both Results are acknowledged, the recorder goes on (a third run, which passes, is
/// recorded and arrives), and the second run arrives in history with `busy` tails. The first
/// does not arrive: its read is the pool's, not the farm's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_blob_read_never_delays_a_result_and_the_run_arrives_with_busy() {
    let objects = Objects(Arc::new(MemoryStore::new(Capabilities {
        conditional_put: true,
        object_lock: false,
    })));
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let options = Options {
        step_every: Duration::from_millis(20),
        limits: Limits {
            chunk_age_ms: 0,
            ..Limits::default()
        },
        tails: TailOptions {
            wait: Duration::from_millis(50),
            ..TailOptions::default()
        },
        ..Options::default()
    };
    // The source given here stays: the server gives its own cache's only to a history that
    // was given none.
    let history = over(
        objects.clone(),
        root(),
        Arc::clone(&store),
        IndexRoot::default(),
        options,
    )
    .with_blobs(Arc::new(StallsOnHundred), ReadBudget::new(100));
    let index = HistoryIndex::new(Arc::clone(&store), IndexRoot::default(), Arc::new(Inline));
    let reader = Reader::new(objects, root(), index);
    let server = server_over(StateStore {
        store,
        members: Default::default(),
        history: Some(history),
    });
    // The first run's log is 100 bytes: its read never ends, and holds all 100 of the budget.
    let (_, one) = run(&server, "stalls", &[b'x'; 100], 2).await;
    // The second's is 10 bytes, and the budget is not free.
    let (_, two) = run(&server, "waits", &[b'y'; 10], 2).await;
    let view = resolved(&reader, two).await;
    let failed = view.failed.expect("a failed-run section");
    let tail = failed.stdout.expect("a stdout tail record");
    assert_eq!(tail.omitted(), TailOmitted::Busy);
    assert_eq!((tail.bytes.len(), tail.total_bytes), (0, 10));
    // The recorder is not stuck: a run after both, that passes, is recorded and arrives.
    let (_, three) = run(&server, "after", &[b'z'; 5], 0).await;
    assert!(resolved(&reader, three).await.failed.is_none());
    let first = reader
        .run(RunId::from_bytes(one), &|_| true)
        .await
        .expect("a read");
    assert!(
        first.is_none(),
        "the run whose read never ends is in the pool"
    );
}

/// Catches (defect "a stop does not wait for the pool"): a failed run whose tails were being
/// read when the server stopped lost with them (the writer's last step would run before the
/// run reached its queue), and a stop that waits for ever for a pool that is slow. The log
/// takes 300 ms to read, and the server stops at once: the run is in the chunk the last step
/// flushed, with its tail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_waits_for_the_failed_runs_in_the_pool() {
    let objects = Objects(Arc::new(MemoryStore::new(Capabilities {
        conditional_put: true,
        object_lock: false,
    })));
    let store: Arc<dyn Store> = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let history = over(
        objects.clone(),
        root(),
        Arc::clone(&store),
        IndexRoot::default(),
        Options {
            // A chunk is cut at its age (two minutes) or at a close: only the stop's last
            // step, with every open invocation closed, flushes this one.
            step_every: Duration::from_millis(20),
            // The last step reserves a block of run ids as well as writing the rows.
            limits: Limits {
                txns_per_step: 4,
                ..Limits::default()
            },
            ..Options::default()
        },
    )
    .with_blobs(
        Arc::new(Slow(Duration::from_millis(300))),
        ReadBudget::default(),
    );
    let index = HistoryIndex::new(Arc::clone(&store), IndexRoot::default(), Arc::new(Inline));
    let reader = Reader::new(objects, root(), index);
    let server = server_over(StateStore {
        store,
        members: Default::default(),
        history: Some(history),
    });
    let (_, id) = run(&server, "slow", b"a slow log", 2).await;
    server.stop().await;
    let view = resolved(&reader, id).await;
    let tail = view
        .failed
        .expect("a failed-run section")
        .stdout
        .expect("a tail");
    assert_eq!(tail.bytes, b"a slow log");
}
