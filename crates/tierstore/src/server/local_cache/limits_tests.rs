//! The limits of the fills: a fill that panics, the reservation against the critical mark, the
//! most fills that run at once, and readers that share a fill that fails.

use std::sync::atomic::Ordering;

use bytes::Bytes;
use tokio::task::JoinHandle;

use super::layout::name_of;
use super::locked;
use super::metrics::{FailedFill, read};
use super::testing::{Rig, digest, pattern};
use crate::{Backend, Error, Op};

type Reader = JoinHandle<Result<Bytes, Error>>;

/// `count` readers of `key`, started on tasks of their own.
fn readers(rig: &Rig, key: &str, count: usize) -> Vec<Reader> {
    (0..count)
        .map(|_| {
            let cache = rig.cache.clone();
            let key = key.to_owned();
            tokio::spawn(async move { cache.get(&key, None).await })
        })
        .collect()
}

/// Yields until the cache has counted `count` misses. A miss is counted just before the reader
/// joins or starts the fill of its key, with no wait between, so the readers counted have joined.
async fn until_misses(rig: &Rig, count: u64) {
    while read(&rig.cache.counters().misses) < count {
        tokio::task::yield_now().await;
    }
}

/// The number of entries in the stage directory of the rig.
fn staged(rig: &Rig) -> usize {
    std::fs::read_dir(rig.config.path.join(".stage"))
        .unwrap()
        .count()
}

/// Asserts that a fill that panics does not wedge its key: the readers that waited for it get an
/// `Unavailable` error (and are not panicked), the failure is counted once as `aborted`, the
/// table of the fills that run, the reservation, the permit and the stage directory are
/// empty again, and the next read of the same key fills it and is served. A defect that left the
/// key in the table would panic or hang every later read of it.
#[tokio::test]
async fn a_fill_that_panics_frees_its_key_and_its_waiters_get_an_error() {
    let rig = Rig::new("tscache-panic");
    let body = pattern(100);
    rig.put("cas/b/obj", &body).await;
    let gate = rig.tamper.hold();
    rig.tamper.panic_once.store(true, Ordering::SeqCst);
    let waiting = readers(&rig, "cas/b/obj", 3);
    until_misses(&rig, 3).await;
    gate.add_permits(64);
    for reader in waiting {
        let error = reader.await.unwrap().unwrap_err();
        assert!(error.is_unavailable(), "{error}");
    }
    let inner = &rig.cache.inner;
    assert_eq!(rig.cache.counters().failures(FailedFill::Aborted), 1);
    assert!(locked(&inner.flights).is_empty());
    assert_eq!(inner.reserved.load(Ordering::SeqCst), 0);
    assert_eq!(
        inner.permits.available_permits(),
        rig.config.max_concurrent_fills
    );
    assert_eq!(staged(&rig), 0);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(
        rig.cache
            .metrics()
            .contains("tierstore_cache_fill_failures_total{reason=\"aborted\"} 1\n")
    );

    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(read(&rig.cache.counters().fills), 1);
    assert_eq!(rig.cache.held(), (100, 1));
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(read(&rig.cache.counters().hits), 1);
}

/// Asserts the order of the reservation: the bytes are reserved before the index is read. A fill
/// of 50 bytes holds a reservation in a cache of 100 bytes (critical 95); a second reservation of
/// 50 starts, and between its two steps the first fill commits (its object is in the index and its
/// reservation is gone). The second is refused and gives its bytes back: the object is counted
/// in the index. With the steps the other way round, the index is read before the commit and the
/// reservation after it, and the object is counted in neither. A reservation that fits is kept.
#[tokio::test]
async fn a_fill_that_commits_between_the_steps_of_a_reservation_is_counted() {
    let rig = Rig::with("tscache-reserve-race", |config| {
        config.budget = 100;
        config.high_water = 100;
        config.low_water = 50;
    });
    let inner = &rig.cache.inner;
    let first = inner.reserve(50).expect("room for the first");
    let commit = || {
        let name = name_of("cas/b/first");
        locked(&inner.index).insert(&name.hex, 50, "tag".to_owned(), 0);
        drop(first);
    };
    assert!(inner.reserve_between(50, commit).is_none());
    assert_eq!(inner.reserved.load(Ordering::SeqCst), 0);
    assert_eq!(rig.cache.held(), (50, 1));

    let fits = inner.reserve_between(40, || {}).expect("90 is under 95");
    assert_eq!(inner.reserved.load(Ordering::SeqCst), 40);
    drop(fits);
    assert_eq!(inner.reserved.load(Ordering::SeqCst), 0);
}

/// Asserts the most fills that run at once. With one permit and the fill of one key held, a
/// reader of a second key is served from the upstream while that fill is still held, keeps
/// nothing, and is counted as skipped and as busy; a second reader of the first key shares its
/// fill and is not skipped. When the fill ends the permit is back: the second key fills.
#[tokio::test]
async fn a_miss_with_every_fill_running_is_passed_through_and_not_kept() {
    let rig = Rig::with("tscache-busy", |config| config.max_concurrent_fills = 1);
    let (held, passed) = (pattern(100), pattern(60));
    rig.put("cas/b/held", &held).await;
    rig.put("cas/b/passed", &passed).await;
    let gate = rig.tamper.hold_key("cas/b/held");
    let first = readers(&rig, "cas/b/held", 1);
    until_misses(&rig, 1).await;
    let sharing = readers(&rig, "cas/b/held", 1);
    until_misses(&rig, 2).await;
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.fills_skipped_busy), 0);

    // Served while the fill is held: the gate has no permit yet.
    assert_eq!(rig.get("cas/b/passed").await.unwrap(), passed);
    assert_eq!(read(&counters.fills_skipped), 1);
    assert_eq!(read(&counters.fills_skipped_busy), 1);
    assert_eq!(read(&counters.fills), 0);
    assert!(!rig.has_file("cas/b/passed"));
    assert_eq!(rig.cache.held(), (0, 0));

    gate.add_permits(64);
    for reader in first.into_iter().chain(sharing) {
        assert_eq!(reader.await.unwrap().unwrap(), held);
    }
    assert_eq!(read(&counters.fills), 1);
    assert_eq!(rig.cache.held(), (100, 1));
    assert_eq!(rig.cache.inner.permits.available_permits(), 1);

    assert_eq!(rig.get("cas/b/passed").await.unwrap(), passed);
    assert_eq!(read(&counters.fills), 2);
    assert!(rig.has_file("cas/b/passed"));
    assert_eq!(read(&counters.fills_skipped_busy), 1);
    let text = rig.cache.metrics();
    assert!(
        text.contains("tierstore_cache_fills_skipped_busy_total 1\n"),
        "{text}"
    );
}

/// Asserts that readers that share a fill that fails on the upstream all get the error, that the
/// failure is counted once, that nothing is kept or left staged, and that the next read fills
/// again.
#[tokio::test]
async fn readers_that_share_a_fill_the_upstream_fails_all_get_the_error() {
    let rig = Rig::new("tscache-shared-upstream-error");
    let body = pattern(100);
    rig.put("cas/b/obj", &body).await;
    let gate = rig.tamper.hold();
    rig.tamper.fail_once.store(true, Ordering::SeqCst);
    let waiting = readers(&rig, "cas/b/obj", 4);
    until_misses(&rig, 4).await;
    gate.add_permits(64);
    for reader in waiting {
        let error = reader.await.unwrap().unwrap_err();
        assert!(error.is_unavailable(), "{error}");
    }
    let counters = rig.cache.counters();
    assert_eq!(counters.failures(FailedFill::UpstreamError), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/obj"));
    assert_eq!(staged(&rig), 0);
    assert!(locked(&rig.cache.inner.flights).is_empty());
    assert_eq!(rig.calls(Op::Get), 1);

    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(read(&counters.fills), 1);
    assert_eq!(rig.calls(Op::Get), 2);
}

/// Asserts that readers that share a fill whose bytes do not hash to the name (a `cas/l` key) all
/// get the error and none is served the bytes, that the mismatch is counted once, that nothing is
/// kept or left staged, and that once the upstream has the right bytes the next read fills.
#[tokio::test]
async fn readers_that_share_a_fill_with_the_wrong_hash_all_get_the_error() {
    let rig = Rig::new("tscache-shared-mismatch");
    let right = b"what the name says".to_vec();
    let key = format!("cas/l/{}", digest(&right));
    let key = key.as_str();
    rig.put(key, &pattern(50)).await;
    let gate = rig.tamper.hold();
    let waiting = readers(&rig, key, 4);
    until_misses(&rig, 4).await;
    gate.add_permits(64);
    for reader in waiting {
        let error = reader.await.unwrap().unwrap_err();
        assert!(error.is_unavailable(), "{error}");
    }
    let counters = rig.cache.counters();
    assert_eq!(counters.failures(FailedFill::HashMismatch), 1);
    assert_eq!(read(&counters.bytes_served_upstream), 0);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(key));
    assert_eq!(staged(&rig), 0);
    assert!(locked(&rig.cache.inner.flights).is_empty());

    rig.put(key, &right).await;
    assert_eq!(rig.get(key).await.unwrap(), right);
    assert_eq!(read(&counters.fills), 1);
    assert!(rig.has_file(key));
}
