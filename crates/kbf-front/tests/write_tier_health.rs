//! The write tier's health as a storage node's heartbeat reads it (`Cache::tier_health`):
//! the budget and use, the unflushed backlog and its age, and the flush failure and
//! conflict counts. Every test's comment names the defect it is there to catch.

mod common;
#[allow(dead_code)]
#[path = "write_tier/support.rs"]
mod support;

use common::faulty::PutFault;
use kbf_front::{Cache, FLUSH_BACKOFF_FIRST};
use kbf_objstore::Capabilities;
use support::{Farm, blobs, later, verified};

const GIB: u64 = 1 << 30;

/// Catches: a cache with no write tier answering a health of zeros (it must answer none,
/// so the heartbeat says nothing and the operator sees "unknown", not an empty tier).
#[tokio::test]
async fn a_cache_with_no_tier_has_no_health() {
    assert_eq!(Cache::memory().tier_health(), None);
}

/// Catches: the backlog read from another source than the tier (it would stay at zero or
/// at the staged count after a flush), a budget that is not the configured one, an age
/// missing while a block waits, and a backlog or age that stays after the flush.
#[tokio::test]
async fn the_backlog_and_budget_follow_the_tier() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let idle = cache.tier_health().expect("a tier");
    assert_eq!(idle.budget_bytes, GIB);
    assert_eq!(
        (
            idle.unflushed_bytes,
            idle.unflushed_blocks,
            idle.oldest_unflushed
        ),
        (0, 0, None)
    );

    cache
        .store_blobs(verified(&blobs(1, 3, 4096)))
        .await
        .expect("staged");
    let waiting = cache.tier_health().expect("a tier");
    assert!(waiting.unflushed_bytes >= 3 * 4096, "{waiting:?}");
    assert_eq!(waiting.unflushed_blocks, 1);
    assert!(waiting.oldest_unflushed.is_some(), "{waiting:?}");
    assert!(waiting.used_bytes > 0, "{waiting:?}");
    assert_eq!(waiting.budget_bytes, GIB);

    cache.flush_tier(later()).await.expect("round");
    let flushed = cache.tier_health().expect("a tier");
    assert_eq!(
        (
            flushed.unflushed_bytes,
            flushed.unflushed_blocks,
            flushed.oldest_unflushed
        ),
        (0, 0, None),
        "{flushed:?}"
    );
}

/// Catches: the failure count read from the wrong counter or not counted per attempt: one
/// refused PUT is one failure, a second attempt that fails is two, and a flush that then
/// succeeds adds none. No conflict is reported where none happened.
#[tokio::test]
async fn failed_flushes_are_counted_per_attempt() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    cache
        .store_blobs(verified(&blobs(1, 1, 4096)))
        .await
        .expect("staged");
    assert_eq!(cache.tier_health().expect("a tier").flush_failures, 0);
    farm.store.on_put(PutFault::Fail);
    farm.store.on_put(PutFault::Fail);
    let t0 = later();
    cache.flush_tier(t0).await.expect("round");
    assert_eq!(cache.tier_health().expect("a tier").flush_failures, 1);
    cache
        .flush_tier(t0 + FLUSH_BACKOFF_FIRST)
        .await
        .expect("round");
    assert_eq!(cache.tier_health().expect("a tier").flush_failures, 2);
    cache
        .flush_tier(t0 + FLUSH_BACKOFF_FIRST * 4)
        .await
        .expect("round");
    let done = cache.tier_health().expect("a tier");
    assert_eq!((done.flush_failures, done.flush_conflicts), (2, 0));
    assert_eq!(done.unflushed_blocks, 0);
}
