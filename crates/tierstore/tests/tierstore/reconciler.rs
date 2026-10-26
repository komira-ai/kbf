//! The reconciler: copies, deletes, eviction, checkpoints, restarts and pruning.

use std::time::Duration;

use tierstore::{Backend, Error, Op, PassReport};
use tokio::time::Instant;

use crate::support::{Rig, assert_absent, body, journal_key, journal_keys, keys, read, text};

fn count(prefixes: &[String], wanted: &str) -> usize {
    prefixes.iter().filter(|p| p.as_str() == wanted).count()
}

/// Asserts that one pass copies every journaled put to the cold tier, stores the checkpoint of
/// the proxy, and publishes the usage and the copy lag, which the getters give as well.
#[tokio::test(start_paused = true)]
async fn a_pass_copies_every_put_and_stores_the_checkpoint_and_usage() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("a", body("1")).await.unwrap();
    proxy.put("b", body("22")).await.unwrap();
    proxy.put("c", body("333")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);
    assert_eq!(reconciler.hot_bytes(), 0);

    let report = reconciler.pass().await;
    let want = PassReport {
        held: true,
        copied: 3,
        ..PassReport::default()
    };
    assert_eq!(report, want);
    assert_eq!(read(&*rig.cold_mem, "a").await, body("1"));
    assert_eq!(read(&*rig.cold_mem, "b").await, body("22"));
    assert_eq!(read(&*rig.cold_mem, "c").await, body("333"));
    let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert_eq!(stored, r#"{"seq":3,"evict_seq":0,"evict_idx":0}"#);
    let usage = text(&*rig.hot_mem, "_tiering/usage").await;
    assert_eq!(usage, r#"{"hot_bytes":6,"copy_lag":0,"time":0}"#);
    assert_eq!((reconciler.hot_bytes(), reconciler.copy_lag()), (6, 0));
}

/// Asserts that a pass with no proxy that has a journal holds the lease, does nothing, and
/// publishes a usage of zero.
#[tokio::test(start_paused = true)]
async fn a_pass_with_no_proxies_publishes_an_empty_usage() {
    let rig = Rig::new();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    let want = PassReport {
        held: true,
        ..PassReport::default()
    };
    assert_eq!(report, want);
    let usage = text(&*rig.hot_mem, "_tiering/usage").await;
    assert_eq!(usage, r#"{"hot_bytes":0,"copy_lag":0,"time":0}"#);
}

/// Asserts that the copies of one journal object run at most `parallel` at a time: with a
/// cold tier that takes 10 seconds for a put, four objects take 20 seconds when two copies
/// run at once and 10 seconds when four do.
#[tokio::test(start_paused = true)]
async fn copies_run_in_parallel_up_to_the_configured_number() {
    for (parallel, seconds) in [(2, 20), (4, 10)] {
        let rig = Rig::new();
        let mut config = rig.config("p1", 1000);
        config.batch_entries = 4;
        let proxy = rig.open(config).await.unwrap();
        let (a, b, c, d) = tokio::join!(
            proxy.put("a", body("1")),
            proxy.put("b", body("2")),
            proxy.put("c", body("3")),
            proxy.put("d", body("4"))
        );
        a.unwrap();
        b.unwrap();
        c.unwrap();
        d.unwrap();
        rig.cold.set_delay(Op::Put, Some(Duration::from_secs(10)));
        let mut settings = rig.reconciler_config("r1", 1000);
        settings.parallel = parallel;
        let mut reconciler = rig.reconciler_with(settings);

        let start = Instant::now();
        let report = reconciler.pass().await;
        assert_eq!(report.copied, 4);
        assert_eq!(start.elapsed(), Duration::from_secs(seconds));
    }
}

/// Asserts that a reconciler that dies in the middle of a batch (a copy failed, and the process
/// went without giving up its lease) is replaced, once the lease has expired, by one that
/// resumes from the stored checkpoint and copies the rest: no object is lost, and the
/// objects before the checkpoint are not copied again.
#[tokio::test(start_paused = true)]
async fn a_reconciler_that_dies_mid_batch_is_resumed_without_loss() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    for key in ["a", "b", "c", "d", "e"] {
        proxy.put(key, body(key)).await.unwrap();
    }
    let down = Error::Unavailable("cold".to_owned());
    rig.cold.fail_next_on(Op::Put, "c", down.clone());

    let mut first = rig.reconciler("r1", 1000);
    let report = first.pass().await;
    assert_eq!(report.copied, 2);
    assert_eq!(report.failed, [down]);
    assert_eq!(keys(&*rig.cold_mem).await, ["a", "b"]);
    let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert_eq!(stored, r#"{"seq":2,"evict_seq":0,"evict_idx":0}"#);
    drop(first);

    rig.clock.advance(31_000);
    let mut second = rig.reconciler("r2", 1000);
    let report = second.pass().await;
    assert!(report.held);
    assert_eq!(report.copied, 3);
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(keys(&*rig.cold_mem).await, ["a", "b", "c", "d", "e"]);
    assert_eq!(rig.cold.calls(Op::Put), 6);
}

/// Asserts that an object the cold tier holds already counts as copied, and that the cold
/// tier's object is left as it was.
#[tokio::test(start_paused = true)]
async fn an_object_the_cold_tier_holds_already_counts_as_copied() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("a", body("hot")).await.unwrap();
    rig.cold_mem.put("a", body("cold")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.copied, 1);
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(read(&*rig.cold_mem, "a").await, body("cold"));
    assert_eq!(reconciler.copy_lag(), 0);
}

/// Asserts that a key the hot tier no longer holds is skipped without a failure, is not
/// copied, and is not counted in the usage, also when the key was put several times and the
/// hot tier lost the last object; and that the checkpoint moves past it.
#[tokio::test(start_paused = true)]
async fn a_key_the_hot_tier_no_longer_holds_is_skipped() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("a", body("1")).await.unwrap();
    proxy.put("b", body("22")).await.unwrap();
    rig.hot_mem.delete("a").await.unwrap();
    proxy.put("c", body("3")).await.unwrap();
    proxy.delete("c").await.unwrap();
    proxy.put("c", body("4")).await.unwrap();
    rig.hot_mem.delete("c").await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.copied, 1);
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(keys(&*rig.cold_mem).await, ["b"]);
    assert_eq!((reconciler.hot_bytes(), reconciler.copy_lag()), (2, 0));
    let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert_eq!(stored, r#"{"seq":5,"evict_seq":0,"evict_idx":0}"#);
}

/// Asserts that a delete entry removes the cold copy that a copy under way made after the
/// delete: the object is deleted while the cold tier takes 10 seconds to store the copy,
/// the copy lands, and the next pass applies the delete entry and removes it, so the object is
/// not resurrected.
#[tokio::test(start_paused = true)]
async fn a_delete_entry_removes_a_cold_copy_made_while_the_delete_ran() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("value")).await.unwrap();
    rig.cold.set_delay(Op::Put, Some(Duration::from_secs(10)));
    let mut reconciler = rig.reconciler("r1", 1000);

    let delete = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        proxy.delete("k").await.unwrap();
    };
    let (report, ()) = tokio::join!(reconciler.pass(), delete);
    assert_eq!(report.copied, 1);
    assert_eq!(read(&*rig.cold_mem, "k").await, body("value"));
    assert_absent(&*rig.hot_mem, "k").await;

    rig.cold.set_delay(Op::Put, None);
    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_absent(&*rig.cold_mem, "k").await;
    let missing = Error::NotFound("k".to_owned());
    assert_eq!(proxy.get("k", None).await.unwrap_err(), missing);
    assert_eq!(reconciler.hot_bytes(), 0);
}

/// Asserts that a delete entry does not remove the cold copy of a key that was put again: the
/// hot tier holds the key, so the cold tier's copy is of the new object.
#[tokio::test(start_paused = true)]
async fn a_delete_entry_keeps_the_cold_copy_of_a_key_that_was_put_again() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("old")).await.unwrap();
    proxy.delete("k").await.unwrap();
    proxy.put("k", body("new")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.copied, 2);
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(read(&*rig.cold_mem, "k").await, body("new"));
    assert_eq!(read(&*rig.hot_mem, "k").await, body("new"));
    assert_eq!((reconciler.hot_bytes(), reconciler.copy_lag()), (3, 0));
}

/// Asserts that a delete entry of a key that was put again, which the cold tier holds no copy
/// of, changes nothing in the cold tier, and that the entry that put the key again copies the
/// new object.
#[tokio::test(start_paused = true)]
async fn a_delete_entry_of_a_key_the_cold_tier_lacks_leaves_the_cold_tier_alone() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("old")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);
    reconciler.pass().await;
    proxy.delete("k").await.unwrap();
    proxy.put("k", body("new")).await.unwrap();

    let report = reconciler.pass().await;
    assert_eq!(report.copied, 1);
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(read(&*rig.cold_mem, "k").await, body("new"));
    assert_eq!((reconciler.hot_bytes(), reconciler.copy_lag()), (3, 0));
}

/// Asserts that a journal entry with an operation this reader does not know (one that a newer
/// writer adds) is skipped: the entries around it are applied, nothing is reported, and the
/// checkpoint moves past the journal object.
#[tokio::test(start_paused = true)]
async fn a_journal_entry_with_an_unknown_operation_is_skipped() {
    let rig = Rig::new();
    let _proxy = rig.proxy("p1", 1000).await;
    rig.hot_mem.put("a", body("1")).await.unwrap();
    rig.hot_mem.put("x", body("22")).await.unwrap();
    let unknown = r#"{"op":"FUTURE","key":"x","size":2,"time":0}"#;
    let put = r#"{"op":"PUT","key":"a","size":1,"time":0}"#;
    let key = journal_key("p1", 1);
    let entries = format!("[{unknown},{put}]");
    rig.hot_mem.put(&key, body(&entries)).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);

    let report = reconciler.pass().await;
    assert_eq!(report.copied, 1);
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(keys(&*rig.cold_mem).await, ["a"]);
    assert_eq!((reconciler.hot_bytes(), reconciler.copy_lag()), (1, 0));
    let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert_eq!(stored, r#"{"seq":1,"evict_seq":0,"evict_idx":0}"#);
}

/// Asserts that above the high-water mark the reconciler evicts copied objects oldest first,
/// down to the low-water mark and no further, that the evicted objects are in the cold tier
/// and still readable, and that the checkpoint records how far the eviction went.
#[tokio::test(start_paused = true)]
async fn above_the_high_water_mark_the_oldest_objects_are_evicted_to_the_low_water() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let value = "x".repeat(21);
    for key in ["k1", "k2", "k3", "k4", "k5"] {
        rig.clock.advance(10);
        proxy.put(key, body(&value)).await.unwrap();
    }
    let mut reconciler = rig.reconciler("r1", 100);

    let report = reconciler.pass().await;
    assert_eq!(report.evicted, 2);
    assert_absent(&*rig.hot_mem, "k1").await;
    assert_absent(&*rig.hot_mem, "k2").await;
    for key in ["k3", "k4", "k5"] {
        assert_eq!(read(&*rig.hot_mem, key).await, body(&value));
    }
    assert_eq!(keys(&*rig.cold_mem).await.len(), 5);
    assert_eq!((reconciler.hot_bytes(), reconciler.copy_lag()), (63, 0));
    let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert_eq!(stored, r#"{"seq":5,"evict_seq":3,"evict_idx":0}"#);
    assert_eq!(proxy.get("k1", None).await.unwrap(), body(&value));
}

/// Asserts that nothing is evicted while the usage is not above the high-water mark.
#[tokio::test(start_paused = true)]
async fn nothing_is_evicted_while_the_usage_is_not_above_the_high_water_mark() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let value = "x".repeat(21);
    for key in ["k1", "k2", "k3"] {
        proxy.put(key, body(&value)).await.unwrap();
    }
    let mut reconciler = rig.reconciler("r1", 100);

    let report = reconciler.pass().await;
    assert_eq!(report.evicted, 0);
    assert_eq!(report.copied, 3);
    for key in ["k1", "k2", "k3"] {
        assert_eq!(read(&*rig.hot_mem, key).await, body(&value));
    }
}

/// Asserts that an object the cold tier does not hold is never evicted: with the copy of the
/// second object failing, only the first is evicted and the hot tier stays above the low-water
/// mark; once the copies run, the rest is evicted down to it.
#[tokio::test(start_paused = true)]
async fn an_object_the_cold_tier_does_not_hold_is_never_evicted() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let value = "x".repeat(21);
    for key in ["k1", "k2", "k3", "k4", "k5"] {
        proxy.put(key, body(&value)).await.unwrap();
    }
    let down = Error::Unavailable("cold".to_owned());
    rig.cold.fail_next_on(Op::Put, "k2", down);
    let mut reconciler = rig.reconciler("r1", 100);

    let report = reconciler.pass().await;
    assert_eq!(
        (report.copied, report.evicted, report.failed.len()),
        (1, 1, 1)
    );
    assert_absent(&*rig.hot_mem, "k1").await;
    for key in ["k2", "k3", "k4", "k5"] {
        assert_eq!(read(&*rig.hot_mem, key).await, body(&value));
    }
    assert_eq!((reconciler.hot_bytes(), reconciler.copy_lag()), (84, 84));

    let report = reconciler.pass().await;
    assert_eq!((report.copied, report.evicted), (4, 1));
    assert_absent(&*rig.hot_mem, "k2").await;
    assert_eq!((reconciler.hot_bytes(), reconciler.copy_lag()), (63, 0));
}

/// Asserts that across two proxies the objects are evicted oldest first by the time of their
/// journal entries, not proxy by proxy.
#[tokio::test(start_paused = true)]
async fn eviction_takes_the_oldest_entry_of_any_proxy_first() {
    let rig = Rig::new();
    let one = rig.proxy("p1", 1000).await;
    let two = rig.proxy("p2", 1000).await;
    let value = "x".repeat(30);
    rig.clock.set(10);
    one.put("a", body(&value)).await.unwrap();
    rig.clock.set(5);
    two.put("b", body(&value)).await.unwrap();
    rig.clock.set(20);
    one.put("c", body(&value)).await.unwrap();
    rig.clock.set(15);
    two.put("d", body(&value)).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 100);

    let report = reconciler.pass().await;
    assert_eq!(report.evicted, 2);
    assert_absent(&*rig.hot_mem, "a").await;
    assert_absent(&*rig.hot_mem, "b").await;
    assert_eq!(read(&*rig.hot_mem, "c").await, body(&value));
    assert_eq!(read(&*rig.hot_mem, "d").await, body(&value));
    assert_eq!(reconciler.hot_bytes(), 60);
    let first = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    let second = text(&*rig.hot_mem, "_tiering/checkpoint/p2").await;
    assert_eq!(first, r#"{"seq":2,"evict_seq":2,"evict_idx":0}"#);
    assert_eq!(second, first);
}

/// Asserts that a proxy none of whose objects is copied yet has nothing to evict, so the
/// eviction takes the objects of the other proxy.
#[tokio::test(start_paused = true)]
async fn a_proxy_with_nothing_copied_is_passed_over_by_the_eviction() {
    let rig = Rig::new();
    let one = rig.proxy("p1", 1000).await;
    let two = rig.proxy("p2", 1000).await;
    let value = "y".repeat(31);
    for key in ["a", "b", "c", "d"] {
        one.put(key, body(&value)).await.unwrap();
    }
    two.put("z", body("zzzzzzzzzz")).await.unwrap();
    let down = Error::Unavailable("cold".to_owned());
    rig.cold.fail_next_on(Op::Put, "z", down);
    let mut reconciler = rig.reconciler("r1", 100);

    let report = reconciler.pass().await;
    assert_eq!(report.evicted, 3);
    for key in ["a", "b", "c"] {
        assert_absent(&*rig.hot_mem, key).await;
    }
    assert_eq!(read(&*rig.hot_mem, "d").await, body(&value));
    assert_eq!(read(&*rig.hot_mem, "z").await, body("zzzzzzzzzz"));
    assert_eq!(reconciler.hot_bytes(), 41);
    assert_eq!(reconciler.copy_lag(), 10);
}

/// Asserts that a restarted reconciler rebuilds the usage and the copy lag from the stored
/// checkpoints and the journal objects: the objects evicted before the restart (two of the
/// three entries of one journal object) are not counted again, and the objects put since are
/// counted as not copied.
#[tokio::test(start_paused = true)]
async fn a_restarted_reconciler_rebuilds_the_usage_and_the_copy_lag() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.batch_entries = 3;
    let proxy = rig.open(config).await.unwrap();
    let value = "x".repeat(21);
    let (a, b, c) = tokio::join!(
        proxy.put("a", body(&value)),
        proxy.put("b", body(&value)),
        proxy.put("c", body(&value))
    );
    a.unwrap();
    b.unwrap();
    c.unwrap();
    proxy.put("d", body(&value)).await.unwrap();
    proxy.put("e", body(&value)).await.unwrap();

    let mut first = rig.reconciler("r1", 100);
    let report = first.pass().await;
    assert_eq!(report.evicted, 2);
    assert_eq!((first.hot_bytes(), first.copy_lag()), (63, 0));
    let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert_eq!(stored, r#"{"seq":3,"evict_seq":1,"evict_idx":2}"#);
    first.stop().await;

    proxy.put("f", body("12345")).await.unwrap();
    proxy.put("g", body("12345")).await.unwrap();
    let down = Error::Unavailable("cold".to_owned());
    rig.cold.fail_next_on(Op::Put, "f", down);
    let mut second = rig.reconciler("r2", 100);
    let report = second.pass().await;
    assert_eq!(
        (report.copied, report.evicted, report.failed.len()),
        (0, 0, 1)
    );
    assert_eq!((second.hot_bytes(), second.copy_lag()), (73, 10));
    let usage = text(&*rig.hot_mem, "_tiering/usage").await;
    assert_eq!(usage, r#"{"hot_bytes":73,"copy_lag":10,"time":0}"#);
}

/// Asserts that a journal object is deleted only when the stored checkpoint has applied it
/// and the eviction is past its entries, and then only once it is older than the retention.
#[tokio::test(start_paused = true)]
async fn journal_objects_are_deleted_once_done_with_and_old_enough() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let value = "x".repeat(21);
    for key in ["k1", "k2", "k3", "k4", "k5"] {
        proxy.put(key, body(&value)).await.unwrap();
    }
    let mut settings = rig.reconciler_config("r1", 100);
    settings.retention = Duration::from_millis(1000);
    let mut reconciler = rig.reconciler_with(settings);

    let report = reconciler.pass().await;
    assert_eq!((report.evicted, report.pruned), (2, 0));
    assert_eq!(journal_keys(&*rig.hot_mem, "p1").await.len(), 5);

    rig.clock.set(5000);
    let report = reconciler.pass().await;
    assert_eq!((report.evicted, report.pruned), (0, 2));
    let left = journal_keys(&*rig.hot_mem, "p1").await;
    let want: Vec<String> = (3..=5).map(|seq| journal_key("p1", seq)).collect();
    assert_eq!(left, want);
}

/// Asserts that an entry that a later one replaced never evicts the object: a key deleted and
/// put again keeps its new object in the hot tier when the turn of its old entry comes.
#[tokio::test(start_paused = true)]
async fn an_object_put_again_is_not_evicted_by_its_old_entry() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let value = "x".repeat(10);
    let newer = "y".repeat(10);
    for key in ["a", "k", "b"] {
        proxy.put(key, body(&value)).await.unwrap();
    }
    let mut reconciler = rig.reconciler("r1", 40);
    assert_eq!(reconciler.pass().await.evicted, 0);

    proxy.delete("k").await.unwrap();
    proxy.put("k", body(&newer)).await.unwrap();
    proxy.put("c", body(&value)).await.unwrap();
    let report = reconciler.pass().await;
    assert_eq!(report.evicted, 2);
    assert_absent(&*rig.hot_mem, "a").await;
    assert_absent(&*rig.hot_mem, "b").await;
    assert_eq!(read(&*rig.hot_mem, "k").await, body(&newer));
    assert_eq!(read(&*rig.hot_mem, "c").await, body(&value));
    assert_eq!(read(&*rig.cold_mem, "k").await, body(&newer));
}

/// Asserts that an object that a proxy deletes and puts again while the reconciler is copying
/// is not evicted by its old entry: the pass reads the journals again before it evicts, so the
/// new object stays in the hot tier. And that the next pass replaces the cold tier's copy of
/// the first object, which landed after the delete, with the new object.
#[tokio::test(start_paused = true)]
async fn an_object_replaced_while_the_reconciler_copies_is_not_evicted() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let value = "x".repeat(21);
    let newer = "y".repeat(21);
    for key in ["k1", "k2", "k3", "k4", "k5"] {
        proxy.put(key, body(&value)).await.unwrap();
    }
    rig.cold.set_delay(Op::Put, Some(Duration::from_secs(10)));
    let mut reconciler = rig.reconciler("r1", 100);

    let replace = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        proxy.delete("k1").await.unwrap();
        proxy.put("k1", body(&newer)).await.unwrap();
    };
    let (report, ()) = tokio::join!(reconciler.pass(), replace);
    assert_eq!((report.copied, report.evicted), (5, 2));
    assert_eq!(read(&*rig.hot_mem, "k1").await, body(&newer));
    assert_absent(&*rig.hot_mem, "k2").await;
    assert_absent(&*rig.hot_mem, "k3").await;
    assert_eq!(read(&*rig.cold_mem, "k1").await, body(&value));

    rig.cold.set_delay(Op::Put, None);
    let report = reconciler.pass().await;
    assert_eq!(report.failed, Vec::<Error>::new());
    assert_eq!(report.copied, 1);
    assert_eq!(read(&*rig.cold_mem, "k1").await, body(&newer));
    assert_eq!(read(&*rig.hot_mem, "k1").await, body(&newer));
}

/// Asserts that the entry of an object that was deleted does not hold the cursor back: the
/// checkpoint moves past it to the next object the hot tier may drop.
#[tokio::test(start_paused = true)]
async fn the_cursor_moves_past_the_entries_of_deleted_objects() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("a", body("1")).await.unwrap();
    proxy.put("b", body("2")).await.unwrap();
    let mut reconciler = rig.reconciler("r1", 1000);
    reconciler.pass().await;

    proxy.delete("a").await.unwrap();
    reconciler.pass().await;
    let stored = text(&*rig.hot_mem, "_tiering/checkpoint/p1").await;
    assert_eq!(stored, r#"{"seq":3,"evict_seq":2,"evict_idx":0}"#);
    assert_eq!(reconciler.hot_bytes(), 1);
}

/// Asserts that an object a get copied back into the hot tier is journaled as a put, so the
/// reconciler counts it, finds it in the cold tier, and evicts it again.
#[tokio::test(start_paused = true)]
async fn an_object_copied_back_by_a_get_is_evicted_again() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let value = "x".repeat(21);
    rig.cold_mem.put("k", body(&value)).await.unwrap();
    assert_eq!(proxy.get("k", None).await.unwrap(), body(&value));
    tokio::time::sleep(Duration::from_millis(10)).await;
    let mut reconciler = rig.reconciler("r1", 10);

    let report = reconciler.pass().await;
    assert_eq!((report.copied, report.evicted), (1, 1));
    assert_absent(&*rig.hot_mem, "k").await;
    assert_eq!(read(&*rig.cold_mem, "k").await, body(&value));
    assert_eq!(reconciler.hot_bytes(), 0);
}

/// Asserts that a pass lists nothing but `_tiering/proxies/` and the journal prefix of each
/// proxy in the hot tier (above the last object it read, a page at a time, and once more
/// before it evicts), and nothing in the cold tier, even when it copies, evicts and prunes.
#[tokio::test(start_paused = true)]
async fn a_pass_never_lists_the_data_prefixes() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let value = "x".repeat(21);
    for key in ["k1", "k2", "k3", "k4", "k5"] {
        proxy.put(key, body(&value)).await.unwrap();
    }
    let mut reconciler = rig.reconciler("r1", 100);
    assert_eq!(reconciler.pass().await.evicted, 2);
    assert_eq!(reconciler.pass().await.evicted, 0);

    let prefixes = rig.hot.listed_prefixes();
    assert_eq!(count(&prefixes, "_journal/p1/"), 4);
    assert_eq!(count(&prefixes, "_tiering/proxies/"), 2);
    assert_eq!(prefixes.len(), 6);
    assert_eq!(rig.cold.calls(Op::List), 0);
}

/// Asserts that a pass reads a long journal a page at a time, `page_size` journal objects
/// for each listing, and copies the objects of all the pages.
#[tokio::test(start_paused = true)]
async fn a_pass_reads_a_long_journal_a_page_at_a_time() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    for key in ["a", "b", "c", "d", "e"] {
        proxy.put(key, body(key)).await.unwrap();
    }
    let mut settings = rig.reconciler_config("r1", 1000);
    settings.page_size = 2;
    let mut reconciler = rig.reconciler_with(settings);

    let report = reconciler.pass().await;
    assert_eq!(report.copied, 5);
    assert_eq!(rig.hot.calls(Op::List), 5);
}
