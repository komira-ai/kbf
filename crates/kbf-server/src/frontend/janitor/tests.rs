//! The `cas/b/` janitor: what it deletes and what it keeps, over an in-memory store and pack index
//! with the real key names and the real age rule (its flags, key file and a frontend started with
//! them are `flag_tests`). No test reads the clock: the sweeps are judged at a time the test
//! gives, and keys name their minute.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32};

use bytes::Bytes;
use kbf_objstore::{
    ByteRange, KeyPrefix, ListPage, ListToken, MemoryStore, ObjectKey, ObjectStoreError,
};
use kbf_pack::{DEFAULT_GRACE, Slot, TaskPackKey};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{BlobDigest, NewPack, PackBlob};
use kbf_state::ManualClock;

use super::*;

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
pub(super) fn key(minute: u32, slot: Slot) -> String {
    TaskPackKey::new("lease1", [0xab; 16], minute, slot)
        .expect("a key")
        .to_string()
}

/// The object `slot` of the put directory with nonce byte `n` minted at `minute`, as a task
/// commits it.
pub(super) fn in_dir(n: u8, minute: u32, slot: Slot) -> String {
    TaskPackKey::in_put_dir("lease1", [n; 16], minute, slot)
        .expect("a key")
        .to_string()
}

/// The put directory of `key`: what an open operation's row names.
pub(super) fn dir_of(key: &str) -> String {
    let key = TaskPackKey::parse(key).expect("a key");
    key.put_dir().expect("a put directory key")
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
    /// What the gate of the open operations answers: no directory, until a test says.
    named: Arc<Mutex<Result<BTreeSet<String>, String>>>,
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
        let named = Arc::new(Mutex::new(Ok(BTreeSet::new())));
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
            {
                let named = Arc::clone(&named);
                Arc::new(move || named.lock().expect("lock").clone())
            },
            Arc::new(|| at(NOW_MINUTE)),
        );
        let metrics = janitor.metrics();
        Self {
            store,
            index,
            verifying,
            leader,
            named,
            metrics,
            janitor,
        }
    }

    /// From now on the open operations answer `named`.
    fn name(&self, named: Result<BTreeSet<String>, String>) {
        *self.named.lock().expect("lock") = named;
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
/// (frontend packs), `cas/d/` (worker packs) and a prefix that only starts like `cas/b/` are
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

/// Asserts that a dry run deletes nothing and counts what it would have, a key an open operation
/// names not among them, and says on its gauge that it deletes nothing.
#[tokio::test]
async fn a_dry_run_counts_and_deletes_nothing() {
    let rig = Rig::with(SweepConfig {
        dry_run: true,
        ..SweepConfig::default()
    });
    rig.store.seed(&key(OLD, Slot::Pack(0))).await;
    rig.store.seed(&key(OLD, Slot::Large(0))).await;
    let named = in_dir(1, OLD, Slot::Pack(0));
    rig.store.seed(&named).await;
    rig.name(Ok(BTreeSet::from([dir_of(&named)])));

    rig.janitor.sweep_once().await;

    assert_eq!(rig.store.keys().await.len(), 3);
    assert_eq!(rig.count("would_reap"), 2);
    assert_eq!(rig.count("kept_named"), 1);
    assert_eq!(rig.count("reaped"), 0);
    let mut text = String::new();
    rig.metrics.render(&mut text);
    assert!(text.contains("\nkbf_cas_b_janitor_apply 0\n"), "{text}");
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

/// Asserts that the old objects of a put directory an open operation names are kept and counted
/// `kept_named` while an old one of a directory no operation names is deleted, and that once the
/// operation is closed the next sweep deletes them.
#[tokio::test]
async fn a_key_an_open_operation_names_is_kept_until_it_closes() {
    let rig = Rig::new();
    let named = [
        in_dir(1, OLD, Slot::Pack(0)),
        in_dir(1, OLD, Slot::Large(1)),
    ];
    let orphan = in_dir(2, OLD, Slot::Pack(0));
    for key in named.iter().chain([&orphan]) {
        rig.store.seed(key).await;
    }
    rig.name(Ok(BTreeSet::from([dir_of(&named[0])])));

    rig.janitor.sweep_once().await;

    assert_eq!(rig.store.keys().await.len(), 2);
    assert!(!rig.store.keys().await.contains(&orphan));
    assert_eq!((rig.count("kept_named"), rig.count("reaped")), (2, 1));
    rig.name(Ok(BTreeSet::new()));
    rig.janitor.sweep_once().await;
    assert!(rig.store.keys().await.is_empty());
    assert_eq!((rig.count("kept_named"), rig.count("reaped")), (2, 3));
}

/// Asserts that when the open operations cannot be read the sweep deletes nothing and is
/// counted as an error, after counting what it listed; and that the next sweep, which can read
/// them, deletes.
#[tokio::test]
async fn unreadable_operations_stop_the_sweep_and_are_counted() {
    let rig = Rig::new();
    rig.store.seed(&in_dir(2, OLD, Slot::Pack(0))).await;
    rig.name(Err("exec rows: a row is corrupt".to_owned()));

    rig.janitor.sweep_once().await;

    assert_eq!(rig.store.keys().await.len(), 1);
    assert_eq!(rig.store.deletes.load(Ordering::SeqCst), 0);
    assert_eq!(rig.count("sweep_errors"), 1);
    assert_eq!((rig.count("listed"), rig.count("sweeps")), (1, 0));
    rig.name(Ok(BTreeSet::new()));
    rig.janitor.sweep_once().await;
    assert!(rig.store.keys().await.is_empty());
}

/// Asserts that every family is rendered once with the value counted, under the
/// `kbf_cas_b_janitor_` names: the counters, and the gauge that says the sweeps delete.
#[tokio::test]
async fn the_metrics_render_every_family() {
    let rig = Rig::new();
    rig.store.seed(&key(OLD, Slot::Pack(0))).await;
    rig.store.seed(&key(YOUNG, Slot::Pack(1))).await;
    rig.store.seed("cas/b/lease1/garbage").await;
    let named = in_dir(1, OLD, Slot::Pack(0));
    rig.store.seed(&named).await;
    rig.name(Ok(BTreeSet::from([dir_of(&named)])));

    rig.janitor.sweep_once().await;

    let mut text = String::new();
    rig.metrics.render(&mut text);
    let families = [
        ("sweeps", 1),
        ("sweep_errors", 0),
        ("listed", 4),
        ("kept_young", 1),
        ("kept_verifying", 0),
        ("kept_referenced", 0),
        ("kept_named", 1),
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
    let apply = "# HELP kbf_cas_b_janitor_apply 1 when the sweeps delete (--task-janitor-apply), \
                 0 when each is a dry run.\n# TYPE kbf_cas_b_janitor_apply gauge\n\
                 kbf_cas_b_janitor_apply 1\n";
    assert!(text.starts_with(apply), "{text}");
    assert_eq!(text.matches("# TYPE").count(), families.len() + 1);
    for (name, value) in families {
        assert_eq!(rig.count(name), value, "{name}");
    }
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
