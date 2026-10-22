//! The producer on one cache (`StreamedPut::commit` with `CommitMode::Held`, through
//! `CacheStorage` as the storage service calls it): what is held, what is written through
//! and counted for which reason, a late duplicate of a blob already on disk, and what the
//! flush report says while blobs are held. Every wait is an event or none is needed.

mod common;
#[path = "support/rig.rs"]
#[allow(
    dead_code,
    reason = "shared with the write-pool tests, which use the rest of it"
)]
mod rig;
#[path = "write_tier/support.rs"]
#[allow(
    dead_code,
    reason = "shared with write_tier.rs, which uses the rest of it"
)]
mod support;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use kbf_front::{Cache, CacheStorage, SegmentsConfig};
use kbf_objstore::KeyPrefix;
use kbf_storage::flush::FlushGate;
use kbf_storage::{CommitMode, Storage, StorageError, StreamedPut, Through};
use kbf_tier::TierOptions;
use kbf_types::Digest;
use rig::{Rig, pool};
use support::{SEGMENT, SERVER, TestBlob, TierCache};
use tokio::time::Instant;

const GIB: u64 = 1 << 30;
const KIB4: usize = 4096;

type Store = CacheStorage<support::Log, common::faulty::FaultyStore>;

/// A cache over the rig's log and store with a tier, a one-writer pool with `queue` places,
/// the RAM budget `budget`, and (if given) `gate`.
async fn cache(
    rig: &Rig,
    budget: u64,
    queue: usize,
    gate: Option<Arc<dyn FlushGate>>,
) -> Arc<TierCache> {
    let mut cache = Cache::open(rig.log.clone(), rig.store.clone(), KeyPrefix::default())
        .await
        .expect("open")
        .with_segment_limit(SEGMENT)
        .with_ram_ack_bytes(budget);
    if let Some(gate) = gate {
        cache = cache.with_flush_gate(gate);
    }
    let (cache, _) = cache
        .with_write_pool(pool(1, queue))
        .attach_tier(rig.fs.clone(), TierOptions::new(GIB), SERVER, None)
        .await
        .expect("attach");
    Arc::new(cache)
}

fn storage(cache: &Arc<TierCache>) -> Store {
    CacheStorage::new(Arc::clone(cache), SegmentsConfig::DEFAULT)
}

/// `blob` streamed into `storage` and ended as its digest, ready to commit.
async fn streamed(storage: &Arc<Store>, blob: &TestBlob) -> Box<dyn StreamedPut> {
    let (storage, bytes, digest) = (Arc::clone(storage), blob.bytes.clone(), blob.digest);
    tokio::task::spawn_blocking(move || {
        let mut put = storage.stream(bytes.len() as u64).expect("a stream");
        for piece in bytes.chunks(64 << 10) {
            put.append(piece).expect("appended");
        }
        put.finish(digest, kbf_segments::crc32c(&bytes))
            .expect("finished")
    })
    .await
    .expect("join")
}

/// Streams `blob` into `storage` and commits it as `mode`, as the storage service does.
async fn commit(
    storage: &Arc<Store>,
    blob: &TestBlob,
    mode: CommitMode,
) -> Result<(), StorageError> {
    streamed(storage, blob).await.commit(mode).await
}

fn metric(cache: &TierCache, name: &str) -> f64 {
    let text = cache.render_metrics();
    let key = format!("{name} ");
    text.lines()
        .find_map(|l| l.strip_prefix(&key))
        .unwrap_or_else(|| panic!("no sample {name}"))
        .trim()
        .parse()
        .expect("a number")
}

fn through(reason: &str) -> String {
    format!("kbf_storage_ram_writethrough_total{{reason=\"{reason}\"}}")
}

/// Gap (b) of the earlier verifiers: a late duplicate of a blob that is already in the
/// index is not held again. The blob is held, drained and indexed; a second write of it,
/// committed `Held`, is answered, takes no entry and no budget, and queues nothing for the
/// drain (the pool is not asked to write it a second time). Mutant: `hold_write` without
/// the index look (`hold` alone): the duplicate is held, `unwritten().blobs` is 1.
#[tokio::test(flavor = "multi_thread")]
async fn a_late_duplicate_of_a_blob_on_disk_is_not_held_again() {
    let rig = Rig::new();
    let cache = cache(&rig, 64 << 20, 16, None).await;
    let store = Arc::new(storage(&cache));
    let blob = TestBlob::new(1, KIB4);
    commit(&store, &blob, CommitMode::Held).await.expect("held");
    assert!(cache.is_held_for_test(&blob.digest));
    let report = cache.drain_held(Instant::now()).await;
    assert_eq!(report.written, 1);
    assert!(!cache.is_held_for_test(&blob.digest));
    assert!(rig.on_tier(&blob.digest));
    let syncs = rig.fs.syncs();

    commit(&store, &blob, CommitMode::Held)
        .await
        .expect("answered");
    assert!(!cache.is_held_for_test(&blob.digest), "not queued again");
    assert_eq!(cache.unwritten().blobs, 0);
    assert_eq!(metric(&cache, "kbf_storage_ram_unwritten_bytes"), 0.0);
    let report = cache.drain_held(Instant::now()).await;
    assert_eq!(report.claimed, 0, "the drain has nothing to write");
    assert_eq!(rig.fs.syncs(), syncs, "and wrote nothing");
}

/// T10. Two writes of one digest that are both committed before the drain run: the second is
/// answered without a second entry or a second permit. With room for one blob, the second
/// write is not counted as a full budget; the drain writes the blob once and gives the
/// budget back, so a different blob is then held. Mutant: `hold_write` skips the
/// held-set look and the insert overwrites (a second permit is asked: `writethrough{budget}`
/// is counted).
#[tokio::test(flavor = "multi_thread")]
async fn two_writes_of_one_digest_hold_it_once() {
    let rig = Rig::new();
    let cache = cache(&rig, KIB4 as u64, 16, None).await;
    let store = Arc::new(storage(&cache));
    let (a, b) = (TestBlob::new(2, KIB4), TestBlob::new(3, KIB4));
    commit(&store, &a, CommitMode::Held).await.expect("held");
    commit(&store, &a, CommitMode::Held)
        .await
        .expect("answered");
    assert_eq!(cache.unwritten().blobs, 1);
    assert_eq!(
        metric(&cache, "kbf_storage_ram_unwritten_bytes"),
        KIB4 as f64
    );
    assert_eq!(metric(&cache, &through("budget")), 0.0);
    let report = cache.drain_held(Instant::now()).await;
    assert_eq!((report.claimed, report.written), (1, 1));
    commit(&store, &b, CommitMode::Held).await.expect("held");
    assert!(cache.is_held_for_test(&b.digest), "the budget came back");
}

/// Each reason a write is committed to disk is counted once, in its row, and in no other
/// row (the others read 0): a blob over 4 MiB (`large`), a blob the budget has no room for
/// (`budget`, with the blob written and indexed), one with no replica (`single_owner`) and
/// one whose replica was lost (`replica_lost`). Catches a reason counted under another
/// label or not at all. Mutant: `WriteThrough::Large` and `StorePath` swapped in
/// `commit_held`.
#[tokio::test(flavor = "multi_thread")]
async fn each_write_through_is_counted_for_its_own_reason() {
    let rig = Rig::new();
    let cache = cache(&rig, KIB4 as u64, 16, None).await;
    let store = Arc::new(storage(&cache));
    let rows = [
        "replica_lost",
        "single_owner",
        "large",
        "queue_full",
        "budget",
        "store_path",
    ];
    let counts = |cache: &TierCache| rows.map(|r| metric(cache, &through(r)));

    let big = TestBlob::new(4, (4 << 20) + 1);
    commit(&store, &big, CommitMode::Held)
        .await
        .expect("written");
    assert_eq!(counts(&cache), [0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
    assert!(
        rig.on_tier(&big.digest)
            || !matches!(rig.log.answer(&big.digest), kbf_meta::BlobAnswer::Absent)
    );
    assert!(!cache.is_held_for_test(&big.digest));

    let (first, second) = (TestBlob::new(5, KIB4), TestBlob::new(6, KIB4));
    commit(&store, &first, CommitMode::Held)
        .await
        .expect("held");
    commit(&store, &second, CommitMode::Held)
        .await
        .expect("written");
    assert_eq!(counts(&cache), [0.0, 0.0, 1.0, 0.0, 1.0, 0.0]);
    assert!(!cache.is_held_for_test(&second.digest));
    assert!(rig.on_tier(&second.digest), "committed to the disk");

    let (one, lost) = (TestBlob::new(7, KIB4), TestBlob::new(8, KIB4));
    commit(&store, &one, CommitMode::Through(Through::SingleOwner))
        .await
        .expect("written");
    commit(&store, &lost, CommitMode::Through(Through::ReplicaLost))
        .await
        .expect("written");
    assert_eq!(counts(&cache), [1.0, 1.0, 1.0, 0.0, 1.0, 0.0]);
    assert!(rig.on_tier(&one.digest) && rig.on_tier(&lost.digest));
}

/// A blob that goes straight to the store is counted for why: with no tier it is
/// `store_path`; with the pool's one place taken behind a writer parked in `sync` it is
/// `queue_full`. The second write is queued by polling its commit once (the pool takes the
/// job before the first await), so the third finds the queue full without a race. Catches
/// the two counted as one, and either not counted. Mutant: `store_reason` always
/// `StorePath`.
#[tokio::test(flavor = "multi_thread")]
async fn a_blob_that_goes_straight_to_the_store_is_counted_for_why() {
    let rig = Rig::new();
    let cache = cache(&rig, 64 << 20, 1, None).await;
    let store = Arc::new(storage(&cache));
    rig.fs.pause();
    let (a, b, c) = (
        TestBlob::new(9, KIB4),
        TestBlob::new(10, KIB4),
        TestBlob::new(11, KIB4),
    );
    // The first write is taken by the writer, which parks in `sync`.
    let first = tokio::spawn(
        streamed(&store, &a)
            .await
            .commit(CommitMode::Through(Through::SingleOwner)),
    );
    let fs = rig.fs.clone();
    tokio::task::spawn_blocking(move || fs.wait_waiting(1))
        .await
        .expect("join");
    // The second takes the pool's one place: polled once, it has sent its job.
    let mut second = streamed(&store, &b)
        .await
        .commit(CommitMode::Through(Through::SingleOwner));
    assert!(futures::poll!(&mut second).is_pending());
    assert_eq!(metric(&cache, &through("queue_full")), 0.0);

    commit(&store, &c, CommitMode::Held).await.expect("written");
    assert_eq!(metric(&cache, &through("queue_full")), 1.0);
    assert_eq!(metric(&cache, &through("store_path")), 0.0);
    assert!(!cache.is_held_for_test(&c.digest), "written, not held");
    rig.fs.resume();
    first.await.expect("join").expect("written");
    second.await.expect("written");

    // No tier at all: the blob streams to the store.
    let bare = Arc::new(
        Cache::open(rig.log.clone(), rig.store.clone(), KeyPrefix::default())
            .await
            .expect("open")
            .with_ram_ack_bytes(64 << 20),
    );
    let bare_store = Arc::new(storage(&bare));
    let d = TestBlob::new(15, KIB4);
    commit(&bare_store, &d, CommitMode::Held)
        .await
        .expect("written");
    assert_eq!(metric(&bare, &through("store_path")), 1.0);
    assert_eq!(metric(&bare, &through("queue_full")), 0.0);

    // Over 4 MiB the reason is the size, whichever way the blob is written.
    let big = TestBlob::new(17, (4 << 20) + 1);
    commit(&bare_store, &big, CommitMode::Held)
        .await
        .expect("written");
    assert_eq!(metric(&bare, &through("large")), 1.0);
    assert_eq!(metric(&bare, &through("store_path")), 1.0);
}

/// Gap (c) of PR 5. A Draining member does not count as filled while it holds entries: the
/// flush report's clear generation is held at the generation the oldest held blob was
/// acknowledged under, as it is for a staged object, so the handoff does not read a node
/// that holds acknowledged blobs as flushed through the new map. Held at map 5 and the map
/// moved to 7, the report says 5; once the blob is drained and flushed it says 7. Catches
/// a report that ignores the set. Mutant: `flush_report` without the `oldest_born` minimum.
#[tokio::test(flavor = "multi_thread")]
async fn a_node_holding_blobs_does_not_report_itself_flushed_through_the_newer_map() {
    #[derive(Debug)]
    struct Gate(AtomicU64);
    impl FlushGate for Gate {
        fn flushes(&self, _: &Digest) -> bool {
            true
        }
        fn generation(&self) -> Option<u64> {
            Some(self.0.load(Ordering::SeqCst))
        }
    }
    let gate = Arc::new(Gate(AtomicU64::new(5)));
    let rig = Rig::new();
    let cache = cache(&rig, 64 << 20, 16, Some(Arc::clone(&gate) as _)).await;
    let store = Arc::new(storage(&cache));
    let blob = TestBlob::new(12, KIB4);
    commit(&store, &blob, CommitMode::Held).await.expect("held");
    gate.0.store(7, Ordering::SeqCst);
    assert_eq!(
        cache.flush_report().await.expect("report").clear_gen,
        Some(5)
    );

    let report = cache.drain_held(Instant::now()).await;
    assert_eq!(report.written, 1);
    // The drain staged it under map 7 and dropped the entry, but it was acknowledged under
    // map 5: the staged object keeps that date until it is flushed.
    let report = cache.flush_report().await.expect("report");
    assert_eq!((report.clear_gen, report.staged_objects), (Some(5), 1));
    cache.flush_tier(support::later()).await.expect("round");
    assert_eq!(
        cache.flush_report().await.expect("report").clear_gen,
        Some(7)
    );
}

/// A drain between the report's two looks (the set, then the staged objects) does not lose
/// the cap. The report reads the set first; the hook between the looks drains the blob
/// (staged, entry gone) and the query then finds the staged object, dated by the
/// acknowledgement: 5. Read the other way round (the query, the hook's drain, then the set),
/// the object is not in the query and the set is empty, and the report says 7. Mutant:
/// `flush_report` reads `oldest_born` after the query.
#[tokio::test(flavor = "multi_thread")]
async fn a_drain_between_the_flush_reports_looks_does_not_lose_the_cap() {
    #[derive(Debug)]
    struct Gate(AtomicU64);
    impl FlushGate for Gate {
        fn flushes(&self, _: &Digest) -> bool {
            true
        }
        fn generation(&self) -> Option<u64> {
            Some(self.0.load(Ordering::SeqCst))
        }
    }
    let gate = Arc::new(Gate(AtomicU64::new(5)));
    let rig = Rig::new();
    let cell: Arc<std::sync::OnceLock<Arc<TierCache>>> = Arc::default();
    let hook: kbf_front::BetweenHook = {
        let cell = Arc::clone(&cell);
        Arc::new(move || {
            let cell = Arc::clone(&cell);
            Box::pin(async move {
                if let Some(cache) = cell.get() {
                    cache.drain_held(Instant::now()).await;
                }
            })
        })
    };
    let (opened, _) = Cache::open(rig.log.clone(), rig.store.clone(), KeyPrefix::default())
        .await
        .expect("open")
        .with_segment_limit(SEGMENT)
        .with_ram_ack_bytes(64 << 20)
        .with_flush_gate(Arc::clone(&gate) as _)
        .with_between_hook(hook)
        .with_write_pool(pool(1, 16))
        .attach_tier(rig.fs.clone(), TierOptions::new(GIB), SERVER, None)
        .await
        .expect("attach");
    let cache = Arc::new(opened);
    cell.set(Arc::clone(&cache)).ok();
    let store = Arc::new(storage(&cache));
    let blob = TestBlob::new(16, KIB4);
    commit(&store, &blob, CommitMode::Held).await.expect("held");
    gate.0.store(7, Ordering::SeqCst);

    let report = cache.flush_report().await.expect("report");
    assert!(!cache.is_held_for_test(&blob.digest), "the hook drained it");
    assert_eq!((report.clear_gen, report.staged_objects), (Some(5), 1));
}

/// A node with the producer off (budget 0, the default) holds nothing whatever the storage
/// layer asks: `Held` is committed to disk, uncounted, and `Through` rows read 0.
/// Catches the default changing a write. Mutant: `ram_ack_enabled` true at 0.
#[tokio::test(flavor = "multi_thread")]
async fn at_a_zero_budget_held_is_a_plain_commit_and_nothing_is_counted() {
    let rig = Rig::new();
    let cache = cache(&rig, 0, 16, None).await;
    let store = Arc::new(storage(&cache));
    let (a, b) = (TestBlob::new(13, KIB4), TestBlob::new(14, KIB4));
    commit(&store, &a, CommitMode::Held).await.expect("written");
    commit(&store, &b, CommitMode::Through(Through::ReplicaLost))
        .await
        .expect("written");
    assert!(!cache.is_held_for_test(&a.digest));
    assert!(rig.on_tier(&a.digest) && rig.on_tier(&b.digest));
    for row in [
        "replica_lost",
        "single_owner",
        "large",
        "queue_full",
        "budget",
        "store_path",
    ] {
        assert_eq!(metric(&cache, &through(row)), 0.0, "{row}");
    }
    assert!(!store.ram_ack());
}
