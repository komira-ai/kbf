//! The shared write-pool rig (`support/rig.rs`) run from a second test binary, with the
//! seams it and the write tier's log give a test: the sync gate's positive event
//! ([`Counting::wait_waiting`]), a held metadata commit ([`Log::hold`]) and the pool's
//! drain step ([`Cache::with_drain_hook`]). Each test waits on an event the seam raises;
//! none sleeps or polls.

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

use kbf_front::DrainHook;
use kbf_meta::BlobAnswer;
use kbf_objstore::KeyPrefix;
use kbf_tier::TierOptions;
use rig::{Rig, pool, pooled, put};
use support::{SEGMENT, SERVER, TestBlob};

const GIB: u64 = 1 << 30;

/// Awaits `event`, failing the test with a message instead of hanging if it never comes.
/// The bound only ends a hang; no test asserts on how long an event takes.
async fn within<T>(what: &str, event: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(60), event)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// With the syncs paused, a write is parked in `sync`: after `wait_waiting(1)` the gate's
/// `waiting` count is exactly 1, no sync completed and the write is not acknowledged;
/// after `resume` and `wait_waiting(0)` it completes and the writer has left the gate.
/// Catches a gate that counts nothing, one that does not pause, a `wait_waiting` that
/// returns before the writer is parked (the `waiting` assert), or one that never wakes
/// for the writer leaving (the test hangs).
#[tokio::test(flavor = "multi_thread")]
async fn a_paused_sync_is_a_positive_event_and_resume_releases_it() {
    let rig = Rig::new();
    let cache = rig.start(1, 16).await;
    let blob = TestBlob::new(1, 4096);
    rig.fs.pause();
    let writing = tokio::spawn({
        let (cache, blob) = (Arc::clone(&cache), blob.clone());
        async move { put(&cache, &blob).await }
    });
    let fs = rig.fs.clone();
    tokio::task::spawn_blocking(move || fs.wait_waiting(1))
        .await
        .expect("join");
    assert_eq!(rig.fs.waiting(), 1, "the writer is parked at the gate");
    assert_eq!(rig.fs.syncs(), 0, "no sync completed while paused");
    assert!(!writing.is_finished(), "not acknowledged before its sync");

    rig.fs.resume();
    let fs = rig.fs.clone();
    tokio::task::spawn_blocking(move || fs.wait_waiting(0))
        .await
        .expect("join");
    writing.await.expect("join").expect("written");
    assert!(rig.fs.syncs() >= 1);
    assert!(rig.on_tier(&blob.digest));
}

/// A held `PutStaged` parks the commit before the log applies it: the blob is not in the
/// index and the log applied nothing while the write waits; after `release` it applies
/// once and the write completes. Catches a hold that lets the commit through, applies it
/// first and parks the answer, or does not wake after `release`.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_put_staged_waits_unapplied_until_released() {
    let rig = Rig::new();
    let cache = rig.start(1, 16).await;
    let blob = TestBlob::new(2, 4096);
    rig.log.hold("put_staged");
    let writing = tokio::spawn({
        let (cache, blob) = (Arc::clone(&cache), blob.clone());
        async move { put(&cache, &blob).await }
    });
    within(
        "the put_staged commit to park at its hold",
        rig.log.wait_held("put_staged", 1),
    )
    .await;
    assert_eq!(rig.log.applied("put_staged"), 0);
    assert_eq!(rig.log.answer(&blob.digest), BlobAnswer::Absent);
    assert!(!writing.is_finished());

    rig.log.release("put_staged");
    writing.await.expect("join").expect("written");
    assert_eq!(rig.log.applied("put_staged"), 1);
    assert!(rig.log.applied_count() >= 1);
    assert!(rig.on_tier(&blob.digest));
}

/// The drain hook runs after the batch's `PutStaged` is applied and before its blobs are
/// answered: parked there, the index has the blob and the write has not returned; after
/// the hook goes on, the write completes. Catches a hook called before the apply, after
/// the answer, or not at all.
#[tokio::test(flavor = "multi_thread")]
async fn the_drain_hook_runs_between_the_index_apply_and_the_answer() {
    let rig = Rig::new();
    let (parked_tx, mut parked) = tokio::sync::mpsc::unbounded_channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    let hook: DrainHook = Arc::new(move |digests| {
        let _ = parked_tx.send(digests.to_vec());
        let _ = go_rx.lock().expect("lock").recv();
    });
    let cache = kbf_front::Cache::open(rig.log.clone(), rig.store.clone(), KeyPrefix::default())
        .await
        .expect("open")
        .with_segment_limit(SEGMENT)
        .with_drain_hook(hook);
    let (cache, _) = pooled(cache, pool(1, 16))
        .attach_tier(rig.fs.clone(), TierOptions::new(GIB), SERVER, None)
        .await
        .expect("attach");
    let cache = Arc::new(cache);
    let blob = TestBlob::new(3, 4096);
    let writing = tokio::spawn({
        let (cache, blob) = (Arc::clone(&cache), blob.clone());
        async move { put(&cache, &blob).await }
    });

    let digests = within("the drain hook to run", parked.recv())
        .await
        .expect("the hook ran");
    assert_eq!(digests, vec![blob.digest]);
    assert!(rig.on_tier(&blob.digest), "the index has it at the hook");
    assert!(!writing.is_finished(), "not answered at the hook");

    go_tx.send(()).expect("go");
    writing.await.expect("join").expect("written");
}
