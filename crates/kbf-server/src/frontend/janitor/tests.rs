//! The `cas/b/` janitor: what it deletes and what it keeps, over an in-memory store and pack index
//! with the real key names and the real age rule; its flags and key file; and a frontend started
//! with the flags, which sweeps a store on loopback. No test reads the clock: the sweeps are
//! judged at a time the test gives, and keys name their minute.

use std::os::unix::fs::PermissionsExt as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32};

use bytes::Bytes;
use clap::error::ErrorKind;
use kbf_objstore::s3::fake::FakeS3;
use kbf_objstore::{
    ByteRange, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStoreError,
};
use kbf_pack::{DEFAULT_GRACE, Slot, TaskPackKey};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{BlobDigest, NewPack, PackBlob};
use kbf_state::ManualClock;

use super::*;
use crate::frontend::pack_tests::{flags_of, index, pack_flags, scrape, started};
use crate::frontend::{self, FrontendFlags};
use crate::test_dir::scratch_dir;

/// The minute the sweeps run at, as a number of minutes after the epoch.
const NOW_MINUTE: u32 = 100_000;
/// A key older than the six hour grace (360 minutes) by 40.
const OLD: u32 = NOW_MINUTE - 400;
/// A key inside the grace.
const YOUNG: u32 = NOW_MINUTE - 100;

fn at(minute: u32) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(u64::from(minute) * 60)
}

/// The task pack key of `slot` minted at `minute`, as the frontend writes it.
fn key(minute: u32, slot: Slot) -> String {
    TaskPackKey::new("lease1", [0xab; 16], minute, slot)
        .expect("a key")
        .to_string()
}

/// A memory object store whose deletes of chosen keys, and whose listings, fail when told to.
struct Flaky {
    inner: MemoryStore,
    refused: Mutex<Vec<String>>,
    no_listing: AtomicBool,
    deny_deletes: AtomicBool,
    refuse_every_delete: AtomicBool,
    deletes: AtomicU32,
    stops_leading: Mutex<Option<Arc<AtomicBool>>>,
}

impl Flaky {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(Capabilities::default()),
            refused: Mutex::new(Vec::new()),
            no_listing: AtomicBool::new(false),
            deny_deletes: AtomicBool::new(false),
            refuse_every_delete: AtomicBool::new(false),
            deletes: AtomicU32::new(0),
            stops_leading: Mutex::new(None),
        })
    }

    async fn seed(&self, key: &str) {
        let key = ObjectKey::new(key).expect("a key");
        let put = self
            .inner
            .put_new(&key, Bytes::from_static(b"x"), None)
            .await;
        put.expect("seeded");
    }

    /// Every key held, in order.
    async fn keys(&self) -> Vec<String> {
        let prefix = KeyPrefix::new("cas/").expect("a prefix");
        let page = self.inner.list(&prefix, None, PageSize::MAX).await;
        let objects = page.expect("listed").objects;
        objects.into_iter().map(|o| o.key.to_string()).collect()
    }

    /// From now on the store refuses to delete `key`.
    fn refuse_delete_of(&self, key: &str) {
        self.refused.lock().expect("lock").push(key.to_owned());
    }

    /// The store deletes every key again.
    fn refuse_nothing(&self) {
        self.refused.lock().expect("lock").clear();
    }
}

impl ObjectStore for Flaky {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_new(key, body, retain_until).await
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        self.inner.get_range(key, range).await
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        let service = |status: u16, code: &str| ObjectStoreError::Service {
            status,
            code: code.to_owned(),
            message: "refused".to_owned(),
        };
        if self.deny_deletes.load(Ordering::SeqCst) {
            return Err(service(403, "AccessDenied"));
        }
        if self.refuse_every_delete.load(Ordering::SeqCst) {
            return Err(service(503, "SlowDown"));
        }
        let leading = self.stops_leading.lock().expect("lock").clone();
        if let Some(leading) = leading {
            // The server stops leading as this delete is made.
            leading.store(false, Ordering::SeqCst);
        }
        let refused = self
            .refused
            .lock()
            .expect("lock")
            .contains(&key.to_string());
        if refused {
            return Err(ObjectStoreError::Service {
                status: 503,
                code: "SlowDown".to_owned(),
                message: "refused".to_owned(),
            });
        }
        self.inner.delete(key).await
    }

    async fn list(
        &self,
        prefix: &KeyPrefix,
        after: Option<&ListToken>,
        max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        if self.no_listing.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Service {
                status: 503,
                code: "SlowDown".to_owned(),
                message: "no listing".to_owned(),
            });
        }
        self.inner.list(prefix, after, max_keys).await
    }
}

/// A janitor over a [`Flaky`] store and a memory pack index, judging ages at [`NOW_MINUTE`].
struct Rig {
    store: Arc<Flaky>,
    index: Arc<MemoryPackIndex>,
    verifying: Arc<VerifyingKeys>,
    leader: Arc<AtomicBool>,
    metrics: Arc<JanitorMetrics>,
    janitor: Janitor,
}

impl Rig {
    fn new() -> Self {
        Self::with(SweepConfig::default())
    }

    fn with(config: SweepConfig) -> Self {
        let store = Flaky::new();
        let index = Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
        let verifying = Arc::new(VerifyingKeys::default());
        let leader = Arc::new(AtomicBool::new(true));
        let janitor = Janitor::new(
            Arc::clone(&store),
            Arc::clone(&index) as Arc<dyn PackIndex>,
            config,
            Duration::from_secs(60),
            Arc::clone(&verifying),
            Box::new({
                let leader = Arc::clone(&leader);
                move || leader.load(Ordering::SeqCst)
            }),
            Arc::new(|| at(NOW_MINUTE)),
        );
        let metrics = janitor.metrics();
        Self {
            store,
            index,
            verifying,
            leader,
            metrics,
            janitor,
        }
    }

    /// The count of `family` now.
    fn count(&self, family: &str) -> u64 {
        self.metrics.get(family)
    }
}

/// A pack of one blob recorded under `key`.
fn recorded(key: &str) -> NewPack {
    NewPack {
        key: key.to_owned(),
        writer: "f/fe".to_owned(),
        size: 100,
        blobs: vec![PackBlob {
            digest: BlobDigest {
                hash: [7; 32],
                size: 10,
            },
            offset: 0,
        }],
    }
}

/// Asserts that old orphan packs and old orphan large objects, which carry different slot names
/// (`-<k>` and `-L<i>`), are all deleted and counted.
#[tokio::test]
async fn old_orphan_pack_and_large_keys_are_deleted() {
    let rig = Rig::new();
    let orphans = [
        key(OLD, Slot::Pack(0)),
        key(OLD, Slot::Pack(3)),
        key(OLD, Slot::Large(0)),
        key(OLD, Slot::Large(7)),
    ];
    for orphan in &orphans {
        rig.store.seed(orphan).await;
    }

    rig.janitor.sweep_once().await;

    assert!(rig.store.keys().await.is_empty());
    assert_eq!(rig.count("listed"), 4);
    assert_eq!(rig.count("reaped"), 4);
    assert_eq!(rig.count("sweeps"), 1);
}

/// Asserts that a pack and a large object younger than the grace are kept and counted young, and
/// that the age is that of the minute in the name: a key one minute short of the grace is kept,
/// and one exactly as old as the grace is deleted.
#[tokio::test]
async fn young_keys_are_kept() {
    let rig = Rig::new();
    let grace_minutes = u32::try_from(DEFAULT_GRACE.as_secs() / 60).expect("fits");
    let edge = NOW_MINUTE - grace_minutes;
    let kept = [
        key(YOUNG, Slot::Pack(0)),
        key(YOUNG, Slot::Large(0)),
        key(edge + 1, Slot::Pack(1)),
    ];
    for young in &kept {
        rig.store.seed(young).await;
    }
    let past = key(edge, Slot::Pack(2));
    rig.store.seed(&past).await;

    rig.janitor.sweep_once().await;

    let mut want = kept.to_vec();
    want.sort();
    assert_eq!(rig.store.keys().await, want);
    assert_eq!(rig.count("kept_young"), 3);
    assert_eq!(rig.count("reaped"), 1);
}

/// Asserts that old keys under `cas/l/` (content-bound large objects), `cas/f/<writer>/`
/// (frontend packs), `cas/d/` (daemon packs) and a prefix that only starts like `cas/b/` are
/// never listed, so never deleted, and that the old task keys under `cas/b/` beside them are.
#[tokio::test]
async fn keys_under_other_prefixes_are_kept() {
    let rig = Rig::new();
    let named = |slot: Slot| key(OLD, slot);
    let others = [
        format!("cas/l/{}", "ab".repeat(32)),
        named(Slot::Large(0)).replace("cas/b/", "cas/l/"),
        named(Slot::Pack(0)).replace("cas/b/", "cas/f/writer/"),
        named(Slot::Large(0)).replace("cas/b/", "cas/f/writer/"),
        named(Slot::Pack(0)).replace("cas/b/", "cas/d/node/"),
        named(Slot::Pack(0)).replace("cas/b/", "cas/bb/"),
    ];
    for other in &others {
        rig.store.seed(other).await;
    }
    rig.store.seed(&named(Slot::Pack(0))).await;
    rig.store.seed(&named(Slot::Large(1))).await;

    rig.janitor.sweep_once().await;

    let mut want = others.to_vec();
    want.sort();
    assert_eq!(rig.store.keys().await, want);
    assert_eq!(rig.count("listed"), 2);
    assert_eq!(rig.count("reaped"), 2);
}

/// Asserts that names under `cas/b/` that are not task pack keys are kept and counted unparsed,
/// however old they might be: a name with no nonce, one whose minute is not hex, and one with an
/// unknown slot.
#[tokio::test]
async fn unparsable_names_are_kept() {
    let rig = Rig::new();
    let strays = [
        "cas/b/lease1/garbage".to_owned(),
        "cas/b/lease1/ab-L3".to_owned(),
        "cas/b/readme".to_owned(),
        key(OLD, Slot::Pack(0)).replace("-0", "-9"),
    ];
    for stray in &strays {
        rig.store.seed(stray).await;
    }
    rig.store.seed(&key(OLD, Slot::Pack(0))).await;

    rig.janitor.sweep_once().await;

    let mut want = strays.to_vec();
    want.sort();
    assert_eq!(rig.store.keys().await, want);
    assert_eq!(rig.count("kept_unparsed"), 4);
    assert_eq!(rig.count("reaped"), 1);
}

/// Asserts that a listing of many pages (a page of 2 over 9 keys) is swept to its end, every page
/// judged, with a pause after each page that has a next one (the test clock stands still until
/// the sleep is waited on, so the sweep is not paced by real time).
#[tokio::test(start_paused = true)]
async fn pagination_over_many_keys_works() {
    let config = SweepConfig {
        page: PageSize::new(2).expect("a page"),
        pause: Duration::from_secs(30),
        ..SweepConfig::default()
    };
    let rig = Rig::with(config);
    for n in 0..9 {
        rig.store.seed(&key(OLD + n, Slot::Pack(0))).await;
    }
    let young = key(YOUNG, Slot::Large(0));
    rig.store.seed(&young).await;

    rig.janitor.sweep_once().await;

    assert_eq!(rig.store.keys().await, [young]);
    assert_eq!(rig.count("listed"), 10);
    assert_eq!(rig.count("reaped"), 9);
}

/// Asserts that a delete the store refuses is counted and does not stop the pass: the other
/// orphans are deleted in it, and the key is deleted by the next pass once the store takes it.
#[tokio::test]
async fn a_delete_error_is_counted_and_the_pass_continues() {
    let rig = Rig::new();
    let stuck = key(OLD, Slot::Pack(0));
    let others = [key(OLD, Slot::Pack(1)), key(OLD, Slot::Large(0))];
    rig.store.seed(&stuck).await;
    for other in &others {
        rig.store.seed(other).await;
    }
    rig.store.refuse_delete_of(&stuck);

    rig.janitor.sweep_once().await;

    assert_eq!(rig.store.keys().await, vec![stuck.clone()]);
    assert_eq!(rig.count("reap_errors"), 1);
    assert_eq!(rig.count("reaped"), 2);
    rig.store.refuse_nothing();

    rig.janitor.sweep_once().await;

    assert!(rig.store.keys().await.is_empty());
    assert_eq!(rig.count("reap_errors"), 1);
    assert_eq!(rig.count("reaped"), 3);
}

/// Asserts that an old key an ingest is verifying is kept and counted, and deleted by a sweep
/// after the ingest lets go of it.
#[tokio::test]
async fn a_key_under_verification_is_kept_until_the_ingest_ends() {
    let rig = Rig::new();
    let (pack, large) = (key(OLD, Slot::Pack(0)), key(OLD, Slot::Large(0)));
    rig.store.seed(&pack).await;
    rig.store.seed(&large).await;
    let hold = rig
        .verifying
        .hold(vec![pack.clone(), large.clone()])
        .expect("free");

    rig.janitor.sweep_once().await;

    assert_eq!(rig.store.keys().await.len(), 2);
    assert_eq!(rig.count("kept_verifying"), 2);
    drop(hold);

    rig.janitor.sweep_once().await;

    assert!(rig.store.keys().await.is_empty());
    assert_eq!(rig.count("reaped"), 2);
}

/// Asserts that an old pack the index records is kept however old it is, and that the orphan
/// beside it is deleted: an ingested pack stays where it lies under `cas/b/`.
#[tokio::test]
async fn a_pack_the_index_records_is_kept() {
    let rig = Rig::new();
    let (held, orphan) = (key(OLD, Slot::Pack(0)), key(OLD, Slot::Pack(1)));
    rig.store.seed(&held).await;
    rig.store.seed(&orphan).await;
    rig.index.record_pack(&recorded(&held)).expect("recorded");

    rig.janitor.sweep_once().await;

    assert_eq!(rig.store.keys().await, [held]);
    assert_eq!(rig.count("kept_referenced"), 1);
    assert_eq!(rig.count("reaped"), 1);
}

/// Asserts that a server that is not the leader lists nothing and deletes nothing, and sweeps
/// once it leads.
#[tokio::test]
async fn a_follower_sweeps_nothing() {
    let rig = Rig::new();
    let orphan = key(OLD, Slot::Pack(0));
    rig.store.seed(&orphan).await;
    rig.leader.store(false, Ordering::SeqCst);
    rig.store.no_listing.store(true, Ordering::SeqCst);

    rig.janitor.sweep_once().await;

    assert_eq!(rig.store.keys().await, [orphan]);
    assert_eq!(rig.count("sweeps"), 0);
    assert_eq!(rig.count("sweep_errors"), 0);
    rig.leader.store(true, Ordering::SeqCst);
    rig.store.no_listing.store(false, Ordering::SeqCst);

    rig.janitor.sweep_once().await;

    assert!(rig.store.keys().await.is_empty());
    assert_eq!(rig.count("sweeps"), 1);
}

/// Asserts that a listing the store refuses is counted as a failed sweep, deletes nothing, and
/// leaves the next sweep to try again.
#[tokio::test]
async fn a_failed_listing_is_counted_and_retried() {
    let rig = Rig::new();
    let orphan = key(OLD, Slot::Pack(0));
    rig.store.seed(&orphan).await;
    rig.store.no_listing.store(true, Ordering::SeqCst);

    rig.janitor.sweep_once().await;

    assert_eq!(rig.count("sweep_errors"), 1);
    assert_eq!(rig.count("sweeps"), 0);
    rig.store.no_listing.store(false, Ordering::SeqCst);
    rig.janitor.sweep_once().await;
    assert!(rig.store.keys().await.is_empty());
    assert_eq!(rig.count("sweep_errors"), 1);
}

/// Asserts that a dry run deletes nothing and counts what it would have.
#[tokio::test]
async fn a_dry_run_counts_and_deletes_nothing() {
    let rig = Rig::with(SweepConfig {
        dry_run: true,
        ..SweepConfig::default()
    });
    rig.store.seed(&key(OLD, Slot::Pack(0))).await;
    rig.store.seed(&key(OLD, Slot::Large(0))).await;

    rig.janitor.sweep_once().await;

    assert_eq!(rig.store.keys().await.len(), 2);
    assert_eq!(rig.count("would_reap"), 2);
    assert_eq!(rig.count("reaped"), 0);
}

/// Asserts that the janitor sweeps again every interval: the loop is run until it has swept
/// three times, the second and third catching what was seeded after the first. The clock of the
/// test is paused and moves only when everything waits.
#[tokio::test(start_paused = true)]
async fn the_loop_sweeps_on_every_interval() {
    let rig = Rig::new();
    let mut finished = rig.metrics.finished();
    let store = Arc::clone(&rig.store);
    let looping = tokio::spawn(rig.janitor.run());
    // Each sweep that ends is told, so the test waits for sweeps and not for time.
    finished
        .wait_for(|sweeps| *sweeps >= 1)
        .await
        .expect("runs");
    store.seed(&key(OLD, Slot::Pack(0))).await;
    finished
        .wait_for(|sweeps| *sweeps >= 3)
        .await
        .expect("runs");
    assert!(store.keys().await.is_empty());
    looping.abort();
}

/// Asserts that every family is rendered once as a counter with the value counted, under the
/// `kbf_cas_b_janitor_` names.
#[tokio::test]
async fn the_metrics_render_every_family() {
    let rig = Rig::new();
    rig.store.seed(&key(OLD, Slot::Pack(0))).await;
    rig.store.seed(&key(YOUNG, Slot::Pack(1))).await;
    rig.store.seed("cas/b/lease1/garbage").await;

    rig.janitor.sweep_once().await;

    let mut text = String::new();
    rig.metrics.render(&mut text);
    let families = [
        ("sweeps", 1),
        ("sweep_errors", 0),
        ("listed", 3),
        ("kept_young", 1),
        ("kept_verifying", 0),
        ("kept_referenced", 0),
        ("kept_unparsed", 1),
        ("kept_outside", 0),
        ("abandoned", 0),
        ("reaped", 1),
        ("would_reap", 0),
        ("reap_errors", 0),
    ];
    for (name, value) in families {
        let name = format!("kbf_cas_b_janitor_{name}_total");
        assert!(text.contains(&format!("# TYPE {name} counter\n")), "{text}");
        assert!(
            text.contains(&format!("\n{name} {value}\n")),
            "{name}: {text}"
        );
    }
    assert_eq!(text.matches("# TYPE").count(), families.len());
    for (name, value) in families {
        assert_eq!(rig.count(name), value, "{name}");
    }
}

// Flags.

/// What a frontend needs of the command line apart from the janitor flags.
const BASE: [&str; 5] = [
    "frontend",
    "--storage=http://127.0.0.1:1",
    "--listen=127.0.0.1:0",
    "--worker-listen=127.0.0.1:0",
    "--api-listen=127.0.0.1:0",
];

/// The flags the task switch needs.
const TASK: [&str; 5] = [
    "--task-secret-file=/etc/kbf/task.secret",
    "--task-put-endpoint=http://127.0.0.1:9000",
    "--task-put-bucket=tiers",
    "--task-put-credentials-file=/etc/kbf/presign.json",
    "--task-read-url=https://127.0.0.1:8443",
];

/// The flags the packs need.
const PACKS: [&str; 5] = [
    "--state-store=postgres:///kbf?host=/run/postgresql",
    "--pack-index",
    "--pack-store-endpoint=http://127.0.0.1:9000",
    "--pack-store-bucket=tiers",
    "--pack-store-credentials-file=/etc/kbf/pack.json",
];

const KEY_FILE: &str = "--task-janitor-key-file=/etc/kbf/gc.json";

fn parsed(more: &[&str]) -> Result<FrontendFlags, clap::Error> {
    let argv = BASE.iter().chain(more).copied();
    frontend::parse(&frontend::command().try_get_matches_from(argv)?)
}

/// The task and pack flags the janitor needs, then `janitor`.
fn with<'a>(janitor: &[&'a str]) -> Vec<&'a str> {
    let mut all: Vec<&str> = TASK.iter().chain(&PACKS).copied().collect();
    all.extend(janitor);
    all
}

fn refused(more: &[&str]) -> ErrorKind {
    parsed(more).expect_err("refused").kind()
}

/// Asserts that no janitor exists without the key file, whatever its companions say, and that
/// the key file needs the task switch (clap) and the packs (the parse).
#[test]
fn the_janitor_is_off_unless_the_key_file_is_given() {
    let companions = [
        "--task-janitor-interval-secs=5",
        "--task-janitor-grace-secs=21600",
        "--task-janitor-page-size=10",
        "--task-janitor-page-pause-ms=0",
        "--task-janitor-dry-run",
    ];
    assert_eq!(parsed(&companions).expect("parses").janitor, None);
    assert_eq!(parsed(&with(&[])).expect("parses").janitor, None);
    assert_eq!(
        refused(&[KEY_FILE]),
        ErrorKind::MissingRequiredArgument,
        "the key file needs the task switch"
    );
    let tasks: Vec<&str> = TASK.iter().copied().chain([KEY_FILE]).collect();
    let said = parsed(&tasks).expect_err("refused");
    assert_eq!(said.kind(), ErrorKind::ArgumentConflict);
    assert!(said.to_string().contains("--pack-store-endpoint"), "{said}");
}

/// Asserts that the flags make the setting they say and that the defaults are the constants:
/// the grace of the sweep, 15 minutes between sweeps, pages of 500 and a second between them.
#[test]
fn the_flags_make_the_setting_and_the_defaults_are_the_constants() {
    let set = parsed(&with(&[KEY_FILE])).expect("parses");
    let set = set.janitor.expect("janitor flags");
    assert_eq!(set.key_file, PathBuf::from("/etc/kbf/gc.json"));
    assert_eq!(set.interval, Duration::from_secs(DEFAULT_INTERVAL_SECS));
    assert_eq!(set.grace, DEFAULT_GRACE);
    assert_eq!(set.page.get(), DEFAULT_PAGE_SIZE);
    assert_eq!(set.pause, Duration::from_millis(DEFAULT_PAGE_PAUSE_MS));
    assert!(!set.dry_run);

    let every = [
        KEY_FILE,
        "--task-janitor-interval-secs=30",
        "--task-janitor-grace-secs=21600",
        "--task-janitor-page-size=7",
        "--task-janitor-page-pause-ms=0",
        "--task-janitor-dry-run",
    ];
    let set = parsed(&with(&every)).expect("parses").janitor;
    let set = set.expect("janitor flags");
    assert_eq!(set.interval, Duration::from_secs(30));
    assert_eq!(set.grace, Duration::from_secs(21_600));
    assert_eq!(set.page.get(), 7);
    assert_eq!(set.pause, Duration::ZERO);
    assert!(set.dry_run);
}

/// Asserts the bounds: a grace under 6 hours (the age an ingest refuses a key at, and an hour of
/// margin for a fenced old leader that is still ingesting) is refused, and the least is exactly
/// that; as are an interval of 0, a page of 0 or over 1000, and a pause over a minute.
#[test]
fn the_flags_hold_to_their_bounds() {
    let margin = Duration::from_secs(3_600);
    assert_eq!(MIN_GRACE_SECS, (kbf_pack::MIN_GRACE + margin).as_secs());
    let bad = [
        "--task-janitor-grace-secs=21599",
        "--task-janitor-grace-secs=604801",
        "--task-janitor-interval-secs=0",
        "--task-janitor-interval-secs=86401",
        "--task-janitor-page-size=0",
        "--task-janitor-page-size=1001",
        "--task-janitor-page-pause-ms=60001",
    ];
    for flag in bad {
        assert_eq!(
            refused(&with(&[KEY_FILE, flag])),
            ErrorKind::ValueValidation,
            "{flag}"
        );
    }
    let edges = [
        "--task-janitor-grace-secs=21600",
        "--task-janitor-page-size=1000",
        "--task-janitor-page-size=1",
    ];
    for flag in edges {
        assert!(parsed(&with(&[KEY_FILE, flag])).is_ok(), "{flag}");
    }
}

/// The file called `name` in `dir` of this text and mode.
fn file(dir: &Path, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode set");
    path
}

const GC_KEY: &str =
    r#"{"access_key_id": "pack-gc", "secret_access_key": "hunter2", "prefix": "cas/b/"}"#;

/// What reading the key file at `path` says, as text.
fn said(path: &Path) -> String {
    match read_key(path) {
        Ok(_) => "accepted".to_owned(),
        Err(error) => error.to_string(),
    }
}

/// Asserts what the key file must be: the JSON of exactly the three fields of the tier store's
/// GC key, with the prefix exactly `cas/b/` (the janitor is given no other reach), owned by this
/// user and private; and that a refusal names the flag and the file and never the secret.
#[test]
fn the_key_file_is_the_gc_key_of_cas_b_and_private() {
    let dir = scratch_dir("janitor-key");
    assert_eq!(
        said(&file(dir.path(), "ok.json", GC_KEY, 0o600)),
        "accepted"
    );
    assert_eq!(
        said(&file(dir.path(), "ro.json", GC_KEY, 0o400)),
        "accepted"
    );

    let other = |prefix: &str| {
        let body = format!(
            r#"{{"access_key_id": "k", "secret_access_key": "hunter2", "prefix": "{prefix}"}}"#
        );
        said(&file(dir.path(), "prefix.json", &body, 0o600))
    };
    for prefix in ["cas/", "cas/l/", "cas/b", "cas/b/x/", ""] {
        let text = other(prefix);
        assert!(text.contains("must be exactly"), "{prefix:?}: {text}");
        assert!(!text.contains("hunter2"));
    }
    let bad = [
        ("open.json", GC_KEY, 0o644, "mode"),
        ("empty.json", "", 0o600, "exactly access_key_id"),
        (
            "extra.json",
            r#"{"access_key_id": "k", "secret_access_key": "hunter2", "prefix": "cas/b/", "x": 1}"#,
            0o600,
            "exactly access_key_id",
        ),
        (
            "no-id.json",
            r#"{"access_key_id": "", "secret_access_key": "hunter2", "prefix": "cas/b/"}"#,
            0o600,
            "exactly access_key_id",
        ),
        (
            "no-secret.json",
            r#"{"access_key_id": "k", "secret_access_key": "", "prefix": "cas/b/"}"#,
            0o600,
            "exactly access_key_id",
        ),
        (
            "no-prefix.json",
            r#"{"access_key_id": "k", "secret_access_key": "hunter2"}"#,
            0o600,
            "exactly access_key_id",
        ),
    ];
    for (name, body, mode, want) in bad {
        let text = said(&file(dir.path(), name, body, mode));
        assert!(text.starts_with("--task-janitor-key-file "), "{text}");
        assert!(text.contains(name), "{text}");
        assert!(text.contains(want), "{name}: {text}");
        assert!(!text.contains("hunter2"), "{text}");
    }
    let missing = said(&dir.path().join("none.json"));
    assert!(missing.starts_with("--task-janitor-key-file "), "{missing}");
}

/// The janitor flags with a key file in `dir`, a sweep every minute, nothing paused.
fn janitor_flags(dir: &Path) -> JanitorFlags {
    JanitorFlags {
        key_file: file(dir, "gc.json", GC_KEY, 0o600),
        interval: Duration::from_secs(60),
        grace: DEFAULT_GRACE,
        page: PageSize::new(2).expect("a page"),
        pause: Duration::ZERO,
        dry_run: false,
    }
}

/// Pack flags for the S3 service at `endpoint`, and packs over them.
fn packs_at(endpoint: &str, dir: &Path) -> (PackFlags, Packs) {
    let flags = flags_of(&pack_flags(endpoint, dir))
        .packs
        .expect("pack flags");
    let packs = flags.open(index()).expect("packs");
    (flags, packs)
}

/// Asserts that the janitor opened from the flags signs its requests with the key file's key and
/// no other, against the pack store: a store that takes that key alone has its old orphan
/// deleted and a `cas/l/` key beside it kept; a store that takes another key refuses the sweep,
/// which is counted, and keeps both.
#[tokio::test]
async fn the_janitor_signs_with_the_key_file_against_the_pack_store() {
    let dir = scratch_dir("janitor-open");
    let s3 = FakeS3::start().await;
    let seeded = s3.store(Capabilities::default());
    let orphan = key(1, Slot::Pack(0));
    let large = key(1, Slot::Large(0));
    let content = format!("cas/l/{}", "cd".repeat(32));
    for name in [&orphan, &large, &content] {
        let name = ObjectKey::new(name.as_str()).expect("a key");
        seeded
            .put_new(&name, Bytes::from_static(b"x"), None)
            .await
            .expect("seeded");
    }
    let (pack_flags, packs) = packs_at(s3.endpoint(), dir.path());
    let flags = janitor_flags(dir.path());
    let open = || {
        let readiness = Arc::new(Readiness::default());
        flags.open(&pack_flags, &packs, index(), readiness)
    };

    s3.require_credentials("another-key", "another-secret");
    let wrong = open().expect("opens");
    wrong.sweep_once().await;
    assert_eq!(wrong.metrics().get("sweep_errors"), 1);
    assert_eq!(s3.keys().len(), 3);

    s3.require_credentials("pack-gc", "hunter2");
    let right = open().expect("opens");
    right.sweep_once().await;
    assert_eq!(right.metrics().get("reaped"), 2);
    assert_eq!(s3.keys(), [content]);
}

/// Asserts what opening refuses: a key file that is missing, and an endpoint that is no URL; and
/// that with no janitor flags nothing is opened, and with them but no packs or index it is an
/// error that names the flags.
#[tokio::test]
async fn opening_refuses_what_cannot_work() {
    let dir = scratch_dir("janitor-refuse");
    let s3 = FakeS3::start().await;
    let (pack_flags, packs) = packs_at(s3.endpoint(), dir.path());
    let readiness = Arc::new(Readiness::default());
    let mut flags = janitor_flags(dir.path());

    flags.key_file = dir.path().join("none.json");
    let error = flags.open(&pack_flags, &packs, index(), Arc::clone(&readiness));
    assert!(matches!(error, Err(JanitorFlagError::Key { .. })));

    let flags = janitor_flags(dir.path());
    let mut broken = pack_flags.clone();
    broken.endpoint = "not a url".to_owned();
    let error = flags.open(&broken, &packs, index(), Arc::clone(&readiness));
    let error = error.err().expect("refused").to_string();
    assert!(error.starts_with("--pack-store-endpoint "), "{error}");

    let index = index();
    let none = super::open(None, None, None, None, &readiness).expect("opens");
    assert!(none.is_none());
    let asked = Some(&flags);
    let error = super::open(asked, None, None, Some(&index), &readiness);
    let error = error.err().expect("refused").to_string();
    assert!(error.contains("--task-janitor-key-file needs"), "{error}");
    let opened = super::open(
        asked,
        Some(&pack_flags),
        Some(&packs),
        Some(&index),
        &readiness,
    );
    assert!(opened.expect("opens").is_some());
}

/// Asserts the whole path from the flags: a frontend started with the janitor flags over a store
/// on loopback sweeps it as it serves, deleting the old orphan pack and large object under
/// `cas/b/` and leaving a `cas/l/` key, and counts it on `GET /metrics`. The orphans carry the
/// minute 1, so they are old at any time the sweep runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_with_the_janitor_flags_sweeps_the_store_as_it_serves() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("janitor-frontend");
    let seeded = s3.store(Capabilities::default());
    let content = format!("cas/l/{}", "ef".repeat(32));
    let names = [
        key(1, Slot::Pack(0)),
        key(1, Slot::Large(0)),
        content.clone(),
    ];
    for name in &names {
        let name = ObjectKey::new(name.as_str()).expect("a key");
        let body = Bytes::from_static(b"x");
        seeded.put_new(&name, body, None).await.expect("seeded");
    }
    let secret = file(
        dir.path(),
        "task.secret",
        "0123456789abcdef0123456789abcdef",
        0o600,
    );
    let presign = file(dir.path(), "presign.json", GC_KEY, 0o600);
    let gc = file(dir.path(), "gc.json", GC_KEY, 0o600);
    let mut more = pack_flags(s3.endpoint(), dir.path());
    more.extend([
        format!("--task-secret-file={}", secret.display()),
        "--task-put-endpoint=http://127.0.0.1:9".to_owned(),
        "--task-put-bucket=kbf-fake".to_owned(),
        format!("--task-put-credentials-file={}", presign.display()),
        "--task-read-url=https://127.0.0.1:8443".to_owned(),
        format!("--task-janitor-key-file={}", gc.display()),
        "--task-janitor-interval-secs=1".to_owned(),
    ]);
    let flags = flags_of(&more);
    assert!(flags.janitor.is_some());
    let handle = started(&flags, Some(index())).expect("starts");
    let api = handle.api.expect("the operator API");
    let mut sweeps = handle.janitor_sweeps().expect("a janitor");
    let served = tokio::spawn(handle.serve());

    // Each sweep that ends is told, so the scrape is repeated per sweep and not per time.
    let mut text = scrape(api).await;
    while !text.contains("kbf_cas_b_janitor_reaped_total 2\n") {
        sweeps.changed().await.expect("the janitor runs");
        text = scrape(api).await;
    }

    assert!(s3.keys().contains(&content), "{:?}", s3.keys());
    assert!(!s3.keys().contains(&names[0]));
    assert!(!s3.keys().contains(&names[1]));
    assert!(text.contains("kbf_cas_b_janitor_sweeps_total"), "{text}");
    served.abort();
}

/// Asserts that a frontend without the janitor flags exposes no janitor family and sweeps nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_without_the_janitor_flags_has_no_janitor() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("janitor-off");
    let flags = flags_of(&pack_flags(s3.endpoint(), dir.path()));
    assert_eq!(flags.janitor, None);
    let handle = started(&flags, Some(index())).expect("starts");
    let api = handle.api.expect("the operator API");
    let served = tokio::spawn(handle.serve());

    let text = scrape(api).await;

    assert!(!text.contains("kbf_cas_b_janitor_"), "{text}");
    served.abort();
}

/// Asserts that leadership lost as a delete is made stops the next delete of the same page: one
/// key is reaped, the rest stay, and the sweep is counted abandoned and not as an error.
#[tokio::test]
async fn leadership_lost_mid_sweep_stops_the_deletes() {
    let rig = Rig::new();
    for n in 0..4 {
        rig.store.seed(&key(OLD + n, Slot::Pack(0))).await;
    }
    *rig.store.stops_leading.lock().expect("lock") = Some(Arc::clone(&rig.leader));

    rig.janitor.sweep_once().await;

    assert_eq!(rig.store.keys().await.len(), 3);
    assert_eq!(rig.count("reaped"), 1);
    assert_eq!(rig.count("abandoned"), 1);
    assert_eq!(rig.count("sweep_errors"), 0);
}

/// Asserts that a delete the store answers 403 ends the sweep at once: one delete is asked and
/// not one per key, the sweep is counted as an error once, and the next sweep does the same.
#[tokio::test]
async fn a_403_on_delete_aborts_the_sweep() {
    let rig = Rig::new();
    for n in 0..4 {
        rig.store.seed(&key(OLD + n, Slot::Pack(0))).await;
    }
    rig.store.deny_deletes.store(true, Ordering::SeqCst);

    rig.janitor.sweep_once().await;

    assert_eq!(rig.store.deletes.load(Ordering::SeqCst), 1);
    assert_eq!(rig.count("sweep_errors"), 1);
    assert_eq!(rig.count("sweeps"), 0, "a sweep that failed did not end");
    // What the sweep did before it stopped is counted: the page it listed.
    assert_eq!(rig.count("listed"), 4);
    assert_eq!(rig.store.keys().await.len(), 4);
    rig.janitor.sweep_once().await;
    assert_eq!(rig.store.deletes.load(Ordering::SeqCst), 2);
    assert_eq!(rig.count("sweep_errors"), 2);
}

/// Asserts that a store that answers every delete 503 is asked only as many times as the sweep
/// tolerates in a row (3 here), and the sweep is counted as an error.
#[tokio::test]
async fn deletes_that_keep_failing_stop_the_sweep() {
    let rig = Rig::with(SweepConfig {
        max_consecutive_failures: 3,
        ..SweepConfig::default()
    });
    for n in 0..6 {
        rig.store.seed(&key(OLD + n, Slot::Pack(0))).await;
    }
    rig.store.refuse_every_delete.store(true, Ordering::SeqCst);

    rig.janitor.sweep_once().await;

    assert_eq!(rig.store.deletes.load(Ordering::SeqCst), 3);
    assert_eq!(rig.count("sweep_errors"), 1);
    // What the sweep did before it stopped is counted: the page it listed, the deletes refused.
    assert_eq!(rig.count("listed"), 6);
    assert_eq!(rig.count("reap_errors"), 3);
    assert_eq!(rig.count("reaped"), 0);
    assert_eq!(rig.store.keys().await.len(), 6);
}
