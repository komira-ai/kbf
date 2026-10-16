//! The conformance suite C1 to C8, generic over a [`Backend`] factory so the in-memory
//! fake, SQLite and Postgres run the same clauses. A backend's own tests call
//! [`run_all`] with their factory; the fake's tests also run each clause against a
//! deliberately broken fake and require it to panic.
//!
//! Every assertion message starts with its clause and the defect it catches, which is
//! what a `should_panic(expected = ...)` test matches. No clause reads the wall clock:
//! time is the [`ManualClock`] the backend is opened with, and waits are joins.

use std::num::NonZeroUsize;
use std::ops::Bound;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::clock::{Clock, ManualClock};
use crate::store::Store;
use crate::types::{
    Change, Commit, Cond, Conflict, Event, ResumeToken, Revision, ScanPage, Txn, Version, Versioned,
};
use crate::{GROUP_COMMIT_WINDOW, TARGET_WRITES_PER_SECOND};

/// How many threads race one compare-and-set in C1.
pub const C1_RACERS: usize = 8;

/// The key C1's racers contend for. A broken fake that splits check from write does so
/// only on this key.
pub const RACE_KEY: &str = "c1/race";

/// Makes stores for the suite.
pub trait Backend {
    /// The store type.
    type Store: Store;

    /// A fresh, empty store whose clock is `clock`.
    fn open(&self, clock: Arc<ManualClock>) -> Self::Store;

    /// Crashes `store` (everything not durable is lost) and opens the same durable state
    /// again with the same clock.
    fn restart(&self, store: Self::Store) -> Self::Store;

    /// Runs `first` and `second` so that `second` commits before `first`, although
    /// `first` took the lower revision, calling `between` after `second` is visible to
    /// the store and before `first` is. A backend that cannot interleave (a single
    /// writer) commits in order and still calls `between` in the middle.
    ///
    /// # Errors
    /// The [`Conflict`] of a transaction that did not apply.
    fn commit_out_of_order(
        &self,
        store: &Self::Store,
        first: Txn,
        second: Txn,
        between: &mut dyn FnMut(),
    ) -> Result<(), Conflict>;
}

/// Runs C1 to C8 against `backend`.
pub fn run_all<B: Backend>(backend: &B) {
    c1_linearizable_cas(backend);
    c2_atomic_txn(backend);
    c3_monotonic_counter(backend);
    c4_store_clock_expiry(backend);
    c5_ordered_feed(backend);
    c6_durable_ack(backend);
    c7_group_commit(backend);
    c8_bounded_range_scan(backend);
}

fn run<S: Store>(store: &S, txn: Txn) -> Result<Commit, Conflict> {
    store.txn(txn).expect("the store is reachable")
}

fn value<S: Store>(store: &S, key: &str) -> Option<Versioned> {
    store.get(key).expect("the store is reachable")
}

fn next_value<S: Store>(store: &S, counter: &str) -> u64 {
    run(store, Txn::new().next(counter))
        .expect("a counter increment commits")
        .counters[0]
}

/// Every feed row after `from`, and the cursor to resume from.
fn drain<S: Store>(store: &S, from: Revision) -> (Vec<Change>, Revision) {
    let mut rows = Vec::new();
    let mut cursor = from;
    loop {
        let page = store.changes(cursor, 64).expect("the store is reachable");
        cursor = page.cursor;
        if page.rows.is_empty() {
            return (rows, cursor);
        }
        rows.extend(page.rows);
    }
}

/// C1: compare-and-set per key is linearizable, with a version never reissued.
pub fn c1_linearizable_cas<B: Backend>(backend: &B) {
    let store = backend.open(Arc::new(ManualClock::default()));

    run(&store, Txn::new().put("c1/k", "a", Cond::Absent))
        .expect("C1 create: create-if-absent on a fresh key must commit");
    let conflict = run(&store, Txn::new().put("c1/k", "b", Cond::Absent))
        .expect_err("C1 create: create-if-absent on an existing key must conflict");
    assert_eq!(
        conflict.index, 0,
        "C1 create: the conflict names the failing operation"
    );
    assert_eq!(
        value(&store, "c1/k").expect("c1/k exists").value,
        b"a",
        "C1 create: a conflict wrote"
    );

    let v1 = value(&store, "c1/k").expect("c1/k exists").version;
    run(&store, Txn::new().put("c1/k", "b", Cond::Version(v1)))
        .expect("C1 version: a put on the current version must commit");
    run(&store, Txn::new().put("c1/k", "c", Cond::Version(v1)))
        .expect_err("C1 version: a put on a stale version must conflict");
    let v2 = value(&store, "c1/k").expect("c1/k exists").version;
    assert_ne!(v1, v2, "C1 version: a write must change the version");

    run(&store, Txn::new().delete("c1/k", Cond::Version(v2)))
        .expect("C1 delete on the current version");
    assert!(
        value(&store, "c1/k").is_none(),
        "C1 delete: the key must be gone"
    );
    run(&store, Txn::new().put("c1/k", "d", Cond::Absent)).expect("C1 recreate after delete");
    let v3 = value(&store, "c1/k").expect("c1/k exists").version;
    assert!(
        v3 != v1 && v3 != v2,
        "C1 aba: a recreated key must not reuse an old version"
    );
    run(&store, Txn::new().put("c1/k", "e", Cond::Version(v1)))
        .expect_err("C1 aba: a stale holder of a deleted version must not match the new key");

    run(&store, Txn::new().put(RACE_KEY, "seed", Cond::Any)).expect("seed the race key");
    let seed = value(&store, RACE_KEY).expect("race key exists").version;
    let wins = thread::scope(|scope| {
        let racers: Vec<_> = (0..C1_RACERS)
            .map(|i| {
                let store = &store;
                scope.spawn(move || {
                    let txn = Txn::new().put(RACE_KEY, format!("racer-{i}"), Cond::Version(seed));
                    run(store, txn).is_ok()
                })
            })
            .collect();
        racers
            .into_iter()
            .map(|racer| racer.join().expect("a C1 racer thread panicked"))
            .filter(|won| *won)
            .count()
    });
    assert_eq!(
        wins, 1,
        "C1 race: exactly one of {C1_RACERS} concurrent compare-and-sets on one version may win, {wins} did"
    );
}

/// C2: a transaction over several keys applies entirely or not at all.
pub fn c2_atomic_txn<B: Backend>(backend: &B) {
    let store = backend.open(Arc::new(ManualClock::default()));

    let commit = run(
        &store,
        Txn::new()
            .put("c2/a", "1", Cond::Any)
            .put("c2/b", "2", Cond::Any)
            .next("c2/n"),
    )
    .expect("C2 commit: an unconditional transaction must commit");
    assert_eq!(commit.counters.len(), 1, "C2 commit: one counter per Next");
    let a = value(&store, "c2/a").expect("c2/a written");
    let b = value(&store, "c2/b").expect("c2/b written");
    assert!(
        a.version != b.version,
        "C2 commit: each row has its own version"
    );
    assert!(
        a.version.0 <= commit.revision.0 && b.version.0 <= commit.revision.0,
        "C2 commit: the commit revision covers every row it wrote"
    );

    let (rows_before, _) = drain(&store, Revision::ZERO);
    // The failing precondition is last, so a store that applies operations one by one
    // has already written the first two when it fails.
    let conflict = run(
        &store,
        Txn::new()
            .put("c2/a", "X", Cond::Any)
            .put("c2/c", "Y", Cond::Absent)
            .check("c2/b", Cond::Version(Version(u64::MAX))),
    )
    .expect_err("C2 aborted: a transaction with a failing precondition must conflict");
    assert_eq!(
        conflict.index, 2,
        "C2 aborted: the conflict names the failing operation"
    );
    assert_eq!(
        value(&store, "c2/a").expect("c2/a exists").value,
        b"1",
        "C2 aborted: a conflicting transaction left one of its writes behind"
    );
    assert!(
        value(&store, "c2/c").is_none(),
        "C2 aborted: a conflicting transaction left a key behind"
    );
    let (rows_after, _) = drain(&store, Revision::ZERO);
    assert_eq!(
        rows_before, rows_after,
        "C2 aborted: a conflicting transaction wrote feed rows"
    );
}

/// C3: counters are strictly monotonic, across threads, transactions and restart.
pub fn c3_monotonic_counter<B: Backend>(backend: &B) {
    let store = backend.open(Arc::new(ManualClock::default()));

    let mut last = 0;
    for _ in 0..5 {
        let n = next_value(&store, "c3/seq");
        assert!(n > last, "C3 order: {n} does not follow {last}");
        last = n;
    }
    let both = run(&store, Txn::new().next("c3/seq").next("c3/seq")).expect("two increments");
    assert!(
        both.counters.len() == 2 && both.counters[0] > last && both.counters[1] > both.counters[0],
        "C3 order: two increments in one transaction must be distinct and ascending"
    );
    let other = next_value(&store, "c3/other");
    assert!(
        next_value(&store, "c3/other") > other,
        "C3 order: a second counter is monotonic too"
    );

    const THREADS: usize = 8;
    const EACH: usize = 25;
    let per_thread: Vec<Vec<u64>> = thread::scope(|scope| {
        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                let store = &store;
                scope.spawn(move || {
                    (0..EACH)
                        .map(|_| next_value(store, "c3/shared"))
                        .collect::<Vec<u64>>()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("a C3 thread panicked"))
            .collect()
    });
    for run_of in &per_thread {
        assert!(
            run_of.windows(2).all(|w| w[0] < w[1]),
            "C3 order: one thread must see its own values ascend"
        );
    }
    let mut all: Vec<u64> = per_thread.concat();
    all.sort_unstable();
    all.dedup();
    assert_eq!(
        all.len(),
        THREADS * EACH,
        "C3 unique: no value may be handed out twice"
    );

    let before_restart = next_value(&store, "c3/seq");
    let store = backend.restart(store);
    let after = next_value(&store, "c3/seq");
    assert!(
        after > before_restart,
        "C3 restart: the counter went from {before_restart} back to {after}; a value was reissued"
    );
}

/// C4: expiry is judged by the store's clock and observable as a feed event.
pub fn c4_store_clock_expiry<B: Backend>(backend: &B) {
    let clock = Arc::new(ManualClock::default());
    let store = backend.open(Arc::clone(&clock));
    assert_eq!(
        store.now(),
        clock.now(),
        "C4 clock: the store must read the injected clock"
    );

    let ttl = Duration::from_secs(10);
    run(
        &store,
        Txn::new()
            .put_ttl("c4/t", "v", ttl, Cond::Any)
            .put("c4/keep", "k", Cond::Any),
    )
    .expect("create ttl and plain keys");
    run(&store, Txn::new().put_ttl("c4/renew", "v1", ttl, Cond::Any))
        .expect("create renewable key");
    let old = value(&store, "c4/t").expect("c4/t exists");
    let expires = old
        .expires_at
        .expect("C4 ttl: a key put with a ttl reports when it expires");
    assert!(
        expires > clock.now(),
        "C4 ttl: the expiry lies in the future of the store's clock"
    );
    let (_, cursor) = drain(&store, Revision::ZERO);

    clock.advance(Duration::from_secs(8));
    run(&store, Txn::new().put_ttl("c4/renew", "v2", ttl, Cond::Any)).expect("renew before expiry");
    assert!(
        value(&store, "c4/t").is_some(),
        "C4 expiry: a key must live until its time"
    );

    clock.advance(Duration::from_secs(3));
    assert!(
        value(&store, "c4/t").is_none(),
        "C4 expiry: the store's clock passed the ttl and the key is still readable"
    );
    let live: Vec<String> = store
        .scan_prefix("c4/")
        .expect("scan")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        live,
        ["c4/keep", "c4/renew"],
        "C4 expiry: a scan must hide the expired key and keep the renewed one"
    );
    assert_eq!(
        store.reap_expired().expect("reap"),
        1,
        "C4 expiry: the reaper removes exactly the expired key"
    );
    let (rows, _) = drain(&store, cursor);
    let expired: Vec<&str> = rows
        .iter()
        .filter(|row| row.event == Event::Expire)
        .map(|row| row.key.as_str())
        .collect();
    assert_eq!(
        expired,
        ["c4/t"],
        "C4 event: expiry must appear in the feed, once, for that key"
    );

    run(
        &store,
        Txn::new().put("c4/t", "again", Cond::Version(old.version)),
    )
    .expect_err("C4 absent: an expired key must not match its old version");
    run(&store, Txn::new().put("c4/t", "again", Cond::Absent))
        .expect("C4 absent: an expired key counts as absent");

    clock.advance(Duration::from_secs(8));
    assert_eq!(
        store.reap_expired().expect("reap"),
        1,
        "C4 expiry: the renewed key expires from its new time"
    );
    assert!(
        value(&store, "c4/renew").is_none(),
        "C4 expiry: the renewed key is gone"
    );
    assert!(
        value(&store, "c4/keep").is_some(),
        "C4 expiry: a key without a ttl never expires"
    );
}

/// C5: prefix scans are ordered; the feed resumes from a revision and never skips a row
/// that can still commit.
pub fn c5_ordered_feed<B: Backend>(backend: &B) {
    let store = backend.open(Arc::new(ManualClock::default()));

    for key in ["c5/b", "c5/a", "c5/c", "other/x"] {
        run(&store, Txn::new().put(key, key, Cond::Any)).expect("put");
    }
    let keys: Vec<String> = store
        .scan_prefix("c5/")
        .expect("scan")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        keys,
        ["c5/a", "c5/b", "c5/c"],
        "C5 scan: keys come back in key order, only under the prefix"
    );
    run(&store, Txn::new().delete("c5/b", Cond::Any)).expect("delete");

    let (all, end) = drain(&store, Revision::ZERO);
    assert_eq!(all.len(), 5, "C5 feed: one row per put and delete");
    assert!(
        all.windows(2).all(|w| w[0].revision < w[1].revision),
        "C5 feed: rows are in strictly ascending revision order"
    );
    assert_eq!(
        all[4].event,
        Event::Delete,
        "C5 feed: a delete is a Delete row"
    );
    assert_eq!(
        all[0].event,
        Event::Put(b"c5/b".to_vec()),
        "C5 feed: a put row carries the value"
    );

    // Paging by limit and resuming from the cursor reproduces the feed.
    let mut paged = Vec::new();
    let mut cursor = Revision::ZERO;
    loop {
        let page = store.changes(cursor, 2).expect("page");
        assert!(
            page.rows.len() <= 2,
            "C5 feed: a page holds at most the limit"
        );
        cursor = page.cursor;
        if page.rows.is_empty() {
            break;
        }
        paged.extend(page.rows);
    }
    assert_eq!(
        paged, all,
        "C5 resume: paging with cursors must neither skip nor repeat a row"
    );
    assert_eq!(cursor, end, "C5 resume: the end cursors agree");

    // Two commits whose revisions are taken in one order and commit in the other.
    let mut seen = Vec::new();
    let mut cursor = end;
    backend
        .commit_out_of_order(
            &store,
            Txn::new().put("c5/first", "1", Cond::Any),
            Txn::new().put("c5/second", "2", Cond::Any),
            &mut || {
                let (rows, next) = drain(&store, cursor);
                seen.extend(rows);
                cursor = next;
            },
        )
        .expect("the interleaved transactions apply");
    let (rows, _) = drain(&store, cursor);
    seen.extend(rows);
    let seen_keys: Vec<&str> = seen.iter().map(|row| row.key.as_str()).collect();
    assert!(
        seen_keys.contains(&"c5/first") && seen_keys.contains(&"c5/second"),
        "C5 interleaved: a poller that saw the later revision first skipped the earlier one, saw {seen_keys:?}"
    );
    assert!(
        seen.windows(2).all(|w| w[0].revision < w[1].revision),
        "C5 interleaved: rows reach the poller in revision order, each once"
    );
}

/// C6: an acknowledged write is still there after a crash and restart.
pub fn c6_durable_ack<B: Backend>(backend: &B) {
    let clock = Arc::new(ManualClock::default());
    let store = backend.open(Arc::clone(&clock));

    run(&store, Txn::new().put("c6/k", "v", Cond::Any)).expect("single write");
    run(
        &store,
        Txn::new().put_ttl("c6/ttl", "t", Duration::from_secs(100), Cond::Any),
    )
    .expect("ttl write");
    let batch = store
        .apply_batch(vec![
            Txn::new().put("c6/b1", "1", Cond::Any),
            Txn::new().put("c6/b2", "2", Cond::Any),
        ])
        .expect("batch");
    assert!(batch.iter().all(Result::is_ok), "the batch commits");
    let counter = next_value(&store, "c6/n");
    let k = value(&store, "c6/k").expect("c6/k exists");
    let ttl = value(&store, "c6/ttl").expect("c6/ttl exists");
    let (feed, _) = drain(&store, Revision::ZERO);

    let store = backend.restart(store);
    assert_eq!(
        value(&store, "c6/k"),
        Some(k),
        "C6 durable: an acknowledged write (same value, same version) was lost across restart"
    );
    assert_eq!(
        value(&store, "c6/ttl"),
        Some(ttl),
        "C6 durable: an acknowledged ttl write was lost"
    );
    assert!(
        value(&store, "c6/b1").is_some() && value(&store, "c6/b2").is_some(),
        "C6 durable: an acknowledged batch was lost"
    );
    assert!(
        next_value(&store, "c6/n") > counter,
        "C6 durable: an acknowledged counter value was lost"
    );
    let (replayed, _) = drain(&store, Revision::ZERO);
    assert!(
        replayed.starts_with(&feed),
        "C6 durable: the change feed lost an acknowledged row across restart"
    );

    clock.advance(Duration::from_secs(200));
    assert!(
        value(&store, "c6/ttl").is_none(),
        "C6 durable: a ttl key must still expire on schedule after restart"
    );
}

/// C7: writes are grouped, so a burst at the target rate costs one durable sync and
/// stays inside the commit window.
pub fn c7_group_commit<B: Backend>(backend: &B) {
    let clock = Arc::new(ManualClock::default());
    let store = backend.open(Arc::clone(&clock));
    let burst = usize::try_from(TARGET_WRITES_PER_SECOND).expect("the rate fits a usize");
    let batch: Vec<Txn> = (0..burst)
        .map(|i| Txn::new().put(format!("c7/{i:05}"), "x", Cond::Any))
        .collect();

    let stats_before = store.commit_stats();
    let started = clock.now();
    let results = store.apply_batch(batch).expect("the burst commits");
    let elapsed = clock.now().since(started);
    let stats = store.commit_stats();

    assert!(
        elapsed <= GROUP_COMMIT_WINDOW,
        "C7 latency: a burst of {burst} writes took {elapsed:?} of store time; the bound is {GROUP_COMMIT_WINDOW:?}"
    );
    assert_eq!(results.len(), burst, "C7 grouped: one result per write");
    assert!(
        results.iter().all(Result::is_ok),
        "C7 grouped: every write in the burst commits"
    );
    assert!(
        stats.syncs - stats_before.syncs <= 1,
        "C7 syncs: the burst took {} durable syncs; a group needs one",
        stats.syncs - stats_before.syncs
    );
    assert_eq!(
        stats.txns - stats_before.txns,
        burst as u64,
        "C7 grouped: the stats count every write"
    );

    let store = backend.restart(store);
    assert_eq!(
        store.scan_prefix("c7/").expect("scan").len(),
        burst,
        "C7 durable: a write acknowledged in a group was lost across restart"
    );
}

/// The live keys C8 stores under its prefix, in bytewise order: the prefix itself, plain
/// keys, and a key whose multi-byte character sorts after every ASCII one.
const C8_KEYS: [&str; 6] = ["c8/", "c8/a", "c8/b", "c8/c", "c8/d", "c8/\u{e9}"];

/// Keys just outside C8's prefix on both sides: `.` sorts before `/`, and `0` and `~`
/// after it.
const C8_OUTSIDE: [&str; 4] = ["c8", "c8.z", "c80", "c8~"];

fn scan_page<S: Store>(store: &S, start: Bound<&str>, limit: usize) -> ScanPage {
    let limit = NonZeroUsize::new(limit).expect("C8 limits are positive");
    store
        .scan_range("c8/", start, limit)
        .expect("the store is reachable")
}

fn keys_of(page: &ScanPage) -> Vec<&str> {
    page.rows.iter().map(|(key, _)| key.as_str()).collect()
}

/// Every page of a scan of `c8/` with `limit`, from the start of the prefix to the end.
fn scan_all<S: Store>(store: &S, limit: usize) -> Vec<ScanPage> {
    let mut pages = vec![scan_page(store, Bound::Unbounded, limit)];
    while let Some(token) = pages.last().and_then(|page| page.resume.clone()) {
        assert!(
            pages.len() <= C8_KEYS.len() + C8_OUTSIDE.len() + 2,
            "C8 resume: the scan never ends; a token resumes at or before its own page"
        );
        pages.push(scan_page(store, token.start(), limit));
    }
    pages
}

/// C8: a range scan under a prefix is bounded by its limit, ordered as C5's scan, begins
/// at its start bound, and pages with a resume token that continues exactly after the
/// last key, across concurrent commits.
pub fn c8_bounded_range_scan<B: Backend>(backend: &B) {
    let clock = Arc::new(ManualClock::default());
    let store = backend.open(Arc::clone(&clock));
    let mut seed = Txn::new();
    for key in C8_KEYS.iter().rev().chain(C8_OUTSIDE.iter()) {
        seed = seed.put(*key, *key, Cond::Any);
    }
    // A key between live ones that expires: it must neither appear nor shorten a page.
    seed = seed.put_ttl("c8/bb", "gone", Duration::from_secs(1), Cond::Any);
    run(&store, seed).expect("seed the C8 keys");
    clock.advance(Duration::from_secs(2));

    // The limit.
    let first = scan_page(&store, Bound::Unbounded, 2);
    assert_eq!(
        keys_of(&first),
        ["c8/", "c8/a"],
        "C8 limit: a scan with limit 2 must return the first 2 keys of the prefix in key order"
    );
    assert_eq!(
        first.resume.as_ref().map(ResumeToken::key),
        Some("c8/a"),
        "C8 resume: a full page with keys after it names its last key"
    );
    assert_eq!(
        first.rows[1].1,
        value(&store, "c8/a").expect("c8/a exists"),
        "C8 limit: a row carries the key's value, version and expiry"
    );
    // The start bound.
    let at = scan_page(&store, Bound::Included("c8/c"), 2);
    assert_eq!(
        keys_of(&at),
        ["c8/c", "c8/d"],
        "C8 start: an included start key is the first row"
    );
    let between = scan_page(&store, Bound::Included("c8/bz"), 1);
    assert_eq!(
        keys_of(&between),
        ["c8/c"],
        "C8 start: a start key that is not stored begins at the next key"
    );
    // The prefix: a start past it finds nothing.
    // `c8~` is stored past the prefix; nothing is stored at or after `c9`, so that scan
    // runs off the end of the store.
    for beyond in ["c8~", "c9"] {
        let past = scan_page(&store, Bound::Included(beyond), 4);
        assert_eq!(
            keys_of(&past),
            Vec::<&str>::new(),
            "C8 prefix: a start past the prefix ({beyond}) gives an empty page"
        );
        assert_eq!(
            past.resume, None,
            "C8 prefix: a start past the prefix ({beyond}) gives a final page"
        );
    }
    // A page as large as the prefix.
    let whole = scan_page(&store, Bound::Unbounded, C8_KEYS.len());
    let listed: Vec<(String, Versioned)> = store.scan_prefix("c8/").expect("scan");
    assert_eq!(
        whole.rows, listed,
        "C8 limit: a page as large as the prefix holds every live key, in scan_prefix's order"
    );
    assert_eq!(
        whole.resume, None,
        "C8 resume: a page that reached the end of the prefix has no token"
    );

    // The resume token.
    let second = scan_page(&store, first.resume.as_ref().expect("a token").start(), 2);
    assert_eq!(
        keys_of(&second),
        ["c8/b", "c8/c"],
        "C8 resume: the token must resume exactly after the last key of the page"
    );

    let after = scan_page(&store, Bound::Excluded("c8/c"), 1);
    assert_eq!(
        keys_of(&after),
        ["c8/d"],
        "C8 start: an excluded start key is not returned"
    );
    for before in [Bound::Included("c8"), Bound::Excluded("c8.z")] {
        assert_eq!(
            keys_of(&scan_page(&store, before, 1)),
            ["c8/"],
            "C8 start: a start before the prefix begins at the prefix ({before:?})"
        );
    }

    // The prefix: no page, however it starts, holds a key outside it.
    let last = scan_page(&store, Bound::Excluded("c8/d"), 4);
    assert_eq!(
        keys_of(&last),
        ["c8/\u{e9}"],
        "C8 prefix: the page ends with the prefix's last key in byte order"
    );
    for limit in 1..=C8_KEYS.len() + 1 {
        let pages = scan_all(&store, limit);
        let keys: Vec<&str> = pages.iter().flat_map(keys_of).collect();
        assert_eq!(
            keys, C8_KEYS,
            "C8 prefix: paging with limit {limit} returns each live key under the prefix once, in order, and nothing else"
        );
        assert!(
            pages.iter().all(|page| page.rows.len() <= limit),
            "C8 limit: a page holds at most the limit"
        );
        assert!(
            pages
                .iter()
                .rev()
                .skip(1)
                .all(|page| page.rows.len() == limit),
            "C8 limit: only the last page may be short (expired keys do not shorten one)"
        );
    }

    // Concurrent commits between pages: behind the token, ahead of it, a delete and an
    // update ahead of it.
    let first = scan_page(&store, Bound::Unbounded, 3);
    assert_eq!(
        keys_of(&first),
        ["c8/", "c8/a", "c8/b"],
        "C8 limit: page of 3"
    );
    run(
        &store,
        Txn::new()
            .put("c8/aa", "behind", Cond::Absent)
            .put("c8/bz", "ahead", Cond::Absent)
            .put("c8/c", "updated", Cond::Any)
            .delete("c8/d", Cond::Any),
    )
    .expect("the concurrent commit applies");
    let rest = scan_page(&store, first.resume.as_ref().expect("a token").start(), 8);
    assert_eq!(
        keys_of(&rest),
        ["c8/bz", "c8/c", "c8/\u{e9}"],
        "C8 concurrent: the next page holds keys committed ahead of the token, not behind it, and not a deleted key"
    );
    assert_eq!(
        rest.rows[1].1.value, b"updated",
        "C8 concurrent: a page shows the value committed before it was read"
    );
}
