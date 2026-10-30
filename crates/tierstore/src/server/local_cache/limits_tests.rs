//! The limits of the fills: a fill that panics, the reservation against the critical mark, the
//! most fills that run at once, and readers that share a fill that fails.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tokio::task::JoinHandle;

use super::fill::{Filled, Pending};
use super::layout::name_of;
use super::metrics::{FailedFill, read};
use super::testing::{Rig, digest, pattern};
use super::{Inner, locked};
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
/// reservation is gone). The second is refused and gives its bytes back. With the steps the
/// other way round, the index is read before the commit and the reservation after it, and the
/// object is counted in neither. The bytes held before the second reservation already refuse
/// it, so this test is red for that order only; it does not tell the index read from the
/// reserved bytes. A reservation that fits is kept. (The two terms of the sum are told apart
/// by `a_reservation_counts_the_index_the_running_fills_and_both`.)
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

/// A cache of 100 bytes with the critical mark at 95, called `name`.
fn tight(name: &str) -> Rig {
    Rig::with(name, |config| {
        config.budget = 100;
        config.high_water = 100;
        config.low_water = 50;
    })
}

/// Puts an object of `size` bytes in the index of the rig, as a fill that committed does.
fn commit_object(rig: &Rig, key: &str, size: u64) {
    let name = name_of(key);
    locked(&rig.cache.inner.index).insert(&name.hex, size, "tag".to_owned(), 0);
}

/// Asserts that a reservation is refused for the objects in the index, for the fills that run, and
/// for the two together, each with sizes where the other term alone would let it through, and
/// that one that lands exactly on the critical mark (95) is kept. The sum is of the index
/// (`held`), the reservations made before (`before`) and the new size: with 60 in the index, 40
/// more is refused though nothing is reserved; with 60 reserved and an empty index, the same;
/// with 30 in the index and 30 reserved, 40 more is refused though each term and the size make 70.
/// A reservation that is refused leaves nothing reserved. A defect that left the index out of
/// the sum passes the first case, one that left the reserved bytes out the second, and one that
/// took the larger of the two terms the third.
#[tokio::test]
async fn a_reservation_counts_the_index_the_running_fills_and_both() {
    let indexed = tight("tscache-reserve-index");
    commit_object(&indexed, "cas/b/one", 60);
    assert!(indexed.cache.inner.reserve(40).is_none());
    assert_eq!(indexed.cache.inner.reserved.load(Ordering::SeqCst), 0);
    assert!(indexed.cache.inner.reserve(35).is_some());

    let running = tight("tscache-reserve-running");
    let inner = &running.cache.inner;
    let fill = inner.reserve(60).expect("room for the fill that runs");
    assert!(inner.reserve(40).is_none());
    assert_eq!(inner.reserved.load(Ordering::SeqCst), 60);
    assert!(inner.reserve(35).is_some());
    drop(fill);
    assert_eq!(inner.reserved.load(Ordering::SeqCst), 0);

    let both = tight("tscache-reserve-both");
    commit_object(&both, "cas/b/one", 30);
    let inner = &both.cache.inner;
    let fill = inner.reserve(30).expect("room for the fill that runs");
    assert!(inner.reserve(40).is_none());
    assert_eq!(inner.reserved.load(Ordering::SeqCst), 30);
    assert!(inner.reserve(35).is_some());
    drop(fill);
}

/// Asserts that a fill started by a thread of a runtime that is shutting down (its task is
/// dropped by the spawn, on the thread that spawned it) does not wait for the table of the fills
/// that run, and leaves no key in it: the dropped task takes its key out, the table and the
/// permit are free again, and the reader that joined gets the `Unavailable` error and the failure
/// is counted as `aborted`. The start runs from the drop of a task during the shutdown, which
/// is when a spawn drops its task at once. A start made with the table locked would wait on
/// that lock for ever, so the defect is a test that does not end.
#[test]
fn a_fill_dropped_by_its_spawn_leaves_no_key_in_the_table() {
    /// Starts a fill when it is dropped.
    struct Starter {
        inner: Arc<Inner>,
        joined: Arc<Mutex<Option<Result<Pending, Filled>>>>,
    }

    impl Drop for Starter {
        fn drop(&mut self) {
            let joined = self.inner.join_or_start("cas/b/obj", &name_of("cas/b/obj"));
            *locked(&self.joined) = Some(joined);
        }
    }

    let rig = Rig::new("tscache-spawn-dropped");
    let inner = Arc::clone(&rig.cache.inner);
    let joined = Arc::new(Mutex::new(None));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let starter = Starter {
        inner: Arc::clone(&inner),
        joined: Arc::clone(&joined),
    };
    let task = runtime.spawn(async move {
        let _starter = starter;
        std::future::pending::<()>().await;
    });
    drop(task);
    drop(runtime);

    assert!(locked(&inner.flights).is_empty());
    assert_eq!(
        inner.permits.available_permits(),
        rig.config.max_concurrent_fills
    );
    let pending = locked(&joined).take().unwrap().unwrap();
    let waiter = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let error = waiter.block_on(pending).unwrap_err();
    assert!(error.is_unavailable(), "{error}");
    assert_eq!(rig.cache.counters().failures(FailedFill::Aborted), 1);
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

    rig.replace(key, &right).await;
    assert_eq!(rig.get(key).await.unwrap(), right);
    assert_eq!(read(&counters.fills), 1);
    assert!(rig.has_file(key));
}
