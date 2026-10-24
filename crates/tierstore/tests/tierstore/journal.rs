//! The journal the proxy writes: batches, numbers, restarts and failed writes.

use std::time::Duration;

use tierstore::{Backend, Error, Op, encode_journal};
use tokio::time::Instant;

use crate::support::{
    Rig, assert_absent, body, journal_entry_keys, journal_key, journal_keys, keys_under,
};

/// Asserts that a batch that reaches `batch_entries` is written at once, without waiting for
/// its linger, as one journal object that holds the entries in the order they were appended.
#[tokio::test(start_paused = true)]
async fn a_batch_is_written_at_once_when_it_reaches_its_size() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.batch_entries = 3;
    config.linger = Duration::from_secs(10);
    let proxy = rig.open(config).await.unwrap();

    let start = Instant::now();
    let (a, b, c) = tokio::join!(
        proxy.put("k1", body("1")),
        proxy.put("k2", body("2")),
        proxy.put("k3", body("3"))
    );
    a.unwrap();
    b.unwrap();
    c.unwrap();
    assert!(start.elapsed() < Duration::from_secs(10));
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, [journal_key("p1", 1)]);
    let entry_keys = journal_entry_keys(&*rig.hot_mem, "p1", 1).await;
    assert_eq!(entry_keys, ["k1", "k2", "k3"]);
}

/// Asserts that a batch that is not full is written after its linger, with the entries that
/// came meanwhile, and that an entry after that starts a batch of its own.
#[tokio::test(start_paused = true)]
async fn a_batch_waits_for_its_linger_and_takes_the_entries_that_come_meanwhile() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.batch_entries = 10;
    config.linger = Duration::from_millis(50);
    let proxy = rig.open(config).await.unwrap();

    let start = Instant::now();
    let (a, b) = tokio::join!(proxy.put("k1", body("1")), proxy.put("k2", body("2")));
    a.unwrap();
    b.unwrap();
    assert!(start.elapsed() >= Duration::from_millis(50));
    assert!(start.elapsed() < Duration::from_millis(60));
    let first = journal_entry_keys(&*rig.hot_mem, "p1", 1).await;
    assert_eq!(first, ["k1", "k2"]);

    proxy.put("k3", body("3")).await.unwrap();
    assert!(start.elapsed() >= Duration::from_millis(100));
    let second = journal_entry_keys(&*rig.hot_mem, "p1", 2).await;
    assert_eq!(second, ["k3"]);
}

/// Asserts that a put whose journal object cannot be written is not acknowledged: it returns
/// the error, the object it wrote to the hot tier is removed, no journal number is used up,
/// and the same key can be put once the journal can be written.
#[tokio::test(start_paused = true)]
async fn a_put_whose_journal_append_fails_is_not_acknowledged_and_its_object_removed() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let down = Error::Unavailable("journal".to_owned());
    rig.hot.fail_next_on(Op::Put, "_journal/", down.clone());

    let failed = proxy.put("k", body("value")).await;
    assert_eq!(failed, Err(down));
    assert_absent(&*rig.hot_mem, "k").await;
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, Vec::<String>::new());

    proxy.put("k", body("value")).await.unwrap();
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, [journal_key("p1", 1)]);
}

/// Asserts that the removal of the object of a put whose journal append failed is best
/// effort: when the hot tier fails that delete too, the put still returns the journal's
/// error, and the object stays in the hot tier, so a retry is refused as existing.
#[tokio::test(start_paused = true)]
async fn the_removal_after_a_failed_journal_append_is_best_effort() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let down = Error::Unavailable("journal".to_owned());
    rig.hot.fail_next_on(Op::Put, "_journal/", down.clone());
    let stuck = Error::Other("stuck".to_owned());
    rig.hot.fail_next_on(Op::Delete, "k", stuck);

    let failed = proxy.put("k", body("value")).await;
    assert_eq!(failed, Err(down));
    assert_eq!(rig.hot_mem.head("k").await.unwrap().size, 5);
    let retried = proxy.put("k", body("value")).await;
    assert_eq!(retried, Err(Error::AlreadyExists("k".to_owned())));
}

/// Asserts that a journal number that already holds an object (a write that failed after it
/// reached the hot tier) is skipped by the next batch, and that the numbers go on from there.
#[tokio::test(start_paused = true)]
async fn a_journal_number_that_is_taken_is_skipped() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let taken = journal_key("p1", 1);
    rig.hot_mem.put(&taken, encode_journal(&[])).await.unwrap();

    proxy.put("k", body("1")).await.unwrap();
    proxy.put("j", body("2")).await.unwrap();
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    let want: Vec<String> = (1..=3).map(|seq| journal_key("p1", seq)).collect();
    assert_eq!(objects, want);
    assert_eq!(journal_entry_keys(&*rig.hot_mem, "p1", 2).await, ["k"]);
    assert_eq!(journal_entry_keys(&*rig.hot_mem, "p1", 3).await, ["j"]);
}

/// Asserts that when the journal object of a batch cannot be written, every put that waited
/// on the batch fails with the error and removes its object.
#[tokio::test(start_paused = true)]
async fn a_failed_batch_fails_every_put_waiting_on_it() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.linger = Duration::from_millis(50);
    let proxy = rig.open(config).await.unwrap();
    let down = Error::Unavailable("journal".to_owned());
    rig.hot.fail_next_on(Op::Put, "_journal/", down.clone());

    let (a, b) = tokio::join!(proxy.put("a", body("1")), proxy.put("b", body("2")));
    assert_eq!(a, Err(down.clone()));
    assert_eq!(b, Err(down));
    assert_absent(&*rig.hot_mem, "a").await;
    assert_absent(&*rig.hot_mem, "b").await;
}

/// Asserts that when the caller that is writing a batch is dropped (its put timed out), a
/// caller that waits on the same batch writes it, so that the batch is not lost.
#[tokio::test(start_paused = true)]
async fn when_the_caller_writing_a_batch_is_dropped_another_writes_it() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.linger = Duration::from_millis(50);
    let proxy = rig.open(config).await.unwrap();

    let limit = Duration::from_millis(10);
    let first = tokio::time::timeout(limit, proxy.put("k1", body("1")));
    let (first, second) = tokio::join!(first, proxy.put("k2", body("2")));
    assert!(first.is_err());
    second.unwrap();
    let entry_keys = journal_entry_keys(&*rig.hot_mem, "p1", 1).await;
    assert_eq!(entry_keys, ["k1", "k2"]);
}

/// Asserts that a proxy that starts again with the same id lists only its own journal prefix
/// (not the prefix of an id that starts with its id), and goes on after the highest number it
/// finds there, so that no journal object is overwritten.
#[tokio::test(start_paused = true)]
async fn a_restarted_proxy_goes_on_after_the_highest_number_of_its_own_journal() {
    let rig = Rig::new();
    let other = rig.proxy("ab", 1000).await;
    for key in ["k1", "k2", "k3"] {
        other.put(key, body("v")).await.unwrap();
    }
    let proxy = rig.proxy("a", 1000).await;
    proxy.put("j1", body("v")).await.unwrap();
    drop(proxy);

    let proxy = rig.proxy("a", 1000).await;
    proxy.put("j2", body("v")).await.unwrap();
    let own = journal_keys(&*rig.hot_mem, "a").await;
    assert_eq!(own, [journal_key("a", 1), journal_key("a", 2)]);
    assert_eq!(journal_entry_keys(&*rig.hot_mem, "a", 1).await, ["j1"]);
    assert_eq!(journal_entry_keys(&*rig.hot_mem, "a", 2).await, ["j2"]);
    let others = journal_keys(&*rig.hot_mem, "ab").await;
    assert_eq!(others.len(), 3);
    let listed = rig.hot.listed_prefixes();
    assert_eq!(listed, ["_journal/ab/", "_journal/a/", "_journal/a/"]);
}

/// Asserts that a proxy whose journal objects were all deleted by the reconciler goes on after
/// the number its checkpoint says is applied, not from the start, since the reconciler would
/// ignore the numbers up to its checkpoint.
#[tokio::test(start_paused = true)]
async fn a_proxy_goes_on_after_its_checkpoint_when_its_journal_is_gone() {
    let rig = Rig::new();
    let checkpoint = r#"{"seq":10,"evict_seq":0,"evict_idx":0}"#;
    let key = "_tiering/checkpoint/p1";
    rig.hot_mem.put(key, body(checkpoint)).await.unwrap();
    let proxy = rig.proxy("p1", 1000).await;

    proxy.put("k", body("v")).await.unwrap();
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, [journal_key("p1", 11)]);
}

/// Asserts that two proxies over the same tiers keep separate journals, each numbered from 1,
/// and that each tells the reconciler about itself under its own name.
#[tokio::test(start_paused = true)]
async fn two_proxies_keep_separate_journals() {
    let rig = Rig::new();
    let one = rig.proxy("p1", 1000).await;
    let two = rig.proxy("p2", 1000).await;
    one.put("a", body("v")).await.unwrap();
    two.put("b", body("v")).await.unwrap();
    two.put("c", body("v")).await.unwrap();

    assert_eq!(journal_entry_keys(&*rig.hot_mem, "p1", 1).await, ["a"]);
    assert_eq!(journal_entry_keys(&*rig.hot_mem, "p2", 1).await, ["b"]);
    assert_eq!(journal_entry_keys(&*rig.hot_mem, "p2", 2).await, ["c"]);
    let announced = keys_under(&*rig.hot_mem, "_tiering/proxies/").await;
    assert_eq!(announced, ["_tiering/proxies/p1", "_tiering/proxies/p2"]);
}
