//! The counts the cache keeps for the heartbeat (`Cache::input_counts`): entries filled
//! from the content store, entries served, and entries evicted, each with its bytes.

use std::sync::Arc;

use crate::support::cache_kit::{CountingCas, FakeClock, Gates, config, file, lease, stop};
use crate::support::scratch;
use kbf_daemon::cache::Cache;

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
    let cache = Cache::open(config(&dir, 300, Arc::clone(&gates), &clock)).expect("open");
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
