//! The state store's on-disk compatibility: a SQLite state store written by an earlier
//! release stays readable by this one, row for row.
//!
//! `tests/golden/state/` holds, per released schema version `N`, a real state store file
//! (`vN.db`, written by the code of the release through a real farm) and the rows it
//! held (`vN.rows`, a text rendering). This test opens a copy of every such file with the
//! code under test and requires:
//!
//! - the file opens (so a bumped `SCHEMA_VERSION` with no way to read the old file is red,
//!   and so is a renamed or retyped column);
//! - every row reads back unchanged: keys, values, versions, expiry times, the change
//!   feed, the head revision and a counter;
//! - a farm started over the file takes up what the scheduler wrote (every `op/`,
//!   `queue/`, `lease/`, `loss/`, `fate/` and `cordon/` row decodes), answers for the
//!   `done/` row by the key the code forms today, and accepts a result for a recovered
//!   lease (the `lease/` key and its value);
//! - there is a golden for the current `SCHEMA_VERSION`, so a bump cannot ship without a
//!   new golden (and, until a migration exists, without the old ones being refused here).
//!
//! A golden is written once per release, by a person: `cargo test -p kbf-server --test
//! state_golden -- --ignored write_golden` writes `v<SCHEMA_VERSION>.db` and `.rows`, and
//! refuses to replace a file that exists. Commit the two files on the release commit.
//! The clock of the store is a manual one that never advances, so expiry times are
//! deterministic.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use kbf_caps::NodeCaps;
use kbf_front::{Cache, Described, Finished, Submission};
use kbf_proto::google::rpc;
use kbf_proto::worker::{self, MemoryKill, ServerMessage};
use kbf_sched::{DaemonInstance, Request};
use kbf_server::farm::{NodeAction, Reported};
use kbf_server::{EpochPolicy, Farm, StateStore};
use kbf_state::sqlite::{Options, SCHEMA_VERSION, SqliteStore};
use kbf_state::{Fenced, ManualClock, Revision, Store, Txn};
use kbf_types::{
    ActionKey, Digest, DigestFunction, LeaseId, LeaseKind, OperationId, Qos, Resources, WorkerId,
};
use tokio::sync::mpsc;
use tonic::Code;

const GIB: u64 = 1 << 30;

/// The counter the writer advances twice, so a golden holds a counter row.
const COUNTER: &str = "golden/counter";
const COUNTER_AT_WRITE: u64 = 2;

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/state")
}

fn open(path: &Path) -> SqliteStore {
    SqliteStore::open(path, Arc::new(ManualClock::default()), Options::default())
        .expect("a SQLite state store")
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        write!(out, "{byte:02x}").expect("a string takes text");
    }
    out
}

/// Every row the store reads back, as text: the live keys with version and expiry, the
/// change feed with its cursor.
fn render(store: &dyn Store) -> String {
    let mut out = String::new();
    for (key, row) in store.scan_prefix("").expect("a scan") {
        let expiry = row.expires_at.map_or_else(
            || "-".to_owned(),
            |t| t.as_duration().as_nanos().to_string(),
        );
        writeln!(
            out,
            "kv {key} version={} expires_ns={expiry} value={}",
            row.version.0,
            hex(&row.value)
        )
        .expect("a string takes text");
    }
    let feed = store.changes(Revision::ZERO, 100_000).expect("the feed");
    for change in &feed.rows {
        let event = match &change.event {
            kbf_state::Event::Put(value) => format!("put {}", hex(value)),
            kbf_state::Event::Delete => "delete".to_owned(),
            kbf_state::Event::Expire => "expire".to_owned(),
        };
        writeln!(out, "feed {} {} {event}", change.revision.0, change.key)
            .expect("a string takes text");
    }
    writeln!(out, "cursor {}", feed.cursor.0).expect("a string takes text");
    writeln!(out, "counter {COUNTER} {COUNTER_AT_WRITE}").expect("a string takes text");
    out
}

fn request(tag: u8, resources: Resources) -> Request {
    Request {
        key: ActionKey {
            instance: "main".to_owned(),
            action: Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag)),
        },
        qos: Qos::Ci,
        kind: LeaseKind::Action,
        resources,
        hermetic: true,
        do_not_cache: false,
        needs: kbf_caps::Request::default(),
    }
}

fn submit(farm: &Farm, request: Request) {
    kbf_front::Dispatch::submit(
        farm,
        Submission {
            request,
            call: None,
            described: Described::default(),
        },
    )
    .expect("submitted");
}

type Inbox = mpsc::UnboundedReceiver<Result<ServerMessage, tonic::Status>>;

fn register(farm: &Farm, name: &str, room: Resources) -> (kbf_server::farm::StreamId, Inbox) {
    let caps = NodeCaps::from_report([("arch", "x86_64"), ("os", "linux")])
        .expect("caps")
        .with_drivers(["container"]);
    let (outbound, inbox) = mpsc::unbounded_channel();
    let stream = farm.register(
        &WorkerId::new(name),
        DaemonInstance::new("daemon"),
        DaemonInstance::new(""),
        Reported::serving(room, caps),
        outbound,
        ServerMessage::default(),
    );
    (stream, inbox)
}

fn starts(inbox: &mut Inbox) -> Vec<LeaseId> {
    let mut leases = Vec::new();
    while let Ok(message) = inbox.try_recv() {
        if let Some(worker::server_message::Message::Start(start)) =
            message.expect("no error").message
        {
            let lease = start.lease_id.expect("a lease id");
            leases.push(LeaseId::new(lease.term, lease.seq));
        }
    }
    leases
}

fn result(lease: LeaseId, memory_kill: MemoryKill) -> worker::Result {
    let code = match memory_kill {
        MemoryKill::OwnLimit => Code::ResourceExhausted,
        _ => Code::InvalidArgument,
    };
    worker::Result {
        lease_id: Some(worker::LeaseId {
            term: lease.term,
            seq: lease.seq,
        }),
        status: Some(rpc::Status {
            code: code as i32,
            message: "golden".to_owned(),
            details: Vec::new(),
        }),
        action_result: None,
        action_digest: None,
        memory_kill: memory_kill as i32,
        lost: 0,
    }
}

async fn accepted(farm: &Farm, worker: &str, lease: LeaseId) -> bool {
    let ack = farm
        .report(
            &WorkerId::new(worker),
            result(lease, MemoryKill::Unspecified),
        )
        .await;
    ack.expect("an acknowledgement").accepted
}

fn farm_over(store: Arc<dyn Store>) -> Farm {
    let fenced = Fenced::take(store, None).expect("the next writer generation");
    Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_state(Some(StateStore {
        store: Arc::new(fenced),
        epoch: EpochPolicy::Keep,
        members: Default::default(),
        history: None,
    }))
    .expect("a farm over the store")
}

/// Writes `vN.db` and `vN.rows` into `dir` by driving a real farm over a new store:
///
/// - `op-0` is killed for memory on a cordoned node, so the store holds its `op/`,
///   `queue/` (at the raised booking), `fate/` and the node's `cordon/` rows;
/// - `op-1` finishes on a second node, so the store holds a `done/` row;
/// - `op-2` loses its lease once, so the store holds a `loss/` row and a live `lease/`.
async fn write_into(dir: &Path) {
    let db = dir.join(format!("v{SCHEMA_VERSION}.db"));
    let rows = dir.join(format!("v{SCHEMA_VERSION}.rows"));
    assert!(
        !db.exists() && !rows.exists(),
        "{} exists: a golden is written once per release, never replaced",
        db.display()
    );
    let scratch = tempdir();
    let store = Arc::new(open(&scratch.join("state.db")));
    let farm = farm_over(store.clone());

    let (_, mut big) = register(&farm, "node-a", Resources::new(4_000, 64 * GIB));
    submit(&farm, request(1, Resources::new(1_000, 4 * GIB)));
    let [lease] = starts(&mut big)[..] else {
        panic!("one lease expected");
    };
    farm.place(&WorkerId::new("node-a"), NodeAction::Cordon)
        .expect("cordoned");
    let ack = farm
        .report(
            &WorkerId::new("node-a"),
            result(lease, MemoryKill::OwnLimit),
        )
        .await;
    assert!(ack.expect("an acknowledgement").accepted);

    let (_, mut small) = register(&farm, "node-b", Resources::new(4_000, 2 * GIB));
    submit(&farm, request(2, Resources::new(1_000, GIB)));
    let [done] = starts(&mut small)[..] else {
        panic!("one lease expected");
    };
    assert!(accepted(&farm, "node-b", done).await);
    submit(&farm, request(3, Resources::new(1_000, GIB)));
    assert_eq!(starts(&mut small).len(), 1);
    let (stream, mut small) = register(&farm, "node-b", Resources::new(4_000, 2 * GIB));
    assert!(
        farm.heartbeat(&WorkerId::new("node-b"), stream, 1, Vec::new())
            .await
    );
    assert_eq!(starts(&mut small).len(), 1, "granted again after the loss");

    drop(farm);
    for _ in 0..COUNTER_AT_WRITE {
        store
            .apply_batch(vec![Txn::new().next(COUNTER)])
            .expect("a counter");
    }
    let text = render(&*store);
    for prefix in [
        "op/",
        "queue/",
        "lease/",
        "loss/",
        "fate/",
        "done/",
        "cordon/",
        "session/",
        "meta/epoch",
        "meta/next_op",
        "meta/next_seq",
        "meta/generation",
    ] {
        assert!(
            text.lines().any(|l| l.starts_with(&format!("kv {prefix}"))),
            "the golden holds no {prefix} row"
        );
    }
    std::fs::write(&rows, text).expect("the rows are written");
    drop(store);
    std::fs::copy(scratch.join("state.db"), &db).expect("the file is copied");
    std::fs::remove_dir_all(&scratch).expect("the scratch is removed");
}

fn tempdir() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("kbf-state-golden-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// Writes the golden of this release. Run by a person on the release commit (see the
/// module docs); it refuses to replace a file.
#[tokio::test(start_paused = true)]
#[ignore = "writes the golden of the current schema version; a release-time step"]
async fn write_golden() {
    write_into(&golden_dir()).await;
}

fn goldens() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(golden_dir())
        .expect("the golden directory")
        .map(|entry| entry.expect("an entry").path())
        .filter(|path| path.extension().is_some_and(|e| e == "db"))
        .collect();
    found.sort();
    found
}

/// Catches: a change to the on-disk layout or to the rows' keys that an earlier release's
/// file does not survive: a schema version bumped with no way to read the old file, a
/// renamed or retyped column, a `done/` key format that no longer finds the rows already
/// written, a `lease/` key or value the farm no longer takes up. Each of those is a
/// rolling upgrade that loses or refuses the store.
#[tokio::test(start_paused = true)]
async fn every_golden_state_store_reads_back_unchanged() {
    let current = golden_dir().join(format!("v{SCHEMA_VERSION}.db"));
    assert!(
        current.exists(),
        "no golden for schema version {SCHEMA_VERSION}: write {} on the release commit",
        current.display()
    );
    let found = goldens();
    assert!(
        !found.is_empty(),
        "no golden at all: the test would pass empty"
    );
    for path in found {
        read_back(&path).await;
    }
}

async fn read_back(golden: &Path) {
    let name = golden
        .file_name()
        .expect("a name")
        .to_string_lossy()
        .into_owned();
    let expected = std::fs::read_to_string(golden.with_extension("rows"))
        .unwrap_or_else(|e| panic!("{name} has no .rows beside it: {e}"));
    let scratch = tempdir();
    let copy = scratch.join("state.db");
    std::fs::copy(golden, &copy).expect("a copy");

    let store = Arc::new(open(&copy));
    assert_eq!(
        render(&*store),
        expected,
        "{name}: the rows read back changed"
    );
    // The counter continues from the stored value.
    let next = store
        .apply_batch(vec![Txn::new().next(COUNTER)])
        .expect("a counter")
        .remove(0)
        .expect("committed")
        .counters;
    assert_eq!(next, vec![COUNTER_AT_WRITE + 1], "{name}: the counter row");

    let keys = |prefix: &str| -> Vec<String> {
        store
            .scan_prefix(prefix)
            .expect("a scan")
            .into_iter()
            .map(|(key, _)| key)
            .collect()
    };
    let hexes = |key: &str, skip: &str| -> u64 {
        u64::from_str_radix(key.strip_prefix(skip).expect("a prefix"), 16).expect("hex")
    };
    let [done_key] = &keys("done/")[..] else {
        panic!("{name}: expected one done/ row");
    };
    let done = OperationId(hexes(done_key, "done/"));
    let leases = keys("lease/");
    let held: Vec<LeaseId> = leases
        .iter()
        .map(|key| {
            let (term, seq) = key
                .strip_prefix("lease/")
                .and_then(|rest| rest.split_once('.'))
                .expect("a lease key");
            LeaseId::new(
                u64::from_str_radix(term, 16).expect("hex"),
                u64::from_str_radix(seq, 16).expect("hex"),
            )
        })
        .collect();
    assert!(!held.is_empty(), "{name}: no lease row to recover");

    let farm = farm_over(store.clone());
    let ended = farm.ended(done).expect("the done/ row decodes");
    assert!(
        matches!(&ended, Some(Finished::Failed(s)) if s.code == Code::InvalidArgument as i32),
        "{name}: the done/ row is not found by today's key: {ended:?}"
    );
    let (stream, _inbox) = register(&farm, "node-b", Resources::new(4_000, 2 * GIB));
    assert!(
        farm.heartbeat(&WorkerId::new("node-b"), stream, 1, held.clone())
            .await
    );
    assert!(
        accepted(&farm, "node-b", held[0]).await,
        "{name}: the recovered lease is not taken up"
    );
    drop(farm);
    drop(store);
    std::fs::remove_dir_all(&scratch).expect("the scratch is removed");
}

/// Catches: a writer that replaces a released golden (the file the next release must still
/// read would then be one the new code wrote).
#[tokio::test(start_paused = true)]
async fn the_writer_refuses_to_replace_a_golden() {
    let dir = tempdir();
    let scratch = dir.clone();
    std::fs::write(dir.join(format!("v{SCHEMA_VERSION}.db")), b"kept").expect("a file");
    let wrote = tokio::spawn(async move { write_into(&dir).await }).await;
    assert!(wrote.is_err(), "the writer replaced an existing golden");
    let _ = std::fs::remove_dir_all(&scratch);
}
