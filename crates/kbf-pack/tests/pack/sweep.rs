//! The sweep of orphans under `cas/b/`, over the fault-injecting store and index of
//! `support`. Ages are read from the keys, `now` is passed in, and nothing sleeps.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

use kbf_objstore::PageSize;
use kbf_pack::{SweepConfig, SweepGates, Sweeper};
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_pack_index::{
    BlobDigest, Location, NewPack, PackBlob, PackIndex, PackIndexError, PackInfo, Recorded,
};
use kbf_state::ManualClock;

use crate::support::{FaultyStore, Rig};

/// The minute the sweeps of these tests run at.
const NOW_MINUTE: u64 = 100_000;
/// An age past the grace of six hours, in minutes: 400 minutes.
const OLD: u64 = NOW_MINUTE - 400;
/// An age inside the grace: 100 minutes.
const YOUNG: u64 = NOW_MINUTE - 100;

fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(NOW_MINUTE * 60)
}

/// The key of a pack minted at `minute`; the minute is the part of the name before `-0`.
fn key(minute: u64) -> String {
    format!("cas/b/lease1/{minute}-0")
}

/// The age a key says: its minute, or `None` for a name that has none.
fn age_of(key: &str) -> Option<SystemTime> {
    let name = key.rsplit('/').next()?;
    let minute: u64 = name.strip_suffix("-0")?.parse().ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(minute * 60))
}

fn gates(leader: bool, verifying: Option<String>) -> SweepGates {
    SweepGates {
        age_of: Box::new(age_of),
        is_leader: Box::new(move || leader),
        verifying: Box::new(move |k| verifying.as_deref() == Some(k)),
    }
}

fn sweeper(rig: &Rig, gates: SweepGates) -> Sweeper<FaultyStore> {
    let index: Arc<dyn PackIndex> = rig.index.clone();
    Sweeper::new(Arc::clone(&rig.store), index, SweepConfig::default(), gates)
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

/// Asserts that an orphan older than the grace is deleted and counted.
#[tokio::test]
async fn an_orphan_older_than_grace_is_deleted() {
    let rig = Rig::new();
    rig.store.seed(&key(OLD), b"orphan").await;
    let report = sweeper(&rig, gates(true, None)).sweep(now()).await.unwrap();
    assert!(!rig.store.holds(&key(OLD)).await);
    assert!(report.led);
    assert_eq!(report.listed, 1);
    assert_eq!(report.deleted, 1);
}

/// Asserts that an orphan younger than the grace is kept and counted as young.
#[tokio::test]
async fn a_young_orphan_is_kept() {
    let rig = Rig::new();
    rig.store.seed(&key(YOUNG), b"orphan").await;
    let report = sweeper(&rig, gates(true, None)).sweep(now()).await.unwrap();
    assert!(rig.store.holds(&key(YOUNG)).await);
    assert_eq!(report.young, 1);
    assert_eq!(report.deleted, 0);
}

/// Asserts that a pack the index records is kept however old it is.
#[tokio::test]
async fn an_indexed_pack_is_kept() {
    let rig = Rig::new();
    rig.store.seed(&key(OLD), b"indexed").await;
    rig.index.record_pack(&recorded(&key(OLD))).unwrap();
    let report = sweeper(&rig, gates(true, None)).sweep(now()).await.unwrap();
    assert!(rig.store.holds(&key(OLD)).await);
    assert_eq!(report.referenced, 1);
    assert_eq!(report.deleted, 0);
}

/// Asserts that an old orphan under verification is kept and counted, and is deleted by the
/// next sweep once the verification is over.
#[tokio::test]
async fn a_key_under_verification_is_kept() {
    let rig = Rig::new();
    rig.store.seed(&key(OLD), b"orphan").await;
    let held = gates(true, Some(key(OLD)));
    let report = sweeper(&rig, held).sweep(now()).await.unwrap();
    assert!(rig.store.holds(&key(OLD)).await);
    assert_eq!(report.verifying, 1);
    assert_eq!(report.deleted, 0);
    sweeper(&rig, gates(true, None)).sweep(now()).await.unwrap();
    assert!(!rig.store.holds(&key(OLD)).await);
}

/// Asserts that a process that does not lead lists nothing and deletes nothing.
#[tokio::test]
async fn a_non_leader_does_nothing() {
    let rig = Rig::new();
    rig.store.seed(&key(OLD), b"orphan").await;
    rig.store.fail_list_after(0);
    let report = sweeper(&rig, gates(false, None))
        .sweep(now())
        .await
        .unwrap();
    assert!(rig.store.holds(&key(OLD)).await);
    assert!(!report.led);
    assert_eq!(report.listed, 0);
    assert_eq!(report.deleted, 0);
}

/// Asserts that a delete the store refuses is counted and does not stop the sweep: the next
/// key is deleted, and the next sweep deletes the one that failed.
#[tokio::test]
async fn a_failed_delete_is_counted_and_retried_next_sweep() {
    let rig = Rig::new();
    rig.store.seed(&key(OLD), b"first").await;
    rig.store.seed(&key(OLD + 1), b"second").await;
    rig.store.fail_deletes(1);
    let sweep = sweeper(&rig, gates(true, None));
    let report = sweep.sweep(now()).await.unwrap();
    assert_eq!(report.delete_failed, 1);
    assert_eq!(report.deleted, 1);
    let left = rig.store.keys_under("cas/b/").await;
    assert_eq!(left, vec![key(OLD)]);
    let again = sweep.sweep(now()).await.unwrap();
    assert_eq!(again.deleted, 1);
    assert!(rig.store.keys_under("cas/b/").await.is_empty());
}

/// Asserts that only keys under `cas/b/` are swept: an old key under `cas/d/` or `cas/f/`
/// stays.
#[tokio::test]
async fn only_the_cas_b_prefix_is_swept() {
    let rig = Rig::new();
    rig.store
        .seed(&format!("cas/d/n/b/{OLD}-0"), b"daemon")
        .await;
    rig.store
        .seed(&format!("cas/f/n/b/{OLD}-0"), b"frontend")
        .await;
    rig.store.seed(&key(OLD), b"orphan").await;
    let report = sweeper(&rig, gates(true, None)).sweep(now()).await.unwrap();
    assert_eq!(report.listed, 1);
    assert_eq!(report.deleted, 1);
    assert_eq!(rig.store.keys_under("cas/").await.len(), 2);
}

/// Asserts that a key whose age the key does not say is reported and kept.
#[tokio::test]
async fn a_stray_key_is_reported_not_deleted() {
    let rig = Rig::new();
    rig.store.seed("cas/b/lease1/garbage", b"stray").await;
    let report = sweeper(&rig, gates(true, None)).sweep(now()).await.unwrap();
    assert!(rig.store.holds("cas/b/lease1/garbage").await);
    assert_eq!(report.strays, vec!["cas/b/lease1/garbage".to_owned()]);
    assert_eq!(report.deleted, 0);
}

/// Asserts that a listing of several pages (a page size of two over five keys) is swept to
/// its end.
#[tokio::test]
async fn the_sweep_pages_through_a_long_listing() {
    let rig = Rig::new();
    for n in 0..5 {
        rig.store.seed(&key(OLD + n), b"orphan").await;
    }
    let index: Arc<dyn PackIndex> = rig.index.clone();
    let config = SweepConfig {
        page: PageSize::new(2).unwrap(),
        ..SweepConfig::default()
    };
    let sweep = Sweeper::new(Arc::clone(&rig.store), index, config, gates(true, None));
    let report = sweep.sweep(now()).await.unwrap();
    assert_eq!(report.listed, 5);
    assert_eq!(report.deleted, 5);
    assert!(rig.store.keys_under("cas/b/").await.is_empty());
}

/// Asserts that a dry run counts what it would delete and deletes nothing.
#[tokio::test]
async fn a_dry_run_deletes_nothing() {
    let rig = Rig::new();
    rig.store.seed(&key(OLD), b"orphan").await;
    let index: Arc<dyn PackIndex> = rig.index.clone();
    let config = SweepConfig {
        dry_run: true,
        ..SweepConfig::default()
    };
    let sweep = Sweeper::new(Arc::clone(&rig.store), index, config, gates(true, None));
    let report = sweep.sweep(now()).await.unwrap();
    assert!(rig.store.holds(&key(OLD)).await);
    assert_eq!(report.would_delete, 1);
    assert_eq!(report.deleted, 0);
}

/// An index that records a pack under `key` when it is asked the second time which keys are
/// unreferenced: the pack is indexed between the sweep's page question and its delete.
struct Flips {
    inner: MemoryPackIndex,
    asked: AtomicU32,
    key: String,
}

impl PackIndex for Flips {
    fn record_pack(&self, pack: &NewPack) -> Result<Recorded, PackIndexError> {
        self.inner.record_pack(pack)
    }
    fn pack(&self, key: &str) -> Result<Option<PackInfo>, PackIndexError> {
        self.inner.pack(key)
    }
    fn locate(&self, digests: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        self.inner.locate(digests)
    }
    fn find_missing(&self, digests: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        self.inner.find_missing(digests)
    }
    fn touch(&self, digests: &[BlobDigest]) -> Result<usize, PackIndexError> {
        self.inner.touch(digests)
    }
    fn dead_packs(&self, grace: Duration, limit: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        self.inner.dead_packs(grace, limit)
    }
    fn unreferenced_keys(&self, keys: &[String]) -> Result<Vec<String>, PackIndexError> {
        if self.asked.fetch_add(1, Ordering::SeqCst) == 1 {
            self.inner.record_pack(&recorded(&self.key))?;
        }
        self.inner.unreferenced_keys(keys)
    }
}

/// Asserts that a pack indexed after the page was judged unreferenced and before its delete
/// is kept: the index is asked again for the one key just before the delete.
#[tokio::test]
async fn a_pack_indexed_between_list_and_delete_is_kept() {
    let rig = Rig::new();
    rig.store.seed(&key(OLD), b"late").await;
    let index = Arc::new(Flips {
        inner: MemoryPackIndex::new(Arc::new(ManualClock::default())),
        asked: AtomicU32::new(0),
        key: key(OLD),
    });
    let sweep = Sweeper::new(
        Arc::clone(&rig.store),
        index,
        SweepConfig::default(),
        gates(true, None),
    );
    let report = sweep.sweep(now()).await.unwrap();
    assert!(rig.store.holds(&key(OLD)).await);
    assert_eq!(report.referenced, 1);
    assert_eq!(report.deleted, 0);
}
