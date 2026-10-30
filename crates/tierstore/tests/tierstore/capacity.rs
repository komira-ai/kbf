//! The hot tier's capacity as the proxy sees it: back-pressure, a full hot tier, and the
//! usage the reconciler publishes.

use std::time::Duration;

use tierstore::{Backend, ByteRange, Error, Op};
use tokio::time::Instant;

use crate::support::{
    Rig, assert_absent, body, journal_entry_keys, journal_key, journal_keys, read, set_usage,
};

/// Asserts that a put is delayed by the back-pressure while the published usage is at or above
/// the critical share of the budget and under the budget, and is not delayed under it.
#[tokio::test(start_paused = true)]
async fn a_put_above_the_critical_mark_is_delayed_by_the_back_pressure() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.critical = 0.5;
    config.back_pressure = Duration::from_millis(200);
    let proxy = rig.open(config).await.unwrap();

    set_usage(&rig, 100, 0).await;
    let start = Instant::now();
    proxy.put("a", body("1")).await.unwrap();
    assert!(start.elapsed() < Duration::from_millis(100));

    set_usage(&rig, 600, 0).await;
    let start = Instant::now();
    proxy.put("b", body("2")).await.unwrap();
    assert!(start.elapsed() >= Duration::from_millis(200));
    assert_eq!(read(&*rig.hot_mem, "b").await, body("2"));
}

/// Asserts that while the published usage is at the budget new objects go straight to the cold
/// tier, create-only and unjournaled; that reads work from both tiers and copy nothing back;
/// that a put is still refused where either tier holds the key; and that the hot tier is used
/// again once the usage falls.
#[tokio::test(start_paused = true)]
async fn when_the_usage_is_at_the_budget_puts_go_straight_to_the_cold_tier() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("h", body("hot")).await.unwrap();
    set_usage(&rig, 1000, 0).await;

    proxy.put("k", body("direct")).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "k").await, body("direct"));
    assert_absent(&*rig.hot_mem, "k").await;
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, [journal_key("p1", 1)]);

    assert_eq!(proxy.get("k", None).await.unwrap(), body("direct"));
    assert_absent(&*rig.hot_mem, "k").await;
    assert_eq!(proxy.get("h", None).await.unwrap(), body("hot"));

    let in_cold = proxy.put("k", body("again")).await;
    assert_eq!(in_cold, Err(Error::AlreadyExists("k".to_owned())));
    let in_hot = proxy.put("h", body("again")).await;
    assert_eq!(in_hot, Err(Error::AlreadyExists("h".to_owned())));

    set_usage(&rig, 10, 0).await;
    proxy.put("m", body("back")).await.unwrap();
    assert_eq!(read(&*rig.hot_mem, "m").await, body("back"));
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, [journal_key("p1", 1), journal_key("p1", 2)]);
}

/// Asserts that a put made while the hot tier is full is an error when the hot tier cannot
/// answer whether it holds the key, and not an object written to the cold tier.
#[tokio::test(start_paused = true)]
async fn a_put_to_the_cold_tier_needs_the_hot_tier_to_say_it_lacks_the_key() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    set_usage(&rig, 1000, 0).await;
    let down = Error::Unavailable("hot".to_owned());
    rig.hot.fail_next_on(Op::Head, "k", down.clone());

    let failed = proxy.put("k", body("value")).await;
    assert_eq!(failed, Err(down));
    assert_absent(&*rig.cold_mem, "k").await;
}

/// Asserts that a usage object that is missing, that is not readable, or that is older than
/// `usage_stale_after` counts as not full (the put goes to the hot tier), and that a fresh
/// usage at the budget is believed.
#[tokio::test(start_paused = true)]
async fn a_usage_that_is_missing_unreadable_or_stale_counts_as_not_full() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("a", body("missing")).await.unwrap();

    let garbage = body("not json");
    rig.hot_mem.put("_tiering/usage", garbage).await.unwrap();
    proxy.put("b", body("unreadable")).await.unwrap();

    set_usage(&rig, 1000, 0).await;
    rig.clock.set(100_000);
    proxy.put("c", body("stale")).await.unwrap();
    for key in ["a", "b", "c"] {
        assert_absent(&*rig.cold_mem, key).await;
        assert!(rig.hot_mem.head(key).await.is_ok());
    }

    set_usage(&rig, 1000, 100_000).await;
    rig.clock.set(110_000);
    proxy.put("d", body("fresh")).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "d").await, body("fresh"));
    assert_absent(&*rig.hot_mem, "d").await;
}

/// Asserts that when the hot tier answers a put with a capacity error the object goes to the
/// cold tier instead, that the next puts go there without asking the hot tier, and that the
/// hot tier is tried again after `usage_refresh`.
#[tokio::test(start_paused = true)]
async fn a_capacity_error_from_the_hot_tier_sends_puts_to_cold_until_the_refresh() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.usage_refresh = Duration::from_secs(10);
    let proxy = rig.open(config).await.unwrap();
    let full = Error::Full("hot".to_owned());
    rig.hot.fail_next_on(Op::Put, "k1", full);

    proxy.put("k1", body("1")).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "k1").await, body("1"));
    assert_absent(&*rig.hot_mem, "k1").await;
    let hot_puts = rig.hot.calls(Op::Put);

    proxy.put("k2", body("2")).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "k2").await, body("2"));
    assert_eq!(rig.hot.calls(Op::Put), hot_puts);

    tokio::time::sleep(Duration::from_secs(11)).await;
    proxy.put("k3", body("3")).await.unwrap();
    assert_eq!(read(&*rig.hot_mem, "k3").await, body("3"));
    assert_absent(&*rig.cold_mem, "k3").await;
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, [journal_key("p1", 1)]);
    let entry_keys = journal_entry_keys(&*rig.hot_mem, "p1", 1).await;
    assert_eq!(entry_keys, ["k3"]);
}

/// Asserts that a put whose journal object the hot tier answers with a capacity error does
/// not fail: the object goes to the cold tier, the copy it first wrote to the hot tier is
/// removed, and nothing is journaled.
#[tokio::test(start_paused = true)]
async fn a_capacity_error_on_the_journal_object_sends_the_put_to_the_cold_tier() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let full = Error::Full("hot".to_owned());
    rig.hot.fail_next_on(Op::Put, "_journal/", full);

    proxy.put("k", body("value")).await.unwrap();
    assert_eq!(read(&*rig.cold_mem, "k").await, body("value"));
    assert_absent(&*rig.hot_mem, "k").await;
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, Vec::<String>::new());
}

/// Asserts that a get of an object only the cold tier holds does not copy it into the hot tier
/// while the usage is above the critical mark, and does once the usage is under it.
#[tokio::test(start_paused = true)]
async fn a_get_does_not_copy_back_above_the_critical_mark() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.critical = 0.5;
    let proxy = rig.open(config).await.unwrap();
    rig.cold_mem.put("k", body("value")).await.unwrap();

    set_usage(&rig, 600, 0).await;
    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_absent(&*rig.hot_mem, "k").await;
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, Vec::<String>::new());

    set_usage(&rig, 100, 0).await;
    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    assert_eq!(read(&*rig.hot_mem, "k").await, body("value"));
}

/// Asserts that a ranged get of an object only the cold tier holds asks the cold tier for the
/// range alone while the usage is above the critical mark (no copy is made, so a whole read
/// would only be thrown away), and for the whole object once the usage is under it.
#[tokio::test(start_paused = true)]
async fn a_ranged_get_above_the_critical_mark_reads_only_the_range_from_the_cold_tier() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.critical = 0.5;
    let proxy = rig.open(config).await.unwrap();
    rig.cold_mem.put("k", body("0123456789")).await.unwrap();

    set_usage(&rig, 600, 0).await;
    let part = ByteRange::new(2, 3);
    assert_eq!(proxy.get("k", Some(part)).await.unwrap(), body("234"));
    assert_eq!(rig.cold.get_ranges(), vec![Some((2, 3))]);
    assert_absent(&*rig.hot_mem, "k").await;

    set_usage(&rig, 100, 0).await;
    assert_eq!(proxy.get("k", Some(part)).await.unwrap(), body("234"));
    assert_eq!(rig.cold.get_ranges(), vec![Some((2, 3)), None]);
    assert_eq!(read(&*rig.hot_mem, "k").await, body("0123456789"));
}

/// Asserts that a read whose copy back into the hot tier fails (the hot tier is full, or
/// another read got there first) still returns the object, and journals nothing.
#[tokio::test(start_paused = true)]
async fn a_failed_copy_back_does_not_fail_the_read() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k1", body("one")).await.unwrap();
    rig.cold_mem.put("k2", body("two")).await.unwrap();
    let full = Error::Full("hot".to_owned());
    rig.hot.fail_next_on(Op::Put, "k1", full);
    let raced = Error::AlreadyExists("k2".to_owned());
    rig.hot.fail_next_on(Op::Put, "k2", raced);

    assert_eq!(proxy.get("k1", None).await.unwrap(), body("one"));
    assert_eq!(proxy.get("k2", None).await.unwrap(), body("two"));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_absent(&*rig.hot_mem, "k1").await;
    assert_absent(&*rig.hot_mem, "k2").await;
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, Vec::<String>::new());
}

/// Asserts that the usage object is read at most once for each `usage_refresh`: a change of
/// the usage is not seen by a put made before the interval has passed, and is seen by one
/// made after it.
#[tokio::test(start_paused = true)]
async fn the_usage_is_read_once_for_each_refresh_interval() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.usage_refresh = Duration::from_secs(10);
    let proxy = rig.open(config).await.unwrap();

    proxy.put("a", body("1")).await.unwrap();
    let reads = rig.hot.calls(Op::Get);
    set_usage(&rig, 1000, 0).await;
    proxy.put("b", body("2")).await.unwrap();
    assert_eq!(rig.hot.calls(Op::Get), reads);
    assert_eq!(read(&*rig.hot_mem, "b").await, body("2"));

    tokio::time::sleep(Duration::from_secs(11)).await;
    proxy.put("c", body("3")).await.unwrap();
    assert_eq!(rig.hot.calls(Op::Get), reads + 1);
    assert_eq!(read(&*rig.cold_mem, "c").await, body("3"));
    assert_absent(&*rig.hot_mem, "c").await;
}
