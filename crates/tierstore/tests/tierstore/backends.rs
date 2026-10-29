//! The in-memory backend, the fault injector and the values they speak in.

use std::sync::Arc;
use std::time::Duration;

use tierstore::{
    Backend, ByteRange, Clock, Error, FaultyBackend, JournalEntry, JournalOp, MemoryBackend, Op,
    SystemClock, TestClock, decode_journal, encode_journal,
};

use crate::conformance;
use crate::support::{body, chunks, keys};

/// Asserts that a second put of a key is refused with `AlreadyExists` and leaves the first
/// object's bytes in place, on two separate backends (the hot and the cold tier are two).
#[tokio::test]
async fn a_second_put_of_a_key_is_refused_and_changes_nothing() {
    let first = MemoryBackend::new();
    let second = MemoryBackend::new();
    for backend in [&first, &second] {
        conformance::a_second_put_is_refused(backend).await;
    }
}

/// Asserts that ranges are byte-exact on a backend: the first byte, the last byte, a slice
/// in the middle, a range running past the end (cut at the end, even when its end overflows
/// a number), empty ranges inside the object and at its end, and an error for a range
/// starting past the end.
#[tokio::test]
async fn ranges_are_byte_exact() {
    conformance::ranges_are_byte_exact(&MemoryBackend::new()).await;
}

/// Asserts that a range reports the offset and the size it was made with.
#[test]
fn a_range_reports_what_it_was_made_with() {
    let range = ByteRange::new(3, 4);
    assert_eq!(range.offset(), 3);
    assert_eq!(range.size(), 4);
}

/// Asserts that head gives the size and a content tag that is equal for equal bytes and
/// different for different bytes of the same size, and that `same_content` compares the two.
#[tokio::test]
async fn head_gives_the_size_and_a_tag_of_the_content() {
    let backend = MemoryBackend::new();
    conformance::head_gives_the_size_and_a_tag_of_the_content(&backend).await;
}

/// Asserts that head gives the time of last write that the clock of the backend said when the
/// object was put, and that `same_content` compares the content and not those times.
#[tokio::test]
async fn head_gives_the_time_of_last_write_and_same_content_ignores_it() {
    let clock = Arc::new(TestClock::new());
    let backend = MemoryBackend::with_clock(clock.clone());
    backend.put("a", body("same")).await.unwrap();
    clock.set(7);
    backend.put("b", body("same")).await.unwrap();
    let a = backend.head("a").await.unwrap();
    let b = backend.head("b").await.unwrap();
    assert!(a.same_content(&b));
    assert_eq!((a.modified, b.modified), (0, 7));
}

/// Asserts that a listing gives the time of last write of each object, as the clock of the
/// backend said when the object was put, and that the clock moves by what it is told.
#[tokio::test]
async fn a_listing_gives_the_times_of_last_write() {
    let clock = Arc::new(TestClock::new());
    let backend = MemoryBackend::with_clock(clock.clone());
    backend.put("a", body("1")).await.unwrap();
    clock.advance(5);
    clock.advance(4);
    backend.put("b", body("2")).await.unwrap();
    let listed = backend.list("").await.unwrap();
    let times: Vec<u64> = listed.iter().map(|object| object.modified).collect();
    assert_eq!(times, [0, 9]);
    assert_eq!(backend.clock().now(), 9);
}

/// Asserts that reading, heading or listing nothing gives `NotFound` or an empty listing,
/// and that deleting a key that is not there succeeds.
#[tokio::test]
async fn an_absent_key_is_not_found_and_deleting_it_succeeds() {
    let backend = MemoryBackend::new();
    conformance::an_absent_key_is_not_found_and_deleting_it_succeeds(&backend).await;
}

/// Asserts that a delete removes the object and that a deleted key can be put again.
#[tokio::test]
async fn a_deleted_key_is_gone_and_can_be_put_again() {
    let backend = MemoryBackend::new();
    conformance::a_deleted_key_is_gone_and_can_be_put_again(&backend).await;
}

/// Asserts that a listing holds the keys under the prefix, in key order, with their sizes.
#[tokio::test]
async fn a_listing_holds_the_keys_under_a_prefix_in_order() {
    let backend = MemoryBackend::new();
    conformance::a_listing_holds_the_keys_under_a_prefix_in_order(&backend).await;
}

/// Asserts that a page of a listing holds at most `max_keys` keys that sort after the
/// start-after key, in key order, that a page shorter than `max_keys` is the last, and that
/// a page of none holds none.
#[tokio::test]
async fn a_page_of_a_listing_starts_after_a_key_and_is_cut_at_max_keys() {
    let backend = MemoryBackend::new();
    conformance::a_page_starts_after_a_key_and_is_cut_at_max_keys(&backend).await;
}

/// Asserts that a put with the content tag of the object replaces it and gives the new
/// object's content tag; that a put with another tag, or with the tag the object had before,
/// is refused and changes nothing; and that a put over an absent key is `NotFound`.
#[tokio::test]
async fn a_put_if_match_replaces_only_the_object_with_that_tag() {
    let backend = MemoryBackend::new();
    conformance::a_put_if_match_replaces_only_the_object_with_that_tag(&backend).await;
}

/// Asserts that a put that replaces moves the time of last write on, and that a put that is
/// refused does not.
#[tokio::test]
async fn a_put_if_match_moves_the_time_of_last_write_only_when_it_replaces() {
    let clock = Arc::new(TestClock::new());
    let backend = MemoryBackend::with_clock(clock.clone());
    backend.put("k", body("one")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;

    clock.set(9);
    let wrong = backend.put_if_match("k", body("x"), "not-the-tag").await;
    assert!(wrong.is_err());
    assert_eq!(backend.head("k").await.unwrap().modified, 0);

    let swap = backend.put_if_match("k", body("two"), &tag);
    swap.await.unwrap();
    assert_eq!(backend.head("k").await.unwrap().modified, 9);
}

/// Asserts that a journal encodes as a JSON list of entries with the operation in capitals,
/// that it decodes to the entries it was made of, and that bytes that are not such a list
/// are an error.
#[test]
fn a_journal_object_is_a_json_list_of_entries() {
    let put = JournalEntry {
        op: JournalOp::Put,
        key: "k".to_owned(),
        size: 5,
        time: 7,
    };
    let delete = JournalEntry {
        op: JournalOp::Delete,
        key: "j".to_owned(),
        size: 0,
        time: 8,
    };
    let entries = vec![put, delete];
    let encoded = encode_journal(&entries);
    let want = concat!(
        r#"[{"op":"PUT","key":"k","size":5,"time":7},"#,
        r#"{"op":"DELETE","key":"j","size":0,"time":8}]"#
    );
    assert_eq!(encoded, want.as_bytes());
    assert_eq!(decode_journal(&encoded), Ok(entries));

    let corrupt = decode_journal(b"not json").unwrap_err();
    assert!(matches!(corrupt, Error::Other(_)));
    assert!(corrupt.to_string().contains("corrupt journal object"));
}

fn says(error: &Error, message: &str) {
    assert_eq!(error.to_string(), message);
    assert!(!error.is_unavailable());
}

/// Asserts that every error says what happened and to which key, and that only
/// `Unavailable` says the backend as a whole is out of reach.
#[test]
fn errors_say_what_happened() {
    says(&Error::NotFound("k".to_owned()), "object k not found");
    let exists = Error::AlreadyExists("k".to_owned());
    says(&exists, "object k already exists");
    let precondition = Error::PreconditionFailed("k".to_owned());
    says(
        &precondition,
        "object k does not have the expected content tag",
    );
    says(&Error::Full("hot".to_owned()), "backend is full: hot");
    says(
        &Error::InvalidPart("part 3".to_owned()),
        "invalid part: part 3",
    );
    says(&Error::Unsupported("no".to_owned()), "not supported: no");
    let reserved = Error::Reserved("_tiering/x".to_owned());
    says(&reserved, "key _tiering/x is under a reserved prefix");
    says(&Error::Other("odd".to_owned()), "backend error: odd");
    let down = Error::Unavailable("no route".to_owned());
    assert_eq!(down.to_string(), "backend unavailable: no route");
    assert!(down.is_unavailable());
}

/// Asserts that the clock of the machine says a time after the year 2020, in milliseconds.
#[test]
fn the_system_clock_counts_milliseconds_since_the_epoch() {
    assert!(SystemClock.now() > 1_577_836_800_000);
}

fn faulty() -> FaultyBackend {
    FaultyBackend::new(Arc::new(MemoryBackend::new()))
}

/// Asserts that a fault injector with nothing planned passes every kind of call through
/// and counts each of them.
#[tokio::test]
async fn an_injector_with_no_plan_passes_calls_through_and_counts_them() {
    let backend = faulty();
    backend.put("k", body("v")).await.unwrap();
    assert_eq!(backend.get("k", None).await.unwrap(), body("v"));
    assert_eq!(backend.head("k").await.unwrap().size, 1);
    assert_eq!(keys(&backend).await, ["k"]);
    backend.delete("k").await.unwrap();
    for op in [Op::Put, Op::Get, Op::Head, Op::List, Op::Delete] {
        assert_eq!(backend.calls(op), 1, "{op:?}");
    }
    assert_eq!(backend.calls(Op::PutIfMatch), 0);
}

/// Asserts that the next `n` calls of a kind fail with `Unavailable` without reaching the
/// wrapped backend, that the call after them succeeds, and that other kinds are unaffected.
#[tokio::test]
async fn the_next_n_calls_of_a_kind_fail_and_then_calls_succeed() {
    let backend = faulty();
    backend.fail_next(Op::Put, 2);
    for _ in 0..2 {
        let failed = backend.put("k", body("v")).await;
        assert_eq!(failed.map_err(|e| e.is_unavailable()), Err(true));
        assert_eq!(keys(&backend).await, Vec::<String>::new());
    }
    backend.put("k", body("v")).await.unwrap();
    assert_eq!(keys(&backend).await, ["k"]);
    assert_eq!(backend.calls(Op::Put), 3);
}

/// Asserts that an error added with `fail_next_with` fails the call after those already
/// planned, with that error, and that the call after that succeeds.
#[tokio::test]
async fn an_error_can_be_queued_after_the_planned_failures() {
    let backend = faulty();
    backend.fail_next(Op::Put, 1);
    let other = Error::Other("odd".to_owned());
    backend.fail_next_with(Op::Put, other.clone());

    let first = backend.put("k", body("v")).await.unwrap_err();
    assert!(first.is_unavailable());
    assert_eq!(backend.put("k", body("v")).await, Err(other));
    backend.put("k", body("v")).await.unwrap();
    assert_eq!(keys(&backend).await, ["k"]);
}

/// Asserts that every kind of call can be failed on its own, and that a failed call leaves
/// the stored objects as they were.
#[tokio::test]
async fn every_kind_of_call_can_be_failed() {
    let backend = faulty();
    backend.put("k", body("v")).await.unwrap();
    for op in [Op::Put, Op::Get, Op::Head, Op::List, Op::Delete] {
        backend.fail_next(op, 1);
    }
    backend.fail_next(Op::PutIfMatch, 1);
    let put = backend.put("j", body("v")).await.unwrap_err();
    let swapped = backend.put_if_match("k", body("w"), "tag");
    let swap = swapped.await.unwrap_err();
    let get = backend.get("k", None).await.unwrap_err();
    let head = backend.head("k").await.unwrap_err();
    let list = backend.list("").await.unwrap_err();
    let delete = backend.delete("k").await.unwrap_err();
    for error in [put, swap, get, head, list, delete] {
        assert!(error.is_unavailable(), "{error}");
    }
    assert_eq!(keys(&backend).await, ["k"]);
    assert_eq!(backend.get("k", None).await.unwrap(), body("v"));
}

/// Asserts that a put planned to fail on keys with a prefix fails the next such call with the
/// error given and once only, that a call on another key is not affected and does not use
/// the plan up, and that a call of another kind on such a key is not affected either.
#[tokio::test]
async fn a_failure_planned_for_a_key_prefix_hits_one_matching_call() {
    let backend = faulty();
    let error = Error::Full("room".to_owned());
    backend.fail_next_on(Op::Put, "hot/", error.clone());
    backend.put("cold/1", body("v")).await.unwrap();
    let missing = backend.head("hot/1").await.unwrap_err();
    assert_eq!(missing, Error::NotFound("hot/1".to_owned()));

    assert_eq!(backend.put("hot/1", body("v")).await, Err(error));
    assert_eq!(keys(&backend).await, ["cold/1"]);
    backend.put("hot/1", body("v")).await.unwrap();
    assert_eq!(keys(&backend).await, ["cold/1", "hot/1"]);
}

/// Asserts that a failure planned for a prefix of a list call hits the list of that prefix,
/// and that the prefixes of all list calls, the failed ones included, are recorded in order.
#[tokio::test]
async fn list_calls_are_recorded_by_prefix() {
    let backend = faulty();
    backend.fail_next_on(Op::List, "b/", Error::Other("x".to_owned()));
    backend.list("a/").await.unwrap();
    assert!(backend.list("b/").await.is_err());
    backend.list_page("c/", None, 5).await.unwrap();
    assert_eq!(backend.listed_prefixes(), ["a/", "b/", "c/"]);
    assert_eq!(backend.calls(Op::List), 3);
}

/// Asserts that a backend that is down fails every kind of call until it is up again.
#[tokio::test]
async fn a_backend_that_is_down_fails_every_call_until_it_is_up() {
    let backend = faulty();
    backend.set_down(true);
    assert!(backend.put("k", body("v")).await.is_err());
    assert!(backend.list("").await.is_err());
    backend.set_down(false);
    backend.put("k", body("v")).await.unwrap();
    assert_eq!(keys(&backend).await, ["k"]);
}

/// Asserts that a delayed call waits the delay before it runs, that other kinds do not, and
/// that clearing the delay stops it.
#[tokio::test(start_paused = true)]
async fn a_delayed_call_waits_before_it_runs() {
    let backend = faulty();
    backend.set_delay(Op::Put, Some(Duration::from_secs(5)));
    let start = tokio::time::Instant::now();
    backend.put("k", body("v")).await.unwrap();
    let waited = start.elapsed();
    assert!(waited >= Duration::from_secs(5));
    backend.get("k", None).await.unwrap();
    backend.set_delay(Op::Put, None);
    backend.put("j", body("v")).await.unwrap();
    assert_eq!(start.elapsed(), waited);
}

/// Asserts that a streamed put to the in-memory backend creates the object from its chunks, with
/// the size and the tag of a put of the same bytes.
#[tokio::test]
async fn a_streamed_put_creates_the_object_from_its_chunks() {
    let backend = MemoryBackend::new();
    conformance::a_streamed_put_creates_the_object_from_its_chunks(&backend).await;
}

/// Asserts that a streamed put whose body fails stores nothing in the in-memory backend and
/// gives the error of the body.
#[tokio::test]
async fn a_streamed_put_that_fails_leaves_nothing() {
    let backend = MemoryBackend::new();
    conformance::a_streamed_put_that_fails_leaves_nothing(&backend).await;
}

/// Asserts that a streamed put of an existing key is refused and the first object stays.
#[tokio::test]
async fn a_streamed_put_of_an_existing_key_is_refused() {
    let backend = MemoryBackend::new();
    conformance::a_streamed_put_of_an_existing_key_is_refused(&backend).await;
}

/// Asserts that the fault injector passes a streamed put to the backend it wraps as a call of
/// kind `Put`: it is counted, and a failure planned for puts fails it and stores nothing.
#[tokio::test]
async fn the_injector_treats_a_streamed_put_as_a_put() {
    let backend = faulty();
    backend.put_stream("a", None, chunks(&["x"])).await.unwrap();
    assert_eq!(backend.calls(Op::Put), 1);
    backend.fail_next(Op::Put, 1);
    let failed = backend.put_stream("b", None, chunks(&["y"])).await;
    assert!(matches!(failed, Err(Error::Unavailable(_))));
    assert_eq!(backend.calls(Op::Put), 2);
    assert_eq!(
        backend.head("b").await.unwrap_err(),
        Error::NotFound("b".to_owned())
    );
}

/// Asserts that the monotonic clock never goes back between reads, for a clock made with `new`
/// and one made with `default`.
#[test]
fn the_monotonic_clock_does_not_go_back() {
    for clock in [
        tierstore::MonotonicClock::new(),
        tierstore::MonotonicClock::default(),
    ] {
        let first = clock.now();
        let second = clock.now();
        assert!(second >= first);
    }
}
