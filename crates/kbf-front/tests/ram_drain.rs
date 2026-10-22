//! The drain of the held set (`docs/design/storage-ram-ack.md`, "Drain and shutdown"): a
//! held blob is written to the hot disk through the write pool and leaves the set only once
//! the index has it; a failed attempt keeps it and is tried again; a shutdown drain is
//! bounded by its deadline. Nothing holds a blob in a running node yet, so the blobs here
//! are put in through `Cache::hold` (the producer's entry) or `Cache::hold_for_test`.
//!
//! Waits are events (the pool's drain hook, a parked store PUT, `durable_waiting`'s
//! notification) or the paused clock; a test that needs the time to pass runs on a
//! current-thread runtime with `start_paused`, where the clock moves only when every task
//! is idle. None sleeps or asserts how long anything took.

mod common;
#[path = "support/rig.rs"]
#[allow(
    dead_code,
    reason = "shared with write_pool.rs, which uses the rest of it"
)]
mod rig;
#[path = "write_tier/support.rs"]
#[allow(
    dead_code,
    reason = "shared with write_tier.rs, which uses the rest of it"
)]
mod support;

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::faulty::PutFault;
use kbf_front::{Cache, DrainHook, DrainState, Hold, PASS_BLOBS, ShutdownDrain};
use kbf_meta::BlobAnswer;
use kbf_objstore::KeyPrefix;
use kbf_tier::TierOptions;
use rig::{Rig, pool, pooled};
use support::{SEGMENT, SERVER, TestBlob, TierCache};
use tokio::time::Instant;

const GIB: u64 = 1 << 30;

/// Awaits `event`, failing the test instead of hanging if it never comes. The bound only
/// ends a hang; no test asserts on how long an event takes.
async fn within<T>(what: &str, event: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(60), event)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// The value of the sample `name` in a scrape.
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

const FAILURES: &str = "kbf_storage_ram_drain_failures_total";
const BYTES: &str = "kbf_storage_ram_unwritten_bytes";
const BLOBS: &str = "kbf_storage_ram_unwritten_blobs";

/// A cache over the rig's log and store with no tier: a drain writes to the store.
async fn untiered(rig: &Rig, budget: u64) -> Arc<TierCache> {
    let cache = Cache::open(rig.log.clone(), rig.store.clone(), KeyPrefix::default())
        .await
        .expect("open")
        .with_ram_ack_bytes(budget);
    Arc::new(cache)
}

/// A cache over the rig with the tier attached and a one-writer pool, and `hook` (if any)
/// called at the pool's drain step.
async fn tiered(rig: &Rig, budget: u64, hook: Option<DrainHook>) -> Arc<TierCache> {
    let mut cache = Cache::open(rig.log.clone(), rig.store.clone(), KeyPrefix::default())
        .await
        .expect("open")
        .with_segment_limit(SEGMENT)
        .with_ram_ack_bytes(budget);
    if let Some(hook) = hook {
        cache = cache.with_drain_hook(hook);
    }
    let (cache, _) = pooled(cache, pool(1, 16))
        .attach_tier(rig.fs.clone(), TierOptions::new(GIB), SERVER, None)
        .await
        .expect("attach");
    Arc::new(cache)
}

/// The blob is present in the index (staged on the tier or in the store).
fn indexed(rig: &Rig, blob: &TestBlob) -> bool {
    !matches!(rig.log.answer(&blob.digest), BlobAnswer::Absent)
}

/// The drain writes to the hot disk, and the entry leaves only after the index has the
/// blob. Parked at the pool's drain step (after `PutStaged` applied, before the writer
/// answers), the index has the blob, the entry is still held, its bytes still count in
/// `unwritten_bytes` and its budget is still taken (a second blob of the same size is
/// refused); released, the entry and its budget go. Catches an entry removed before the
/// index has the blob (mutant: the pass removes the entry before it awaits the write: red
/// at the first assertion in the hook), an entry that is never removed, and a budget that
/// is returned early.
#[tokio::test(flavor = "multi_thread")]
async fn the_drain_writes_to_the_hot_disk_and_releases_after_the_index_has_it() {
    let rig = Rig::new();
    let (parked_tx, mut parked) = tokio::sync::mpsc::unbounded_channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    let hook: DrainHook = Arc::new(move |digests| {
        let _ = parked_tx.send(digests.to_vec());
        let _ = go_rx.lock().expect("lock").recv();
    });
    let cache = tiered(&rig, 4096, Some(hook)).await;
    let (a, b) = (TestBlob::new(1, 4096), TestBlob::new(2, 4096));
    assert_eq!(cache.hold(&a.verified()), Hold::Held);
    assert!(!indexed(&rig, &a), "held, not yet in the index");

    let draining = tokio::spawn({
        let cache = Arc::clone(&cache);
        async move { cache.drain_held(Instant::now()).await }
    });
    let digests = within("the drain step", parked.recv())
        .await
        .expect("the hook ran");
    assert_eq!(digests, vec![a.digest]);
    assert!(rig.on_tier(&a.digest), "the index has it at the drain step");
    assert!(
        cache.is_held_for_test(&a.digest),
        "and the entry is still held: removed only after the index has it"
    );
    assert_eq!(metric(&cache, BYTES), 4096.0, "still counted as unwritten");
    assert_eq!(
        cache.hold(&b.verified()),
        Hold::Budget,
        "its budget is taken"
    );

    go_tx.send(()).expect("go");
    let report = draining.await.expect("join");
    assert_eq!((report.claimed, report.written, report.failed), (1, 1, 0));
    assert!(
        !cache.is_held_for_test(&a.digest),
        "released after the write"
    );
    assert_eq!(metric(&cache, BYTES), 0.0);
    assert_eq!(metric(&cache, BLOBS), 0.0);
    assert_eq!(
        cache.hold(&b.verified()),
        Hold::Held,
        "and its budget is back"
    );
    assert!(
        cache
            .find_missing(&[a.digest])
            .await
            .expect("found")
            .is_empty(),
        "present from the index now"
    );
}

/// A failed drain keeps the entry and tries again. The first `PutStaged` is refused: the
/// pass reports one failure, the entry stays held as `Failing{n: 1}` (still readable, still
/// counted as unwritten), `drain_failures_total` is 1; a pass before the backoff ends
/// takes nothing; a pass at the time the first one said (`retry_at`) writes it, and the
/// failure count stays 1. Catches a failure that drops the entry (mutant: the failed branch
/// removes it: red at "stays held"), a retry that ignores the backoff (mutant: `claim`
/// takes a failed blob at any time: red at the empty pass), a failed entry that is never
/// taken again (mutant: `failed` leaves it `Writing`: red at the retry), and an
/// uncounted failure.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_drain_keeps_the_entry_and_retries_after_the_backoff() {
    let rig = Rig::new();
    let cache = tiered(&rig, 1 << 20, None).await;
    let blob = TestBlob::new(3, 4096);
    assert_eq!(cache.hold(&blob.verified()), Hold::Held);
    rig.log.refuse("put_staged", 1);

    let now = Instant::now();
    let report = cache.drain_held(now).await;
    assert_eq!((report.claimed, report.written, report.failed), (1, 0, 1));
    assert!(
        cache.is_held_for_test(&blob.digest),
        "an un-durable entry is never dropped"
    );
    assert!(
        matches!(
            cache.drain_state_for_test(&blob.digest),
            Some(DrainState::Failing { n: 1, .. })
        ),
        "{:?}",
        cache.drain_state_for_test(&blob.digest)
    );
    assert_eq!(metric(&cache, FAILURES), 1.0);
    assert_eq!(metric(&cache, BYTES), 4096.0, "still unwritten");
    assert!(
        cache
            .find_missing(&[blob.digest])
            .await
            .expect("found")
            .is_empty(),
        "and still served from RAM"
    );
    let due = report.retry_at.expect("a retry is waiting");
    assert!(due > now, "after a backoff");

    let early = cache.drain_held(now).await;
    assert_eq!(early.claimed, 0, "the backoff has not ended");
    let retry = cache.drain_held(due).await;
    assert_eq!((retry.claimed, retry.written, retry.failed), (1, 1, 0));
    assert!(!cache.is_held_for_test(&blob.digest));
    assert!(rig.on_tier(&blob.digest));
    assert_eq!(metric(&cache, FAILURES), 1.0, "the retry did not fail");
    assert_eq!(retry.retry_at, None);
}

/// Failures in a row lengthen the wait: the second failure of one blob is `Failing{n: 2}`
/// and due later than the first one was. Catches a backoff that restarts at the first step
/// (mutant: `failed` forgets the failures it had).
#[tokio::test(flavor = "multi_thread")]
async fn failures_in_a_row_are_counted_and_the_wait_grows() {
    let rig = Rig::new();
    let cache = tiered(&rig, 1 << 20, None).await;
    let blob = TestBlob::new(4, 1024);
    assert_eq!(cache.hold(&blob.verified()), Hold::Held);
    rig.log.refuse("put_staged", 2);
    let now = Instant::now();
    let first = cache.drain_held(now).await.retry_at.expect("waits");
    let second = cache.drain_held(first).await.retry_at.expect("waits");
    assert!(
        matches!(
            cache.drain_state_for_test(&blob.digest),
            Some(DrainState::Failing { n: 2, .. })
        ),
        "{:?}",
        cache.drain_state_for_test(&blob.digest)
    );
    assert!(second - first > first - now, "the second wait is longer");
    assert_eq!(metric(&cache, FAILURES), 2.0);
    assert_eq!(cache.drain_held(second).await.written, 1);
}

/// A tier that takes no blob (its disk fails before the commit) sends the blob to the
/// object store, and the entry leaves once the index has it there; if the store fails too,
/// the entry stays and is counted, readable from RAM, and a later pass writes it. This is
/// the doc's `NoRoom` row. Catches an entry removed when the tier refuses, a blob that is
/// left out of the store, and a store failure that drops the entry.
#[tokio::test(flavor = "multi_thread")]
async fn a_blob_the_tier_does_not_take_goes_to_the_store_and_stays_held_if_that_fails() {
    let rig = Rig::new();
    let cache = tiered(&rig, 1 << 20, None).await;
    rig.fs
        .probe
        .fail_creates
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (a, b) = (TestBlob::new(5, 2048), TestBlob::new(6, 2048));
    assert_eq!(cache.hold(&a.verified()), Hold::Held);
    let report = cache.drain_held(Instant::now()).await;
    assert_eq!((report.written, report.failed), (1, 0));
    assert!(indexed(&rig, &a), "in the index");
    assert!(
        !rig.on_tier(&a.digest),
        "at an object of the store, not on the tier"
    );
    assert!(!cache.is_held_for_test(&a.digest));

    assert_eq!(cache.hold(&b.verified()), Hold::Held);
    rig.store.on_put(PutFault::Fail);
    let now = Instant::now();
    let report = cache.drain_held(now).await;
    assert_eq!((report.written, report.failed), (0, 1));
    assert!(
        cache.is_held_for_test(&b.digest),
        "cold failed: the entry stays"
    );
    assert!(!indexed(&rig, &b));
    assert_eq!(metric(&cache, FAILURES), 1.0);
    let retry = cache.drain_held(report.retry_at.expect("due")).await;
    assert_eq!(retry.written, 1);
    assert!(indexed(&rig, &b));
    assert_eq!(metric(&cache, BLOBS), 0.0);
}

/// One pass writes every blob it takes, and a pass takes at most `PASS_BLOBS`: with one
/// more than that held, the first pass takes `PASS_BLOBS` and the second the one left.
/// Catches a pass that takes none or only some, and one with no limit.
#[tokio::test]
async fn a_pass_takes_at_most_its_limit_and_the_next_takes_the_rest() {
    let rig = Rig::new();
    let cache = untiered(&rig, u64::MAX).await;
    let blobs: Vec<TestBlob> = (0..=PASS_BLOBS as u64)
        .map(|s| TestBlob::new(s, 16))
        .collect();
    for blob in &blobs {
        assert_eq!(cache.hold(&blob.verified()), Hold::Held);
    }
    let first = cache.drain_held(Instant::now()).await;
    assert_eq!((first.claimed, first.written), (PASS_BLOBS, PASS_BLOBS));
    let second = cache.drain_held(Instant::now()).await;
    assert_eq!((second.claimed, second.written), (1, 1));
    for blob in &blobs {
        assert!(indexed(&rig, blob));
    }
    assert_eq!(metric(&cache, BLOBS), 0.0);
}

/// The background drain writes a blob when it is held, with no call from the test but the
/// hold: `durable_waiting` returns (nothing missing) once the entry has left, which it
/// reports by notification. Then a failed write is retried by the loop itself at the
/// backoff (the paused clock moves to it when everything is idle): the PUT is refused
/// once, and the blob is still written. Catches a loop that is never woken by a hold
/// (mutant: `hold` does not ring the kick: the wait ends `Missing`) and one that does not
/// retry (mutant: `run_drain` does not wait for the due time).
#[tokio::test(start_paused = true)]
async fn the_background_drain_writes_what_is_held_and_retries_by_itself() {
    let rig = Rig::new();
    let cache = untiered(&rig, 1 << 20).await;
    let runner = tokio::spawn({
        let cache = Arc::clone(&cache);
        async move { match cache.run_drain().await {} }
    });
    let (a, b) = (TestBlob::new(7, 512), TestBlob::new(8, 512));
    assert_eq!(cache.hold(&a.verified()), Hold::Held);
    assert!(
        cache
            .durable_waiting(&[a.digest])
            .await
            .expect("asked")
            .is_empty(),
        "written by the drain"
    );
    assert!(indexed(&rig, &a));

    rig.store.on_put(PutFault::Fail);
    assert_eq!(cache.hold(&b.verified()), Hold::Held);
    assert!(
        cache
            .durable_waiting(&[b.digest])
            .await
            .expect("asked")
            .is_empty(),
        "written by the retry"
    );
    assert_eq!(metric(&cache, FAILURES), 1.0, "after one failed attempt");
    assert!(indexed(&rig, &b));
    runner.abort();
}

/// A shutdown drain that can finish does: three blobs held, one PUT refused first, and the
/// deadline is long; the answer is `Drained`, nothing is held, all three are in the index.
/// Catches a drain that stops after one pass (the refused blob would stay) and one that
/// leaves entries.
#[tokio::test(start_paused = true)]
async fn the_shutdown_drain_completes_within_its_deadline() {
    let rig = Rig::new();
    let cache = untiered(&rig, 1 << 20).await;
    let blobs: Vec<TestBlob> = (10..13).map(|s| TestBlob::new(s, 300)).collect();
    for blob in &blobs {
        assert_eq!(cache.hold(&blob.verified()), Hold::Held);
    }
    rig.store.on_put(PutFault::Fail);
    let ended = cache.drain_for_shutdown(Duration::from_secs(30)).await;
    assert_eq!(ended, ShutdownDrain::Drained);
    assert_eq!(metric(&cache, BLOBS), 0.0);
    for blob in &blobs {
        assert!(indexed(&rig, blob));
    }
    assert_eq!(metric(&cache, FAILURES), 1.0);
}

/// A shutdown drain that cannot finish ends at its deadline and says what it left: the
/// store refuses every PUT of one blob, the other is written, and the answer is
/// `TimedOut` with the one blob and its bytes, still held and still counted. Catches a
/// drain that ignores the deadline (the test would hang: the bound in `within` fails it),
/// one that reports `Drained` regardless (mutant: the timeout arm answers `Drained`), and
/// one that gives up the entries it could not write.
#[tokio::test(start_paused = true)]
async fn the_shutdown_drain_stops_at_its_deadline_and_says_what_it_left() {
    let rig = Rig::new();
    let cache = untiered(&rig, 1 << 20).await;
    let (stuck, fine) = (TestBlob::new(20, 700), TestBlob::new(21, 300));
    rig.store.fail_puts_holding(stuck.digest);
    assert_eq!(cache.hold(&stuck.verified()), Hold::Held);
    assert_eq!(cache.hold(&fine.verified()), Hold::Held);

    let ended = within(
        "the deadline",
        cache.drain_for_shutdown(Duration::from_secs(5)),
    )
    .await;
    assert_eq!(
        ended,
        ShutdownDrain::TimedOut {
            blobs: 1,
            bytes: 700
        }
    );
    assert!(cache.is_held_for_test(&stuck.digest), "never dropped");
    assert!(indexed(&rig, &fine));
    assert!(
        metric(&cache, FAILURES) >= 2.0,
        "it kept trying until the deadline"
    );
    assert_eq!(metric(&cache, BYTES), 700.0);
}

/// A shutdown drain does not wait out a failed blob's backoff: a blob that failed a moment
/// ago (so is due in a second) is written by a shutdown drain whose deadline is under that
/// second. Catches a shutdown drain that takes only blobs that are due (mutant: `force`
/// ignored: it times out).
#[tokio::test(start_paused = true)]
async fn the_shutdown_drain_does_not_wait_for_a_backoff() {
    let rig = Rig::new();
    let cache = untiered(&rig, 1 << 20).await;
    let blob = TestBlob::new(22, 300);
    assert_eq!(cache.hold(&blob.verified()), Hold::Held);
    rig.store.on_put(PutFault::Fail);
    let failed = cache.drain_held(Instant::now()).await;
    assert_eq!(failed.failed, 1);
    let ended = cache.drain_for_shutdown(Duration::from_millis(500)).await;
    assert_eq!(ended, ShutdownDrain::Drained);
    assert!(indexed(&rig, &blob));
}

/// A drain that is cut off while a write is in flight gives the blob back: the PUT is
/// parked, the shutdown deadline ends and drops the pass, and the entry is `Queued` again
/// (or `Failing` with the failures it had), not stuck as being written, so the next pass
/// takes it. Catches a pass that leaves its blobs claimed (mutant: no `Unclaim`), which
/// would never be drained again.
#[tokio::test(start_paused = true)]
async fn a_drain_cut_off_gives_the_blob_back() {
    let rig = Rig::new();
    let cache = untiered(&rig, 1 << 20).await;
    let (fresh, failed) = (TestBlob::new(23, 300), TestBlob::new(24, 300));
    assert_eq!(cache.hold(&failed.verified()), Hold::Held);
    rig.store.on_put(PutFault::Fail);
    assert_eq!(cache.drain_held(Instant::now()).await.failed, 1);
    assert_eq!(cache.hold(&fresh.verified()), Hold::Held);

    rig.store.hold_puts();
    let ended = cache.drain_for_shutdown(Duration::from_secs(5)).await;
    assert_eq!(
        ended,
        ShutdownDrain::TimedOut {
            blobs: 2,
            bytes: 600
        }
    );
    assert_eq!(
        cache.drain_state_for_test(&fresh.digest),
        Some(DrainState::Queued)
    );
    assert!(matches!(
        cache.drain_state_for_test(&failed.digest),
        Some(DrainState::Failing { n: 1, .. })
    ));

    rig.store.release_puts();
    let report = cache
        .drain_held(Instant::now() + Duration::from_secs(120))
        .await;
    assert_eq!(report.written, 2, "both are taken again");
}

/// The background drain goes on at once while a pass finds a full load: with two full
/// passes and one blob held before the loop starts (the holds ring one wake-up between
/// them), the loop takes the third pass without another hold, and `durable_waiting` for
/// every blob returns. Catches a loop that sleeps after a full
/// pass (mutant: the `continue` removed: the last blob waits for a wake-up that never
/// comes, and the wait ends missing).
#[tokio::test(start_paused = true)]
async fn the_background_drain_goes_on_after_a_full_pass() {
    let rig = Rig::new();
    let cache = untiered(&rig, u64::MAX).await;
    let blobs: Vec<TestBlob> = (0..=2 * PASS_BLOBS as u64)
        .map(|s| TestBlob::new(s, 16))
        .collect();
    for blob in &blobs {
        assert_eq!(cache.hold(&blob.verified()), Hold::Held);
    }
    let runner = tokio::spawn({
        let cache = Arc::clone(&cache);
        async move { match cache.run_drain().await {} }
    });
    // Every blob: the one taken by the second pass is not known in advance.
    let all: Vec<_> = blobs.iter().map(|b| b.digest).collect();
    assert!(
        cache.durable_waiting(&all).await.expect("asked").is_empty(),
        "the second pass ran by itself"
    );
    assert_eq!(metric(&cache, BLOBS), 0.0);
    runner.abort();
}
