//! The content cache's bound, order and flights, on their own (the walk's use of it is
//! in `input_walk_dircache_tests`).

use super::*;

fn digest(n: u64) -> Digest {
    kbf_segments::sha256(format!("directory {n}").as_bytes())
}

/// A Directory naming `files` files and no subdirectory: costs `files * 40 + 160`.
fn decoded(files: u64) -> Arc<Decoded> {
    Arc::new(Decoded {
        files: (0..files).map(digest).collect(),
        dirs: Box::new([]),
    })
}

fn cost(files: u64) -> u64 {
    decoded(files).cost()
}

/// Reads the digest as a leader would: gets the lead of a new read and finishes it.
fn put(cache: &DirCache, n: u64, files: u64) -> u64 {
    match cache.lookup(&digest(n)) {
        Lookup::Lead(lead) => lead.finish(decoded(files)),
        Lookup::Cached(_) => panic!("{n} is already cached"),
        Lookup::Follow(_) => panic!("{n} is being read"),
    }
}

fn cached(cache: &DirCache, n: u64) -> bool {
    matches!(cache.lookup(&digest(n)), Lookup::Cached(_))
}

/// Pins the sizing as a number beside the constant: a test that only reads the constant
/// passes for any value, and the figure is a decision a change must make on purpose.
#[test]
fn the_default_bound_is_the_sized_one() {
    assert_eq!(DIR_CACHE_BYTES, 128 << 20);
    assert_eq!(DirCache::default().capacity, 128 << 20);
    // The sizing in its doc: a digest is 40 bytes and an entry 160 of overhead, so about
    // 0.84 MB holds a komira-like tree of 1060 Directories and 15.5k files.
    assert_eq!(size_of::<Digest>(), 40);
    assert_eq!(cost(1), 40 + 160);
    let tree = 16_600 * 40 + 1060 * 160;
    assert!((DIR_CACHE_BYTES / tree) > 100, "{}", DIR_CACHE_BYTES / tree);
}

/// Catches: a bound in entries (or none) instead of bytes, an eviction of the most
/// recently used instead of the least, a hit that does not count as a use, and a byte
/// count that drifts from what is held. Room for three Directories of 10 files: five
/// are put, a hit on the oldest saves it, and after every step the bytes held are the
/// sum of the entries and within the bound.
#[test]
fn the_bound_is_in_bytes_and_the_least_recently_used_goes_first() {
    let ten = cost(10);
    let cache = DirCache::new(3 * ten);
    assert_eq!(
        (put(&cache, 0, 10), put(&cache, 1, 10), put(&cache, 2, 10)),
        (0, 0, 0)
    );
    assert_eq!((cache.len(), cache.bytes()), (3, 3 * ten));
    assert!(cached(&cache, 0), "a hit: 0 is now the most recently used");
    assert_eq!(put(&cache, 3, 10), 1, "1 went: the least recently used");
    assert!(cached(&cache, 0) && cached(&cache, 2) && cached(&cache, 3));
    assert!(!cached(&cache, 1));
    assert_eq!(cache.bytes(), 3 * ten);
    assert_eq!(put(&cache, 4, 10), 1, "the least recently used again: 0");
    assert!(!cached(&cache, 0) && cached(&cache, 2));
    assert_eq!((cache.len(), cache.bytes()), (3, 3 * ten));
    // A Directory of other size: bytes, not entries, are what is bounded. 25 files cost
    // more than two of 10, so all three go to make room.
    let big = cost(25);
    assert!(big > 2 * ten && big <= 3 * ten);
    assert_eq!(put(&cache, 5, 25), 3);
    assert_eq!((cache.len(), cache.bytes()), (1, big));
}

/// Catches: a Directory costing more than the whole cache evicting everything and then
/// itself being kept (the bound exceeded), or evicting everything for nothing.
#[test]
fn a_directory_larger_than_the_bound_is_not_kept() {
    let cache = DirCache::new(cost(10));
    put(&cache, 0, 10);
    assert_eq!(put(&cache, 1, 11), 0, "nothing pushed out for it");
    assert_eq!((cache.len(), cache.bytes()), (1, cost(10)));
    assert!(cached(&cache, 0));
    assert!(!cached(&cache, 1));
}

/// Catches: a digest inserted a second time counted twice in the bytes (the cache would
/// think itself full of nothing), or left twice in the order (an eviction of a ghost).
#[test]
fn inserting_a_digest_again_keeps_the_bytes_exact() {
    let cache = DirCache::new(10 * cost(10));
    put(&cache, 0, 10);
    assert_eq!(cache.insert(digest(0), decoded(10)), 0);
    assert_eq!((cache.len(), cache.bytes()), (1, cost(10)));
    assert_eq!(cache.lock().order.len(), 1);
}

/// Catches: a cache of zero bytes that keeps content or shares reads (the walk's tests
/// without a content cache would count it), and one that registers flights it never
/// ends.
#[test]
fn a_cache_of_zero_bytes_keeps_nothing_and_shares_nothing() {
    let cache = DirCache::new(0);
    let (first, second) = (cache.lookup(&digest(0)), cache.lookup(&digest(0)));
    assert!(matches!(first, Lookup::Lead(_)) && matches!(second, Lookup::Lead(_)));
    if let Lookup::Lead(lead) = first {
        assert_eq!(lead.finish(decoded(1)), 0);
    }
    drop(second);
    assert!(matches!(cache.lookup(&digest(0)), Lookup::Lead(_)));
    assert_eq!(
        (cache.len(), cache.bytes(), cache.lock().flights.len()),
        (0, 0, 0)
    );
}

/// Catches: a second reader of a digest being read that starts a read of its own, a
/// leader dropped (a failed or cancelled read) that leaves its followers waiting for
/// ever or its flight registered, and a finished read that is not found afterwards.
#[tokio::test(start_paused = true)]
async fn a_digest_is_read_by_one_and_a_dropped_read_frees_its_followers() {
    let cache = DirCache::new(10 * cost(10));
    let Lookup::Lead(lead) = cache.lookup(&digest(0)) else {
        panic!("the first lookup leads")
    };
    let Lookup::Follow(gate) = cache.lookup(&digest(0)) else {
        panic!("the second waits")
    };
    let waiting = tokio::spawn(async move { gate.acquire().await.is_err() });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished(), "the read has not ended");
    drop(lead);
    let ended = tokio::time::timeout(std::time::Duration::from_secs(60), waiting).await;
    assert!(
        ended.expect("left waiting").expect("task"),
        "its end closed the gate"
    );
    assert_eq!(cache.lock().flights.len(), 0);
    assert!(!cached(&cache, 0), "a dropped read cached nothing");
    // That lookup led again; end it properly.
    let Lookup::Lead(lead) = cache.lookup(&digest(1)) else {
        panic!("leads")
    };
    let Lookup::Follow(gate) = cache.lookup(&digest(1)) else {
        panic!("waits")
    };
    lead.finish(decoded(10));
    assert!(gate.acquire().await.is_err(), "finished: closed");
    assert!(cached(&cache, 1));
}
