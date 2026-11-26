//! The collector of tree lists: what a pass deletes and what it keeps, over the S3 service on
//! loopback (whose clock stamps each object it takes, as a test sets it), the pack index in
//! memory and the operations' rows over the state store in memory. No test reads the clock: a
//! pass is judged at [`NOW`], and each object is stamped with the time the test gives.

use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use kbf_objstore::s3::fake::{Failure, FakeS3};
use kbf_objstore::{ObjectKey, ObjectStore, PageSize};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{BlobDigest, Location, NewPack, PackBlob, PackIndexError, PackInfo, Recorded};
use kbf_state::{ManualClock, Store};
use kbf_types::OperationId;

use super::pass::PassError;
use super::*;
use crate::executor::rows::{OP_PREFIX, OpRow, op_key};
use crate::executor::tests::Faulty;

#[path = "flag_tests.rs"]
mod flag_tests;

/// The second a pass is judged at.
pub(super) const NOW: u64 = 2_000_000_000;
/// A day, in seconds.
pub(super) const DAY: u64 = 86_400;
/// Written a day and a second before [`NOW`]: old enough to delete.
pub(super) const OLD: u64 = NOW - DAY - 1;
/// Written a day before [`NOW`]: not older than a day, kept.
pub(super) const A_DAY: u64 = NOW - DAY;

/// The id of the tree list of the tag `tag`, in hex.
pub(super) fn id(tag: u8) -> String {
    format!("{tag:02x}").repeat(32)
}

/// `cas/l/<id>` of the tag `tag`.
pub(super) fn list_key(tag: u8) -> String {
    format!("{LIST_PREFIX}{}", id(tag))
}

/// `cas/l2/<id>` of the tag `tag`.
pub(super) fn sidecar_key(tag: u8) -> String {
    format!("{SIDECAR_PREFIX}{}", id(tag))
}

/// A pack index that fails every call, or panics in the one the collector makes.
struct Down {
    panics: bool,
}

impl PackIndex for Down {
    fn record_pack(&self, _: &NewPack) -> Result<Recorded, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn pack(&self, _: &str) -> Result<Option<PackInfo>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn locate(&self, _: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn find_missing(&self, _: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn touch(&self, _: &[BlobDigest]) -> Result<usize, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn dead_packs(&self, _: Duration, _: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn unreferenced_keys(&self, _: &[String]) -> Result<Vec<String>, PackIndexError> {
        assert!(!self.panics, "the index broke");
        Err(PackIndexError::NotMigrated)
    }
}

/// A collector over a store on loopback, a pack index and rows in memory.
pub(super) struct Rig {
    pub(super) s3: FakeS3,
    pub(super) index: Arc<MemoryPackIndex>,
    pub(super) state: Arc<Faulty>,
    pub(super) leader: Arc<AtomicBool>,
}

impl Rig {
    pub(super) async fn new() -> Self {
        Self {
            s3: FakeS3::start().await,
            index: Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default()))),
            state: Arc::new(Faulty::new(Arc::new(ManualClock::default()))),
            leader: Arc::new(AtomicBool::new(true)),
        }
    }

    /// The rows of a frontend over the rig's state store.
    fn rows(&self) -> Rows {
        Rows::new(Arc::clone(&self.state) as Arc<dyn Store>, "fe")
    }

    /// The pass `config` describes over the rig's two stores and `index`.
    fn pass_over(&self, config: PassConfig, index: Arc<dyn PackIndex>) -> Pass {
        let stores = Stores {
            lists: self.s3.store(Capabilities::default()),
            sidecars: self.s3.store(Capabilities::default()),
        };
        let leader = Arc::clone(&self.leader);
        let is_leader = Box::new(move || leader.load(Ordering::SeqCst));
        Pass::new(stores, index, Arc::new(self.rows()), config, is_leader)
    }

    /// A collector that deletes (`apply`) or counts, over pages of two.
    fn sweep(&self, apply: bool) -> ListSweep {
        self.sweep_with(PassConfig { apply, ..config() })
    }

    fn sweep_with(&self, config: PassConfig) -> ListSweep {
        let index = Arc::clone(&self.index) as Arc<dyn PackIndex>;
        self.sweep_of(self.pass_over(config, index))
    }

    fn sweep_of(&self, pass: Pass) -> ListSweep {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(NOW);
        ListSweep::new(pass, Duration::from_secs(60), Arc::new(move || now))
    }

    /// Puts `bytes` at `key`, stamped `at` (seconds since the epoch).
    pub(super) async fn put(&self, key: &str, bytes: usize, at: u64) {
        self.s3.set_time(at);
        let key = ObjectKey::new(key).expect("a key");
        let body = Bytes::from(vec![7; bytes]);
        let store = self.s3.store(Capabilities::default());
        store.put_new(&key, body, None).await.expect("stored");
    }

    /// The list of the tag `tag`, of `bytes` bytes, and its sidecar, both written at `at`.
    pub(super) async fn list(&self, tag: u8, bytes: usize, at: u64) {
        self.put(&list_key(tag), bytes, at).await;
        self.put(&sidecar_key(tag), 40, at).await;
    }

    /// An open operation `n` whose row names the list of the tag `tag`.
    fn open(&self, n: u64, tag: u8) {
        let row = OpRow {
            owner: String::new(),
            instance: "main".to_owned(),
            hash: "ab".repeat(32),
            size: 9,
            attempt: 0,
            job: format!("k0-{n}-0"),
            key: Some("cas/b/1-1/aa".to_owned()),
            whole_machine: false,
            tree: Some(list_key(tag)),
        };
        self.rows().put_op(OperationId(n), &row).expect("a row");
    }

    pub(super) fn keys(&self) -> Vec<String> {
        self.s3.keys()
    }
}

/// Pages of two, no pause, three failures in a row stop a pass; a dry run.
fn config() -> PassConfig {
    PassConfig {
        apply: false,
        page: PageSize::new(2).expect("a page"),
        pause: Duration::ZERO,
        max_consecutive_failures: 3,
    }
}

/// Catches an old list kept for ever, or deleted without its sidecar, and a delete of anything
/// a sidecar does not name: a list older than a day that no open operation names, and its
/// sidecar, are deleted and counted with the list's bytes; a list a day old, and an old object
/// of `cas/l/` that no sidecar names (a manifest or a large blob of the CAS), stay.
#[tokio::test]
async fn a_list_older_than_a_day_that_no_operation_names_is_deleted_with_its_sidecar() {
    let rig = Rig::new().await;
    rig.list(1, 300, OLD).await;
    rig.list(2, 500, A_DAY).await;
    let manifest = list_key(9);
    rig.put(&manifest, 64, OLD - 30 * DAY).await;
    let sweep = rig.sweep(true);

    let report = sweep.sweep_once().await;

    let mut kept = vec![list_key(2), manifest, sidecar_key(2)];
    kept.sort();
    assert_eq!(rig.keys(), kept);
    assert_eq!(
        (
            report.deleted_lists,
            report.deleted_bytes,
            report.deleted_sidecars
        ),
        (1, 300, 1)
    );
    assert_eq!((report.sidecars, report.young), (2, 1));
    let metrics = sweep.metrics();
    assert_eq!(metrics.get("deleted_lists"), 1);
    assert_eq!(metrics.get("deleted_list_bytes"), 300);
    assert_eq!(metrics.get("passes"), 1);
    assert_eq!(metrics.get("would_delete_lists"), 0);
}

/// Catches a list deleted under a job that still needs it: an old list an open operation's row
/// names is kept and counted at every pass, whatever its age, and two operations naming one list
/// keep it until both have finished; once no row names it the next pass deletes it.
#[tokio::test]
async fn a_list_an_open_operation_names_is_kept_whatever_its_age() {
    let rig = Rig::new().await;
    rig.list(1, 100, OLD - 300 * DAY).await;
    rig.list(2, 100, OLD).await;
    rig.open(1, 1);
    rig.open(2, 1);
    let sweep = rig.sweep(true);

    let report = sweep.sweep_once().await;
    assert_eq!((report.named, report.deleted_lists), (1, 1));
    let mut kept = vec![list_key(1), sidecar_key(1)];
    kept.sort();
    assert_eq!(rig.keys(), kept);

    rig.state
        .txn(kbf_state::Txn::new().delete(op_key(OperationId(1)), kbf_state::Cond::Any))
        .expect("a write")
        .expect("no conflict");
    assert_eq!(
        sweep.sweep_once().await.named,
        1,
        "operation 2 still names it"
    );
    assert_eq!(rig.keys(), kept);

    rig.state
        .txn(kbf_state::Txn::new().delete(op_key(OperationId(2)), kbf_state::Cond::Any))
        .expect("a write")
        .expect("no conflict");
    let report = sweep.sweep_once().await;
    assert_eq!((report.named, report.deleted_lists), (0, 1));
    assert_eq!(rig.keys(), Vec::<String>::new());
    assert_eq!(sweep.metrics().get("kept_named"), 2);
}

/// Catches a dry run that deletes, or counts what it would not delete: without apply nothing is
/// deleted, and the lists and sidecars an apply would delete (a sidecar whose list is gone
/// among them) are counted with the lists' bytes, as a dry run on `GET /metrics`; a list an
/// operation names and a young one are not counted as deletable.
#[tokio::test]
async fn a_dry_run_deletes_nothing_and_counts() {
    let rig = Rig::new().await;
    rig.list(1, 300, OLD).await;
    rig.list(2, 700, OLD - DAY).await;
    rig.put(&sidecar_key(3), 40, OLD).await;
    rig.list(4, 100, OLD).await;
    rig.open(1, 4);
    rig.list(5, 100, NOW).await;
    let before = rig.keys();
    let sweep = rig.sweep(false);

    let report = sweep.sweep_once().await;

    assert_eq!(rig.keys(), before, "nothing deleted");
    assert_eq!(
        (
            report.would_delete_lists,
            report.would_delete_bytes,
            report.would_delete_sidecars
        ),
        (2, 1000, 3)
    );
    assert_eq!(
        (
            report.deleted_lists,
            report.deleted_sidecars,
            report.named,
            report.young
        ),
        (0, 0, 1, 1)
    );
    let mut text = String::new();
    sweep.metrics().render(&mut text);
    for line in [
        "kbf_tree_list_sweep_apply 0\n",
        "kbf_tree_list_sweep_would_delete_lists_total 2\n",
        "kbf_tree_list_sweep_would_delete_list_bytes_total 1000\n",
        "kbf_tree_list_sweep_would_delete_sidecars_total 3\n",
        "kbf_tree_list_sweep_deleted_lists_total 0\n",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
}

/// Asserts the age rule at its edge and the count by age: a list written exactly a day before
/// the pass is kept, one a second older is deleted; the lists the pass finds are counted by
/// whole days (a second under a week in the slot of 6), a week or more in the last slot, with
/// their bytes; and a list stamped after the
/// pass's time is young. Catches: a list deleted at a day or less, or an age counted in the
/// wrong slot.
#[tokio::test]
async fn a_list_a_day_old_or_younger_is_kept_and_the_lists_are_counted_by_age() {
    let rig = Rig::new().await;
    rig.list(1, 10, A_DAY).await;
    rig.list(2, 20, OLD).await;
    rig.list(3, 30, NOW - 3 * DAY).await;
    rig.list(4, 40, NOW - 9 * DAY).await;
    rig.list(5, 50, NOW - 7 * DAY).await;
    rig.list(6, 60, NOW + 60).await;
    rig.list(7, 70, NOW - 7 * DAY + 1).await;
    let sweep = rig.sweep(true);

    let report = sweep.sweep_once().await;

    assert_eq!((report.young, report.deleted_lists), (2, 5));
    let mut kept = vec![list_key(1), list_key(6), sidecar_key(1), sidecar_key(6)];
    kept.sort();
    assert_eq!(rig.keys(), kept);
    let aged = |lists, bytes| Aged { lists, bytes };
    let mut want = [Aged::default(); AGE_DAYS];
    want[0] = aged(1, 60);
    want[1] = aged(2, 30);
    want[3] = aged(1, 30);
    want[6] = aged(1, 70);
    want[7] = aged(2, 90);
    assert_eq!(report.by_day, want);
    let mut text = String::new();
    sweep.metrics().render(&mut text);
    for line in [
        "kbf_tree_list_sweep_lists{age_days=\"1\"} 2\n",
        "kbf_tree_list_sweep_list_bytes{age_days=\"1\"} 30\n",
        "kbf_tree_list_sweep_lists{age_days=\"7+\"} 2\n",
        "kbf_tree_list_sweep_list_bytes{age_days=\"7+\"} 90\n",
        "kbf_tree_list_sweep_lists{age_days=\"2\"} 0\n",
        "kbf_tree_list_sweep_lists{age_days=\"6\"} 1\n",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
}

/// Asserts that a sidecar whose list is gone (a list whose write failed after its sidecar's, or
/// whose sidecar delete failed) is deleted once the sidecar is older than a day, and kept
/// before, or while an open operation's row names its list.
#[tokio::test]
async fn a_sidecar_whose_list_is_gone_is_deleted_once_it_is_older_than_a_day() {
    let rig = Rig::new().await;
    rig.put(&sidecar_key(1), 40, OLD).await;
    rig.put(&sidecar_key(2), 40, A_DAY).await;
    rig.put(&sidecar_key(3), 40, OLD).await;
    rig.open(1, 3);
    let report = rig.sweep(true).sweep_once().await;
    let mut kept = vec![sidecar_key(2), sidecar_key(3)];
    kept.sort();
    assert_eq!(rig.keys(), kept);
    assert_eq!(
        report.named, 1,
        "a list an open row names is coming back: its sidecar stays"
    );
    assert_eq!(
        (report.deleted_sidecars, report.deleted_lists, report.young),
        (1, 0, 1)
    );
    assert_eq!(
        report.by_day,
        [Aged::default(); AGE_DAYS],
        "no list was found"
    );
}

/// Catches a large blob of the CAS deleted because its bytes are those of a list: an old list
/// whose key the pack index records as a pack is kept, with its sidecar.
#[tokio::test]
async fn a_list_the_pack_index_records_is_kept() {
    let rig = Rig::new().await;
    rig.list(1, 100, OLD).await;
    rig.list(2, 100, OLD).await;
    rig.index
        .record_pack(&NewPack {
            key: list_key(1),
            writer: "f/fe".to_owned(),
            size: 100,
            blobs: vec![PackBlob {
                digest: BlobDigest {
                    hash: [1; 32],
                    size: 100,
                },
                offset: 0,
            }],
        })
        .expect("recorded");
    let report = rig.sweep(true).sweep_once().await;
    let mut kept = vec![list_key(1), sidecar_key(1)];
    kept.sort();
    assert_eq!(rig.keys(), kept);
    assert_eq!((report.recorded, report.deleted_lists), (1, 1));
}

/// Asserts that a sidecar key that names no list id (too short, upper-case hex, not hex) is
/// kept and counted, and no `cas/l/` key is made of it.
#[tokio::test]
async fn a_sidecar_key_that_names_no_list_id_is_kept() {
    let rig = Rig::new().await;
    let strays = [
        format!("{SIDECAR_PREFIX}abc"),
        format!("{SIDECAR_PREFIX}{}", "AB".repeat(32)),
        format!("{SIDECAR_PREFIX}{}", "zz".repeat(32)),
    ];
    for stray in &strays {
        rig.put(stray, 40, OLD).await;
    }
    let report = rig.sweep(true).sweep_once().await;
    assert_eq!(report.unparsed, 3);
    assert_eq!(rig.keys().len(), 3);
    assert_eq!(report.deleted_sidecars, 0);
}

/// Catches a pass that deletes when what the rows name is not known: a row of a later release
/// stops the pass before the page's deletes, so do a corrupt row, a state store that fails and
/// one that panics; each is counted as an error and deletes nothing, and the pass deletes once
/// the rows can be read.
#[tokio::test]
async fn a_row_that_cannot_be_read_stops_the_pass_before_any_delete() {
    let rig = Rig::new().await;
    rig.list(1, 100, OLD).await;
    let before = rig.keys();
    let sweep = rig.sweep(true);
    let put = |value: &[u8]| {
        rig.state
            .txn(kbf_state::Txn::new().put(
                format!("{OP_PREFIX}{:016x}", 7),
                value.to_vec(),
                kbf_state::Cond::Any,
            ))
            .expect("a write")
            .expect("no conflict");
    };
    put(&[2, b'{', b'}']);
    sweep.sweep_once().await;
    put(b"\x01{broken");
    sweep.sweep_once().await;
    put(b"\x01{\"owner\":\"o\",\"instance\":\"main\",\"hash\":\"ab\",\"size\":1,\"attempt\":0,\"job\":\"j\"}");
    rig.state.reads_fail.store(true, Ordering::SeqCst);
    sweep.sweep_once().await;
    rig.state.reads_fail.store(false, Ordering::SeqCst);
    rig.state.reads_panic.store(true, Ordering::SeqCst);
    sweep.sweep_once().await;
    assert_eq!(rig.keys(), before);
    let metrics = sweep.metrics();
    assert_eq!(metrics.get("pass_errors"), 4);
    assert_eq!(metrics.get("passes"), 0);
    assert_eq!(
        metrics.get("sidecars_listed"),
        4,
        "each pass listed its page"
    );

    rig.state.reads_panic.store(false, Ordering::SeqCst);
    assert_eq!(sweep.sweep_once().await.deleted_lists, 1);
    assert_eq!(rig.keys(), Vec::<String>::new());
}

/// Catches a pass that deletes when the pack index cannot say what it records: an index that
/// fails, or that panics, stops the pass before any delete, and is counted as an error.
#[tokio::test]
async fn a_pack_index_that_fails_stops_the_pass_before_any_delete() {
    let rig = Rig::new().await;
    rig.list(1, 100, OLD).await;
    let before = rig.keys();
    for panics in [false, true] {
        let config = PassConfig {
            apply: true,
            ..config()
        };
        let sweep = rig.sweep_of(rig.pass_over(config, Arc::new(Down { panics })));
        let mut report = PassReport::default();
        let ended = sweep.pass.run(
            SystemTime::UNIX_EPOCH + Duration::from_secs(NOW),
            &mut report,
        );
        let error = ended.await.expect_err("stopped");
        let want = if panics {
            "a call of the pack index did not return"
        } else {
            "the pack index failed: "
        };
        assert!(error.to_string().starts_with(want), "{error}");
        sweep.sweep_once().await;
        assert_eq!(sweep.metrics().get("pass_errors"), 1);
    }
    assert_eq!(rig.keys(), before);
}

/// Asserts that a frontend that does not lead lists nothing and deletes nothing, and that one
/// that stops leading between two deletes stops there: the pass is counted abandoned, not as an
/// error, and the lists after it stay.
#[tokio::test]
async fn a_follower_deletes_nothing_and_a_lost_leadership_stops_the_deletes() {
    let rig = Rig::new().await;
    for tag in 1..=4 {
        rig.list(tag, 100, OLD).await;
    }
    rig.leader.store(false, Ordering::SeqCst);
    let sweep = rig.sweep(true);
    let report = sweep.sweep_once().await;
    assert_eq!(report, PassReport::default(), "nothing listed");
    assert_eq!(rig.keys().len(), 8);
    assert_eq!(sweep.metrics().get("abandoned"), 0, "never led");

    rig.leader.store(true, Ordering::SeqCst);
    let leader = Arc::clone(&rig.leader);
    let pass = rig.pass_over(
        PassConfig {
            apply: true,
            ..config()
        },
        Arc::clone(&rig.index) as Arc<dyn PackIndex>,
    );
    // The leadership is asked before the first page, before its two deletes, then before the
    // second page: it is lost there.
    let asked = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let pass = Pass {
        is_leader: Box::new(move || {
            asked.fetch_add(1, Ordering::SeqCst) < 3 && leader.load(Ordering::SeqCst)
        }),
        ..pass
    };
    let sweep = rig.sweep_of(pass);
    let report = sweep.sweep_once().await;
    assert!(report.abandoned && report.led && !report.whole);
    assert_eq!(report.deleted_lists, 2);
    assert_eq!(rig.keys().len(), 4);
    assert_eq!(sweep.metrics().get("abandoned"), 1);
    assert_eq!(sweep.metrics().get("pass_errors"), 0);
}

/// Asserts that leadership lost before a page's deletes stops them all: the page is listed and
/// asked, and nothing of it is deleted.
#[tokio::test]
async fn leadership_lost_before_the_deletes_of_a_page_deletes_none_of_it() {
    let rig = Rig::new().await;
    rig.list(1, 100, OLD).await;
    let leader = Arc::clone(&rig.leader);
    let asked = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let pass = Pass {
        is_leader: Box::new(move || {
            asked.fetch_add(1, Ordering::SeqCst) < 1 && leader.load(Ordering::SeqCst)
        }),
        ..rig.pass_over(
            PassConfig {
                apply: true,
                ..config()
            },
            Arc::clone(&rig.index) as Arc<dyn PackIndex>,
        )
    };
    let report = rig.sweep_of(pass).sweep_once().await;
    assert!(report.abandoned);
    assert_eq!((report.sidecars, report.deleted_lists), (1, 0));
    assert_eq!(rig.keys().len(), 2);
}

/// Asserts that the listing is followed page after page to its end: seven lists over pages of
/// two are all deleted and the pass is whole.
#[tokio::test]
async fn the_pass_follows_the_listing_over_many_pages() {
    let rig = Rig::new().await;
    for tag in 1..=7 {
        rig.list(tag, 10, OLD).await;
    }
    let report = rig.sweep(true).sweep_once().await;
    assert!(report.whole);
    assert_eq!(
        (
            report.sidecars,
            report.deleted_lists,
            report.deleted_sidecars
        ),
        (7, 7, 7)
    );
    assert_eq!(rig.keys(), Vec::<String>::new());
}

/// Catches a list left with no sidecar (never listed again) or a delete error that stops the
/// pass at once: a list whose delete fails is kept with its sidecar and counted, and the next
/// list is deleted; a sidecar whose delete fails is left with its list gone, and the next pass
/// deletes it; a HEAD that fails keeps its list.
#[tokio::test]
async fn a_store_call_that_fails_keeps_its_list_and_the_pass_goes_on() {
    let rig = Rig::new().await;
    rig.list(1, 100, OLD).await;
    rig.list(2, 100, OLD).await;
    rig.list(3, 100, OLD).await;
    let sweep = rig.sweep(true);
    // The deletes are list 1, then list 2 and its sidecar, then list 3 and its sidecar.
    rig.s3.fail(Failure::Delete(1));
    rig.s3.fail(Failure::Delete(3));
    let report = sweep.sweep_once().await;
    assert_eq!(report.failed, 2);
    assert_eq!((report.deleted_lists, report.deleted_sidecars), (2, 1));
    let mut left = vec![list_key(1), sidecar_key(1), sidecar_key(2)];
    left.sort();
    assert_eq!(rig.keys(), left);

    rig.s3.fail(Failure::Head(1));
    let report = sweep.sweep_once().await;
    assert_eq!(report.failed, 1, "the HEAD of list 1");
    assert_eq!(rig.keys(), [list_key(1), sidecar_key(1)]);
    sweep.sweep_once().await;
    assert_eq!(rig.keys(), Vec::<String>::new());
    assert_eq!(sweep.metrics().get("store_errors"), 3);
    assert_eq!(sweep.metrics().get("passes"), 3);
}

/// Asserts that store calls failing in a row stop the pass at the bound (three here): five
/// HEADs failing stop it after three, counted as an error.
#[tokio::test]
async fn store_calls_failing_in_a_row_stop_the_pass() {
    let rig = Rig::new().await;
    for tag in 1..=5 {
        rig.list(tag, 10, OLD).await;
    }
    for n in 1..=5 {
        rig.s3.fail(Failure::Head(n));
    }
    let sweep = rig.sweep(true);
    let report = sweep.sweep_once().await;
    assert_eq!(report.failed, 3);
    assert_eq!(report.deleted_lists, 0);
    assert_eq!(sweep.metrics().get("pass_errors"), 1);
    assert_eq!(rig.keys().len(), 10);
}

/// Asserts that a key the store refuses (401 or 403) stops the pass at its first call: a list
/// store signed with a key it does not take refuses the HEAD, and a sidecar store that refuses
/// the listing stops the pass before anything; both are counted as errors.
#[tokio::test]
async fn a_refused_key_stops_the_pass_at_its_first_call() {
    let rig = Rig::new().await;
    rig.list(1, 10, OLD).await;
    rig.list(2, 10, OLD).await;
    let other = FakeS3::start().await;
    other.require_credentials("pack-gc", "hunter2");
    let leader = Arc::clone(&rig.leader);
    let stores = Stores {
        lists: other.store(Capabilities::default()),
        sidecars: rig.s3.store(Capabilities::default()),
    };
    let pass = Pass::new(
        stores,
        Arc::clone(&rig.index) as Arc<dyn PackIndex>,
        Arc::new(rig.rows()),
        PassConfig {
            apply: true,
            ..config()
        },
        Box::new(move || leader.load(Ordering::SeqCst)),
    );
    let sweep = rig.sweep_of(pass);
    let mut report = PassReport::default();
    let ended = sweep.pass.run(
        SystemTime::UNIX_EPOCH + Duration::from_secs(NOW),
        &mut report,
    );
    let error = ended.await.expect_err("stopped");
    assert!(matches!(error, PassError::Denied(_)), "{error}");
    assert_eq!(report.failed, 0);

    rig.s3.require_credentials("pack-gc", "hunter2");
    sweep.sweep_once().await;
    let metrics = sweep.metrics();
    assert_eq!(metrics.get("pass_errors"), 1);
    assert_eq!(metrics.get("sidecars_listed"), 0, "the listing was refused");
    assert_eq!(rig.keys().len(), 4);
}

/// Asserts every family the collector renders, by its exact lines after one pass that deleted a
/// list: each counter, the mode, and both gauges by age.
#[tokio::test]
async fn the_metrics_render_every_family() {
    let rig = Rig::new().await;
    rig.list(1, 300, OLD).await;
    let sweep = rig.sweep(true);
    sweep.sweep_once().await;
    let mut text = String::new();
    sweep.metrics().render(&mut text);
    let counters: Vec<String> = text
        .lines()
        .filter(|line| line.starts_with("kbf_tree_list_sweep_") && line.contains("_total "))
        .map(str::to_owned)
        .collect();
    assert_eq!(
        counters,
        [
            "kbf_tree_list_sweep_passes_total 1",
            "kbf_tree_list_sweep_pass_errors_total 0",
            "kbf_tree_list_sweep_abandoned_total 0",
            "kbf_tree_list_sweep_sidecars_listed_total 1",
            "kbf_tree_list_sweep_kept_young_total 0",
            "kbf_tree_list_sweep_kept_named_total 0",
            "kbf_tree_list_sweep_kept_recorded_total 0",
            "kbf_tree_list_sweep_kept_unparsed_total 0",
            "kbf_tree_list_sweep_deleted_lists_total 1",
            "kbf_tree_list_sweep_deleted_list_bytes_total 300",
            "kbf_tree_list_sweep_deleted_sidecars_total 1",
            "kbf_tree_list_sweep_would_delete_lists_total 0",
            "kbf_tree_list_sweep_would_delete_list_bytes_total 0",
            "kbf_tree_list_sweep_would_delete_sidecars_total 0",
            "kbf_tree_list_sweep_store_errors_total 0",
        ]
    );
    assert!(text.contains("# TYPE kbf_tree_list_sweep_passes_total counter\n"));
    assert!(text.contains("kbf_tree_list_sweep_apply 1\n"), "{text}");
    for family in [
        "kbf_tree_list_sweep_lists",
        "kbf_tree_list_sweep_list_bytes",
    ] {
        assert!(text.contains(&format!("# TYPE {family} gauge\n")), "{text}");
        let lines = text
            .lines()
            .filter(|line| line.starts_with(&format!("{family}{{")))
            .count();
        assert_eq!(lines, AGE_DAYS, "{family}");
    }
    assert!(text.contains("kbf_tree_list_sweep_lists{age_days=\"1\"} 1\n"));
    assert!(text.contains("kbf_tree_list_sweep_list_bytes{age_days=\"1\"} 300\n"));
    assert_eq!(sweep.metrics().get("no such family"), 0);
}

/// Asserts that the gauges by age keep the last whole pass's counts: a pass stopped by an error
/// leaves them as they were.
#[tokio::test]
async fn a_pass_stopped_part_way_leaves_the_counts_by_age_as_they_were() {
    let rig = Rig::new().await;
    rig.list(1, 300, NOW).await;
    let sweep = rig.sweep(false);
    sweep.sweep_once().await;
    rig.list(2, 500, NOW).await;
    rig.list(3, 500, OLD).await;
    rig.state.reads_fail.store(true, Ordering::SeqCst);
    sweep.sweep_once().await;
    let mut text = String::new();
    sweep.metrics().render(&mut text);
    assert!(
        text.contains("kbf_tree_list_sweep_lists{age_days=\"0\"} 1\n"),
        "{text}"
    );
    assert!(
        text.contains("kbf_tree_list_sweep_list_bytes{age_days=\"0\"} 300\n"),
        "{text}"
    );
}

/// Asserts what "in a row" means: a call that succeeds between failures starts the count again
/// (two HEADs fail, one succeeds, two more fail: the pass goes on under a bound of three), and
/// deletes that fail count as HEADs do: three in a row stop the pass, counted as an error, and
/// the lists stay.
#[tokio::test]
async fn a_call_that_succeeds_starts_the_count_of_failures_again() {
    let rig = Rig::new().await;
    for tag in 1..=5 {
        rig.list(tag, 10, OLD).await;
    }
    for n in [1, 2, 4, 5] {
        rig.s3.fail(Failure::Head(n));
    }
    let sweep = rig.sweep(true);
    let report = sweep.sweep_once().await;
    assert!(report.whole, "not stopped");
    assert_eq!((report.failed, report.deleted_lists), (4, 1));
    assert_eq!(sweep.metrics().get("pass_errors"), 0);

    for n in 1..=3 {
        rig.s3.fail(Failure::Delete(n));
    }
    // One page, so the four HEADs come first and the deletes follow one another.
    let sweep = rig.sweep_with(PassConfig {
        apply: true,
        page: PageSize::MAX,
        ..config()
    });
    let report = sweep.sweep_once().await;
    assert!(!report.whole);
    assert_eq!((report.failed, report.deleted_lists), (3, 0));
    assert_eq!(sweep.metrics().get("pass_errors"), 1);
    assert_eq!(rig.keys().len(), 8);
}

/// Asserts that leadership lost between two deletes of one page stops the second: one list is
/// deleted, and the pass is abandoned.
#[tokio::test]
async fn leadership_lost_between_two_deletes_stops_the_second() {
    let rig = Rig::new().await;
    rig.list(1, 100, OLD).await;
    rig.list(2, 100, OLD).await;
    let leader = Arc::clone(&rig.leader);
    let asked = Arc::new(std::sync::atomic::AtomicU32::new(0));
    // Asked before the page, then before each delete: the third ask is lost.
    let pass = Pass {
        is_leader: Box::new(move || {
            asked.fetch_add(1, Ordering::SeqCst) < 2 && leader.load(Ordering::SeqCst)
        }),
        ..rig.pass_over(
            PassConfig {
                apply: true,
                ..config()
            },
            Arc::clone(&rig.index) as Arc<dyn PackIndex>,
        )
    };
    let report = rig.sweep_of(pass).sweep_once().await;
    assert!(report.abandoned);
    assert_eq!(report.deleted_lists, 1);
    assert_eq!(rig.keys(), [list_key(2), sidecar_key(2)]);
}
