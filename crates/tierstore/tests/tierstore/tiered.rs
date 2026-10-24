//! The proxy: puts, gets, heads, deletes and listings over the two tiers.

use std::time::Duration;

use tierstore::{Backend, Error, JournalEntry, JournalOp, Op};

use crate::support::{
    Rig, assert_absent, body, check_ranges, journal, journal_key, journal_keys, keys, page, read,
};

fn entry(op: JournalOp, key: &str, size: u64, time: u64) -> JournalEntry {
    JournalEntry {
        op,
        key: key.to_owned(),
        size,
        time,
    }
}

/// Asserts that a put is acknowledged after the journal object that holds its entry is in the
/// hot tier: when the put returns, the object is in the hot tier and not in the cold tier,
/// and journal object 1 of the proxy holds one put entry with the key, the size and the time.
#[tokio::test(start_paused = true)]
async fn a_put_is_acknowledged_after_its_journal_entry_is_in_the_hot_tier() {
    let rig = Rig::new();
    rig.clock.set(42);
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("value")).await.unwrap();

    assert_eq!(read(&*rig.hot_mem, "k").await, body("value"));
    assert_absent(&*rig.cold_mem, "k").await;
    let entries = journal(&*rig.hot_mem, "p1", 1).await;
    assert_eq!(entries, [entry(JournalOp::Put, "k", 5, 42)]);
}

/// Asserts that a put is refused when the key exists only in the cold tier (the hot tier may
/// have evicted it): the error is `AlreadyExists`, the hot tier gets no object, and nothing is
/// journaled.
#[tokio::test(start_paused = true)]
async fn a_put_is_refused_when_the_key_is_only_in_the_cold_tier() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("old")).await.unwrap();

    let refused = proxy.put("k", body("new")).await;
    assert_eq!(refused, Err(Error::AlreadyExists("k".to_owned())));
    assert_absent(&*rig.hot_mem, "k").await;
    assert_eq!(read(&*rig.cold_mem, "k").await, body("old"));
    let journal_objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(journal_objects, Vec::<String>::new());
}

/// Asserts that a put is refused when the key exists in the hot tier, that the first object
/// is kept, and that the refused put adds nothing to the journal.
#[tokio::test(start_paused = true)]
async fn a_put_is_refused_when_the_key_is_in_the_hot_tier() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("one")).await.unwrap();

    let refused = proxy.put("k", body("two")).await;
    assert_eq!(refused, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(proxy.get("k", None).await.unwrap(), body("one"));
    let journal_objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(journal_objects, [journal_key("p1", 1)]);
}

/// Asserts that a get of an object in the hot tier is answered by the hot tier alone.
#[tokio::test(start_paused = true)]
async fn a_get_of_a_hot_object_does_not_touch_the_cold_tier() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("value")).await.unwrap();

    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    assert_eq!(proxy.head("k").await.unwrap().size, 5);
    assert_eq!(rig.cold.calls(Op::Get), 0);
    assert_eq!(rig.cold.calls(Op::Head), 1);
}

/// Asserts that a get of an object only the cold tier holds returns it and copies it back into
/// the hot tier, journaling the copy as a put, so that a second get is answered by the hot
/// tier; and that a head of such an object copies nothing back.
#[tokio::test(start_paused = true)]
async fn a_get_copies_a_cold_object_back_and_a_head_does_not() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("value")).await.unwrap();

    assert_eq!(proxy.head("k").await.unwrap().size, 5);
    assert_absent(&*rig.hot_mem, "k").await;

    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(read(&*rig.hot_mem, "k").await, body("value"));
    let entries = journal(&*rig.hot_mem, "p1", 1).await;
    assert_eq!(entries, [entry(JournalOp::Put, "k", 5, 0)]);

    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    assert_eq!(rig.cold.calls(Op::Get), 1);
}

/// Asserts that range reads are byte-exact from the hot tier, and again from an object only
/// the cold tier holds (the first read of it copies the whole object back): the first and last
/// byte, a middle slice, a range past the end, empty ranges, and an error for a range
/// starting past the end.
#[tokio::test(start_paused = true)]
async fn range_reads_are_byte_exact_at_both_tiers() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("hot", body("0123456789")).await.unwrap();
    check_ranges(&proxy, "hot").await;

    rig.cold_mem.put("cold", body("0123456789")).await.unwrap();
    check_ranges(&proxy, "cold").await;
    assert_eq!(read(&*rig.hot_mem, "cold").await, body("0123456789"));
}

/// Asserts that a get and a head of an object neither tier holds are `NotFound`.
#[tokio::test(start_paused = true)]
async fn an_object_neither_tier_holds_is_not_found() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let missing = Error::NotFound("k".to_owned());
    assert_eq!(proxy.get("k", None).await.unwrap_err(), missing);
    assert_eq!(proxy.head("k").await.unwrap_err(), missing);
}

/// Asserts that a delete removes the object from both tiers and appends a delete entry with
/// size zero, that the key is then not found, and that deleting a key neither tier holds
/// succeeds and is journaled too.
#[tokio::test(start_paused = true)]
async fn a_delete_removes_both_copies_and_is_journaled() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("value")).await.unwrap();
    rig.cold_mem.put("k", body("value")).await.unwrap();

    proxy.delete("k").await.unwrap();
    assert_absent(&*rig.hot_mem, "k").await;
    assert_absent(&*rig.cold_mem, "k").await;
    let missing = Error::NotFound("k".to_owned());
    assert_eq!(proxy.get("k", None).await.unwrap_err(), missing);
    let entries = journal(&*rig.hot_mem, "p1", 2).await;
    assert_eq!(entries, [entry(JournalOp::Delete, "k", 0, 0)]);

    proxy.delete("k").await.unwrap();
    let again = journal(&*rig.hot_mem, "p1", 3).await;
    assert_eq!(again, [entry(JournalOp::Delete, "k", 0, 0)]);
}

/// Asserts that a client put or delete of a key under `_journal/` or `_tiering/` is refused
/// with `Reserved` and changes nothing, and that the proxy does not replace objects.
#[tokio::test(start_paused = true)]
async fn a_client_write_under_a_reserved_prefix_is_refused() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    for key in ["_journal/p1/x", "_tiering/lease", "_tiering/usage"] {
        let put = proxy.put(key, body("v")).await;
        assert_eq!(put, Err(Error::Reserved(key.to_owned())));
        let delete = proxy.delete(key).await;
        assert_eq!(delete, Err(Error::Reserved(key.to_owned())));
        assert_absent(&*rig.hot_mem, key).await;
    }
    let journal_objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(journal_objects, Vec::<String>::new());

    let replaced = proxy.put_if_match("k", body("v"), "tag").await;
    assert!(matches!(replaced, Err(Error::Other(_))));
}

/// Asserts that a listing is the union of both tiers without duplicates, in key order, with
/// the prefix applied to both, the hot tier's entry where both have the key, and without the
/// keys under the reserved prefixes.
#[tokio::test(start_paused = true)]
async fn a_listing_merges_the_tiers_and_leaves_out_the_reserved_keys() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("x/1", body("one")).await.unwrap();
    proxy.put("x/3", body("three")).await.unwrap();
    rig.cold_mem.put("x/2", body("two")).await.unwrap();
    rig.cold_mem.put("x/3", body("3")).await.unwrap();
    rig.cold_mem.put("y/1", body("other")).await.unwrap();

    assert_eq!(keys(&proxy).await, ["x/1", "x/2", "x/3", "y/1"]);
    let listed = proxy.list("x/").await.unwrap();
    let sizes: Vec<u64> = listed.iter().map(|object| object.size).collect();
    assert_eq!(sizes, [3, 3, 5]);
    let names: Vec<&str> = listed.iter().map(|object| object.key.as_str()).collect();
    assert_eq!(names, ["x/1", "x/2", "x/3"]);
    assert_eq!(keys(&*rig.hot_mem).await.len(), 5);
}

/// Asserts that pages of the merged listing continue after the last key of the page before,
/// are cut at `max_keys` after the reserved keys of the hot tier (which sort before the
/// lowercase keys) are left out, and that a page shorter than `max_keys` is the last.
#[tokio::test(start_paused = true)]
async fn pages_of_the_merged_listing_are_continued_by_the_last_key() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    for key in ["a", "b", "c"] {
        proxy.put(key, body("v")).await.unwrap();
    }
    rig.cold_mem.put("d", body("v")).await.unwrap();
    rig.cold_mem.put("e", body("v")).await.unwrap();

    assert_eq!(page(&proxy, None, 2).await, ["a", "b"]);
    assert_eq!(page(&proxy, Some("b"), 2).await, ["c", "d"]);
    assert_eq!(page(&proxy, Some("d"), 2).await, ["e"]);
    assert_eq!(page(&proxy, Some("e"), 2).await, Vec::<String>::new());
    assert_eq!(page(&proxy, None, 0).await, Vec::<String>::new());
}

/// Asserts that a proxy is not started with an id that is empty or has a slash, and that it
/// reads nothing from the hot tier before it refuses.
#[tokio::test(start_paused = true)]
async fn a_proxy_id_must_be_a_name_without_a_slash() {
    let rig = Rig::new();
    for id in ["", "a/b"] {
        let opened = rig.open(rig.config(id, 10)).await;
        assert!(matches!(opened.err(), Some(Error::Other(_))));
    }
    assert_eq!(rig.hot.calls(Op::List), 0);
}
