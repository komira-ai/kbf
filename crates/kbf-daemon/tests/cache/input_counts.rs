//! The counts the cache keeps for the heartbeat (`Cache::input_counts`): entries filled
//! from the content store, entries served, and entries evicted, each with its bytes.

use std::sync::Arc;

use crate::support::cache_kit::{
    CountingCas, FakeClock, Gates, config, file, key, lease, open_started, stop,
    wait_for_fill_waits,
};
use crate::support::scratch;
use kbf_daemon::cache::{Cache, Point};

/// Catches a fill, a hit or an eviction left out of the counts, a count taken per file
/// name where the cache's unit is the entry, and bytes that are not the entry's size.
/// The budget holds three 100-byte entries. The first lease fills `a`, `b` and `c`
/// (and names `a`'s bytes twice: one entry, one fill); the second finds all three
/// (three hits, no fill); the third needs a fourth entry, which fills it and evicts the
/// least recently used one over the budget.
#[tokio::test]
async fn fills_hits_and_evictions_are_counted_per_entry_with_their_bytes() {
    let dir = scratch("cache-input-counts");
    let gates = Gates::new();
    let clock = FakeClock::at(1000);
    let cache = open_started(config(&dir, 300, Arc::clone(&gates), &clock), &gates);
    let cas = CountingCas::default();
    let blob = |c: u8| vec![c; 100];
    let three = cas.tree(&[
        file("a", &blob(b'a'), false),
        file("again", &blob(b'a'), false),
        file("b", &blob(b'b'), false),
        file("c", &blob(b'c'), false),
    ]);
    let counts = || cache.input_counts();
    assert_eq!(
        counts(),
        Default::default(),
        "a new cache has counted nothing"
    );

    lease(&cas, &cache, &three, &dir, "first")
        .await
        .expect("first");
    let first = counts();
    assert_eq!((first.fills, first.fill_bytes), (3, 300), "{first:?}");
    assert_eq!((first.hits, first.hit_bytes), (0, 0), "{first:?}");
    assert_eq!((first.evictions, first.evicted_bytes), (0, 0), "{first:?}");

    clock.advance(60);
    lease(&cas, &cache, &three, &dir, "second")
        .await
        .expect("second");
    let second = counts();
    assert_eq!((second.fills, second.fill_bytes), (3, 300), "{second:?}");
    assert_eq!((second.hits, second.hit_bytes), (3, 300), "{second:?}");

    clock.advance(60);
    let fourth = cas.tree(&[file("d", &blob(b'd'), false)]);
    lease(&cas, &cache, &fourth, &dir, "third")
        .await
        .expect("third");
    let third = counts();
    assert_eq!((third.fills, third.fill_bytes), (4, 400), "{third:?}");
    assert_eq!((third.hits, third.hit_bytes), (3, 300), "{third:?}");
    assert_eq!(
        (third.evictions, third.evicted_bytes),
        (1, 100),
        "{third:?}"
    );
    assert_eq!(third.trees, None, "the cache alone says nothing of trees");
    stop(cache, &gates);
}

/// Catches an entry copied from its executable sibling counted as a fill from the
/// content store: one blob, plain and executable, is two entries but one fetch.
#[tokio::test]
async fn an_entry_copied_from_its_sibling_is_not_a_fill() {
    let dir = scratch("cache-input-counts-sibling");
    let gates = Gates::new();
    // One key at a time, so that the second finds the first indexed.
    let mut serial = config(&dir, 1 << 30, Arc::clone(&gates), &FakeClock::at(1000));
    serial.link_concurrency = 1;
    let cache = Cache::open(serial).expect("open");
    let cas = CountingCas::default();
    let bytes = vec![b's'; 64];
    let both = cas.tree(&[file("plain", &bytes, false), file("tool", &bytes, true)]);
    lease(&cas, &cache, &both, &dir, "both")
        .await
        .expect("lease");
    let counts = cache.input_counts();
    assert_eq!((counts.fills, counts.fill_bytes), (1, 64), "{counts:?}");
    assert_eq!(cas.reads_of(&crate::support::cache_kit::digest(&bytes)), 1);
    stop(cache, &gates);
}

/// Catches a second lease's use of an entry another lease is filling not counted as a hit, and
/// the entry counted as filled twice: the first lease's fill is held open after the entry is
/// written, the second lease asks for the same missing entry, and once the first is released
/// the counts are one fill of the entry's bytes and one hit (the second lease found it, whether
/// it waited on the slot or came after). The totals do not depend on which of the two happens.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn two_leases_for_one_missing_entry_count_one_fill_and_one_hit() {
    let dir = scratch("cache-input-counts-race");
    let gates = Gates::new();
    let cache = Cache::open(config(
        &dir,
        1 << 30,
        Arc::clone(&gates),
        &FakeClock::at(1000),
    ))
    .expect("open");
    let cas = Arc::new(CountingCas::default());
    let bytes = vec![b'r'; 80];
    let tree = cas.tree(&[file("f", &bytes, false)]);
    let k = key(&bytes, false);
    let held = gates.hold(move |p| *p == Point::Filled(&k));
    let spawn = |name: &'static str| {
        let (cas, cache, dir, tree) = (
            Arc::clone(&cas),
            Arc::clone(&cache),
            dir.clone(),
            tree.clone(),
        );
        tokio::spawn(async move { lease(&cas, &cache, &tree, &dir, name).await })
    };
    let first = spawn("first");
    let waiting = Arc::clone(&gates);
    tokio::task::spawn_blocking(move || waiting.wait_reached(held))
        .await
        .expect("held");
    let second = spawn("second");
    // The second lease waits on the slot the first one holds (its read is held).
    wait_for_fill_waits(&cache, 1).await;
    gates.release(held);
    first.await.expect("joined").expect("first");
    second.await.expect("joined").expect("second");
    let counts = cache.input_counts();
    assert_eq!((counts.fills, counts.fill_bytes), (1, 80), "{counts:?}");
    assert_eq!((counts.hits, counts.hit_bytes), (1, 80), "{counts:?}");
    assert_eq!(cas.reads_of(&crate::support::cache_kit::digest(&bytes)), 1);
    stop(cache, &gates);
}
