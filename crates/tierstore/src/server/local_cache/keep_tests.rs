//! The copy that a create-only put leaves in the cache: what is kept and under what tag, what is
//! not kept and why, and what a body that fails or a delete that comes meanwhile leaves behind.
//! The races are driven by the gates of the upstream fixture and of the bodies; no test waits for
//! a time.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::stream;
use futures::{FutureExt as _, StreamExt as _};
use tokio::sync::Semaphore;
use tokio::time::Instant;

use super::fill::{Filled, Pending, Staged};
use super::keep::{Keeping, Writer, tee};
use super::layout::{Sidecar, meta_path, name_of};
use super::metrics::{SkippedKeep, read};
use super::testing::{Quirk, Rig, digest, pattern};
use crate::{Backend, Error, Op, PartBody};

/// A body of one chunk holding `bytes`.
fn whole(bytes: &[u8]) -> PartBody<'static> {
    let chunk = Bytes::copy_from_slice(bytes);
    stream::once(async move { Ok(chunk) }).boxed()
}

/// A body of these chunks.
fn chunks(parts: &[&'static [u8]]) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = parts
        .iter()
        .copied()
        .map(|part| Ok(Bytes::from_static(part)))
        .collect();
    stream::iter(items).boxed()
}

/// A body that gives `first` and then fails, as a client that goes away.
fn cut(first: &'static [u8]) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = vec![
        Ok(Bytes::from_static(first)),
        Err(Error::Unavailable("the client went away".to_owned())),
    ];
    stream::iter(items).boxed()
}

/// A key named by its content.
fn content_key(bytes: &[u8]) -> String {
    format!("cas/l/{}", digest(bytes))
}

/// The sidecar the cache wrote for `key`.
fn sidecar(rig: &Rig, key: &str) -> Sidecar {
    let path = meta_path(&rig.config.path, &name_of(key).hex);
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

/// The puts that were not kept for `why`.
fn skipped(rig: &Rig, why: SkippedKeep) -> u64 {
    rig.cache.counters().keeps_skipped(why)
}

/// The puts the upstream took.
fn through(rig: &Rig) -> u64 {
    read(&rig.cache.counters().writes_through)
}

/// Asserts that no put is under way: nothing is staged, no byte is reserved and every keep
/// permit is back.
async fn at_rest(rig: &Rig) {
    // The writer of a dropped copy is aborted, and removes its file when the runtime next runs it.
    until("the stage files were removed", || rig.staged_files() == 0).await;
    assert_eq!(rig.cache.inner.reserved.load(Ordering::SeqCst), 0);
    let free = rig.cache.inner.keeps.available_permits();
    assert_eq!(free, rig.config.max_concurrent_keeps);
}

/// The turns of the scheduler a test gives an event before it fails.
const SPINS: u32 = 1_000_000;

/// Waits, by giving up the thread, until `done` is true; fails the test, saying what `what` was
/// to have happened, if it has not after [`SPINS`] turns.
async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..SPINS {
        if done() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("after {SPINS} turns of the scheduler, this never happened: {what}");
}

/// Asserts that a create-only put under a kept prefix leaves the object in the cache before it is
/// answered: the next read is a hit that makes no call to the upstream, the bytes are the
/// body's, the sidecar says the tag the upstream gives the object in a HEAD, the size and the
/// SHA-256 of the body, the put made one HEAD before the body and one after it, and the put is
/// counted. It fails on a cache that passes the put through without keeping it.
#[tokio::test]
async fn a_put_is_a_local_hit_with_the_tag_the_upstream_gave() {
    let rig = Rig::writing("tscache-keep-hit");
    let bytes = pattern(300);
    let parts: [&'static [u8]; 3] = [b"one ", b"two ", b"three"];
    let made = rig
        .cache
        .put_stream("cas/b/small", Some(13), chunks(&parts));
    assert_eq!(made.await, Ok(()));
    let made = rig.cache.put_stream("cas/b/big", Some(300), whole(&bytes));
    assert_eq!(made.await, Ok(()));

    assert_eq!(rig.calls(Op::Head), 4);
    assert_eq!(rig.cache.held(), (313, 2));
    at_rest(&rig).await;
    assert_eq!(through(&rig), 2);
    for (key, want) in [
        ("cas/b/small", &b"one two three"[..]),
        ("cas/b/big", &bytes[..]),
    ] {
        let upstream = rig.tamper.memory.head(key).await.unwrap();
        let kept = sidecar(&rig, key);
        assert_eq!(kept.tag, upstream.tag, "{key}");
        assert_eq!(kept.size, want.len() as u64, "{key}");
        assert_eq!(kept.modified, upstream.modified, "{key}");
        assert_eq!(kept.sha256, digest(want), "{key}");
        assert_eq!(kept.key, key);
        assert!(rig.has_file(key), "{key}");
    }

    let gets = rig.calls(Op::Get);
    let heads = rig.calls(Op::Head);
    assert_eq!(rig.get("cas/b/small").await.unwrap(), "one two three");
    assert_eq!(rig.get("cas/b/big").await.unwrap(), bytes);
    assert_eq!((rig.calls(Op::Get), rig.calls(Op::Head)), (gets, heads));
    assert_eq!(read(&rig.cache.counters().hits), 2);
    assert_eq!(read(&rig.cache.counters().misses), 0);
    let text = rig.cache.metrics();
    assert!(text.contains("tierstore_cache_writes_through_total 2\n"));
    assert!(text.contains("tierstore_cache_write_exists_total 0\n"));
    assert!(text.contains("tierstore_cache_write_keep_skipped_total{reason=\"head\"} 0\n"));
}

/// Asserts that a body that is cut leaves nothing: the put gives the body's error, the upstream
/// has no object, the cache has no entry and no file, the stage file is removed and the room and
/// the permit are given back; and nothing is counted as a put that was taken or skipped. It
/// fails on a put that publishes the part it had received.
#[tokio::test]
async fn a_cut_body_leaves_nothing_at_the_upstream_or_in_the_cache() {
    let rig = Rig::writing("tscache-keep-cut");
    let made = rig
        .cache
        .put_stream("cas/b/cut", Some(100), cut(b"partial"));
    let error = Error::Unavailable("the client went away".to_owned());
    assert_eq!(made.await, Err(error));

    assert!(rig.tamper.memory.head("cas/b/cut").await.is_err());
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/cut"));
    at_rest(&rig).await;
    assert_eq!(through(&rig), 0);
    assert_eq!(skipped(&rig, SkippedKeep::Head), 0);
    assert_eq!(rig.calls(Op::Head), 1);
}

/// Asserts that a key named by its content whose body hashes to another name is refused with an
/// error at the end of the body, so that the upstream stores nothing, and nothing is kept; and
/// that a body that hashes to the name is kept with that hash in its sidecar. It fails on a put
/// that keeps, or lets the upstream store, a body that is not what its name says.
#[tokio::test]
async fn a_body_that_does_not_hash_to_its_name_leaves_nothing() {
    let rig = Rig::writing("tscache-keep-mismatch");
    let key = content_key(b"good bytes");
    let refused = rig.cache.put_stream(&key, Some(10), whole(b"evil bytes"));
    let refused = refused.await;
    assert_eq!(refused, Err(Error::BadDigest(key.clone())));
    assert!(rig.tamper.memory.head(&key).await.is_err());
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(&key));
    at_rest(&rig).await;
    assert_eq!(through(&rig), 0);

    let made = rig.cache.put_stream(&key, Some(10), whole(b"good bytes"));
    assert_eq!(made.await, Ok(()));
    assert_eq!(rig.cache.held(), (10, 1));
    assert_eq!(sidecar(&rig, &key).sha256, digest(b"good bytes"));
    assert_eq!(rig.get(&key).await.unwrap(), "good bytes");
    assert_eq!(read(&rig.cache.counters().hits), 1);
}

/// Asserts that a mismatch is refused whether or not the body is copied: with a budget that
/// cannot hold the body, so that it is not copied, it is still hashed and refused, so what is
/// accepted does not depend on the load.
#[tokio::test]
async fn a_hash_mismatch_is_refused_when_the_body_is_not_copied() {
    let rig = Rig::with("tscache-keep-mismatch-uncopied", |config| {
        config.write_through = true;
        config.budget = 5;
    });
    let key = content_key(b"good bytes");
    let refused = rig.cache.put_stream(&key, Some(10), whole(b"evil bytes"));
    assert_eq!(refused.await, Err(Error::BadDigest(key.clone())));
    assert!(rig.tamper.memory.head(&key).await.is_err());
    assert_eq!(skipped(&rig, SkippedKeep::Budget), 0);
    at_rest(&rig).await;
}

/// Asserts that a stage directory that cannot be written, and a link that fails, each still give
/// a put that succeeds with the object at the upstream, no entry, no data file, no sidecar and
/// no stage file, and count the skip as `io`. The next read fills the object.
#[tokio::test]
async fn an_unwritable_stage_or_a_failed_link_still_succeeds_and_is_counted() {
    let rig = Rig::writing("tscache-keep-io");
    let stage = rig.config.path.join(".stage");
    std::fs::remove_dir_all(&stage).unwrap();
    std::fs::write(&stage, b"not a directory").unwrap();
    let made = rig
        .cache
        .put_stream("cas/b/nostage", Some(5), whole(b"bytes"));
    assert_eq!(made.await, Ok(()));
    assert_eq!(skipped(&rig, SkippedKeep::Io), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/nostage"));
    let stored = rig.tamper.memory.get("cas/b/nostage", None).await;
    assert_eq!(stored.unwrap(), "bytes");
    assert_eq!(through(&rig), 1);
    assert_eq!(rig.cache.inner.reserved.load(Ordering::SeqCst), 0);

    let rig = Rig::writing("tscache-keep-link");
    let denied = std::io::ErrorKind::PermissionDenied;
    rig.disk.set(Quirk::LinkFails(denied));
    let made = rig
        .cache
        .put_stream("cas/b/nolink", Some(5), whole(b"bytes"));
    assert_eq!(made.await, Ok(()));
    assert_eq!(skipped(&rig, SkippedKeep::Io), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/nolink"));
    let meta = meta_path(&rig.config.path, &name_of("cas/b/nolink").hex);
    assert!(!meta.exists());
    at_rest(&rig).await;
    assert_eq!(through(&rig), 1);
    rig.disk.set(Quirk::None);
    assert_eq!(rig.get("cas/b/nolink").await.unwrap(), "bytes");
    assert_eq!(rig.cache.held(), (5, 1));
}

/// Asserts that a body the budget cannot hold, and a body of no declared length (which cannot be
/// reserved for), still give a put that succeeds with the object at the upstream and nothing
/// kept, counted as `budget`.
#[tokio::test]
async fn a_body_over_the_budget_or_of_no_length_is_not_kept_and_is_counted() {
    let rig = Rig::with("tscache-keep-budget", |config| {
        config.write_through = true;
        config.budget = 100;
    });
    let bytes = pattern(300);
    let made = rig
        .cache
        .put_stream("cas/b/large", Some(300), whole(&bytes));
    assert_eq!(made.await, Ok(()));
    assert_eq!(skipped(&rig, SkippedKeep::Budget), 1);
    let made = rig.cache.put_stream("cas/b/nolen", None, whole(b"short"));
    assert_eq!(made.await, Ok(()));
    assert_eq!(skipped(&rig, SkippedKeep::Budget), 2);
    for key in ["cas/b/large", "cas/b/nolen"] {
        assert!(rig.tamper.memory.head(key).await.is_ok(), "{key}");
        assert!(!rig.has_file(key), "{key}");
    }
    assert_eq!(rig.cache.held(), (0, 0));
    at_rest(&rig).await;
    assert_eq!(through(&rig), 2);
}

/// Asserts that a HEAD that fails after the upstream took the put gives a put that succeeds, an
/// object at the upstream, nothing kept and a skip counted as `head`; and that a read of it then
/// fills it as for any object. The HEAD before the body answers "not found", the one after fails.
#[tokio::test]
async fn a_failed_head_after_the_put_keeps_nothing_and_is_counted() {
    let rig = Rig::writing("tscache-keep-head");
    let key = "cas/b/headless";
    rig.upstream
        .fail_next_with(Op::Head, Error::NotFound(key.to_owned()));
    let down = Error::Unavailable("head".to_owned());
    rig.upstream.fail_next_with(Op::Head, down);
    let made = rig.cache.put_stream(key, Some(5), whole(b"bytes"));
    assert_eq!(made.await, Ok(()));

    assert_eq!(rig.calls(Op::Head), 2);
    assert_eq!(skipped(&rig, SkippedKeep::Head), 1);
    assert_eq!(through(&rig), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(key));
    at_rest(&rig).await;
    assert!(super::locked(&rig.cache.inner.flights).is_empty());
    assert_eq!(rig.tamper.memory.get(key, None).await.unwrap(), "bytes");
    assert_eq!(rig.get(key).await.unwrap(), "bytes");
    assert_eq!(read(&rig.cache.counters().misses), 1);
    assert_eq!(rig.cache.held(), (5, 1));
}

/// Asserts that a put that finds every keep permit taken succeeds, is not kept and is counted as
/// `busy`, and that the put that holds the permit is kept when it ends: the first put's body is
/// held after its first chunk, so it has its stage file and its permit while the second runs.
#[tokio::test]
async fn a_put_that_finds_every_keep_busy_succeeds_and_is_counted() {
    let rig = Rig::with("tscache-keep-busy", |config| {
        config.write_through = true;
        config.max_concurrent_keeps = 1;
    });
    let gate = Arc::new(Semaphore::new(0));
    let taken = Arc::new(AtomicUsize::new(0));
    let (held, counted) = (Arc::clone(&gate), Arc::clone(&taken));
    let second = stream::once(async move {
        let _ = counted.fetch_add(1, Ordering::SeqCst);
        held.acquire().await.expect("the gate stays open").forget();
        Ok(Bytes::from_static(b"world"))
    });
    let first = stream::once(async { Ok(Bytes::from_static(b"hello ")) });
    let slow: PartBody<'static> = first.chain(second).boxed();
    let cache = rig.cache.clone();
    let slow_put =
        tokio::spawn(async move { cache.put_stream("cas/b/slow", Some(11), slow).await });
    until(
        "the first put's body was polled for its second chunk",
        || taken.load(Ordering::SeqCst) >= 1,
    )
    .await;
    assert_eq!(rig.staged_files(), 1);

    let made = rig.cache.put_stream("cas/b/fast", Some(4), whole(b"fast"));
    assert_eq!(made.await, Ok(()));
    assert_eq!(skipped(&rig, SkippedKeep::Busy), 1);
    assert!(!rig.has_file("cas/b/fast"));
    assert_eq!(
        rig.tamper.memory.get("cas/b/fast", None).await.unwrap(),
        "fast"
    );

    gate.add_permits(1);
    assert_eq!(slow_put.await.unwrap(), Ok(()));
    assert_eq!(rig.cache.held(), (11, 1));
    assert!(rig.has_file("cas/b/slow"));
    at_rest(&rig).await;
}

/// Asserts that the keeps have permits of their own: with a single fill allowed and that fill
/// running, a put is still kept. It fails if the keeps draw on the permits of the fills.
#[tokio::test]
async fn a_running_fill_does_not_stop_a_put_from_being_kept() {
    let rig = Rig::with("tscache-keep-own-permits", |config| {
        config.write_through = true;
        config.max_concurrent_fills = 1;
    });
    rig.put("cas/b/filling", &pattern(50)).await;
    let gate = rig.tamper.hold_key("cas/b/filling");
    let cache = rig.cache.clone();
    let reader = tokio::spawn(async move { cache.get("cas/b/filling", None).await });
    until("the fill of the first key started", || {
        !super::locked(&rig.cache.inner.flights).is_empty()
    })
    .await;
    assert_eq!(rig.cache.inner.permits.available_permits(), 0);

    let made = rig.cache.put_stream("cas/b/put", Some(3), whole(b"new"));
    assert_eq!(made.await, Ok(()));
    assert_eq!(skipped(&rig, SkippedKeep::Busy), 0);
    assert!(rig.has_file("cas/b/put"));

    gate.add_permits(16);
    assert_eq!(reader.await.unwrap().unwrap().len(), 50);
    assert_eq!(rig.cache.held(), (53, 2));
}

/// Asserts that a delete that finishes after the keep's HEAD and before the keep is in the index
/// leaves no entry: the put was taken (and answered), the object is gone from the upstream, the
/// cache has no entry or file, the skip is counted as `deleted`, and the keep leaves no mark
/// behind. The keep is held after its HEAD has found the object. It fails on a keep that does
/// not look for the delete's mark.
#[tokio::test]
async fn a_delete_that_races_a_keep_leaves_no_entry() {
    let rig = Rig::writing("tscache-keep-delete");
    let key = "cas/b/racing";
    let held = rig.tamper.hold_heads();
    let (cache, name) = (rig.cache.clone(), key.to_owned());
    let putting = tokio::spawn(async move {
        let body = whole(b"doomed");
        cache.put_stream(&name, Some(6), body).await
    });
    until("the keep's HEAD found the object", || {
        rig.tamper.heading() >= 1
    })
    .await;
    assert_eq!(rig.cache.held(), (0, 0));

    assert_eq!(rig.cache.delete(key).await, Ok(()));
    assert!(rig.tamper.memory.head(key).await.is_err());
    held.add_permits(16);
    assert_eq!(putting.await.unwrap(), Ok(()));

    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(key));
    assert!(!meta_path(&rig.config.path, &name_of(key).hex).exists());
    assert_eq!(skipped(&rig, SkippedKeep::Deleted), 1);
    at_rest(&rig).await;
    assert!(super::locked(&rig.cache.inner.flights).is_empty());
    assert!(super::locked(&rig.cache.inner.invalidated).is_empty());
    assert!(matches!(rig.get(key).await, Err(Error::NotFound(_))));
}

/// The data files and the sidecars in the object directory of the rig's cache.
fn cache_files(rig: &Rig) -> (usize, usize) {
    fn walk(dir: &std::path::Path, found: &mut (usize, usize)) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, found);
            } else if path.extension().is_some_and(|ext| ext == "meta") {
                found.1 += 1;
            } else {
                found.0 += 1;
            }
        }
    }
    let mut found = (0, 0);
    walk(&rig.config.path.join("obj"), &mut found);
    found
}

/// Asserts that a put whose request is dropped after the upstream took the object, while its
/// keep waits for the HEAD, still ends with the object kept whole: the keep runs on a task of
/// its own, so when the HEAD is let go the object is in the index with its data file and its
/// sidecar, and there is no other file in the cache, no stage file, no entry in the table of the
/// fills, no mark, and the permit and the room are back. It fails on a keep that runs in the
/// request, which is gone with it and leaves the object to be filled again, and (with a client
/// that goes away while the files are being linked) files that nothing indexes.
#[tokio::test]
async fn a_put_dropped_after_the_ack_still_ends_with_the_object_kept_whole() {
    let rig = Rig::writing("tscache-keep-dropped");
    let key = "cas/b/orphan";
    let held = rig.tamper.hold_heads();
    let (cache, name) = (rig.cache.clone(), key.to_owned());
    let putting =
        tokio::spawn(async move { cache.put_stream(&name, Some(6), whole(b"orphan")).await });
    until("the keep's HEAD found the object", || {
        rig.tamper.heading() >= 1
    })
    .await;
    putting.abort();
    assert!(putting.await.unwrap_err().is_cancelled());

    held.add_permits(16);
    until("the keep ended", || {
        rig.cache.held() == (6, 1) && super::locked(&rig.cache.inner.flights).is_empty()
    })
    .await;
    assert_eq!(cache_files(&rig), (1, 1));
    assert!(rig.has_file(key));
    assert_eq!(sidecar(&rig, key).sha256, digest(b"orphan"));
    assert!(super::locked(&rig.cache.inner.invalidated).is_empty());
    at_rest(&rig).await;
    let gets = rig.calls(Op::Get);
    assert_eq!(rig.get(key).await.unwrap(), "orphan");
    assert_eq!(rig.calls(Op::Get), gets);
}

/// Asserts that a disk that hangs costs the copy and not the put: with every write to the stage
/// file held (and never let go), a body of more chunks than the queue holds is passed to the
/// upstream whole and the put is answered, the copy is dropped and counted as `slow`, and the
/// stage file is removed. It fails on a tee that waits for the disk, which never finishes the
/// put.
#[tokio::test]
async fn a_hung_disk_drops_the_copy_and_never_stops_the_upstream() {
    let rig = Rig::writing("tscache-keep-slow-disk");
    let _held = rig.disk.hold_writes();
    let pieces = super::keep::KEEP_QUEUE + 8;
    let items: Vec<Result<Bytes, Error>> = (0..pieces)
        .map(|n| Ok(Bytes::from(vec![n as u8; 100])))
        .collect();
    let length = (pieces * 100) as u64;
    let made = rig
        .cache
        .put_stream("cas/b/slow", Some(length), stream::iter(items).boxed());
    assert_eq!(made.await, Ok(()));

    assert_eq!(skipped(&rig, SkippedKeep::Slow), 1);
    assert_eq!(through(&rig), 1);
    let stored = rig.tamper.memory.get("cas/b/slow", None).await.unwrap();
    assert_eq!(stored.len() as u64, length);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/slow"));
    at_rest(&rig).await;
    assert!(
        rig.cache
            .metrics()
            .contains("write_keep_skipped_total{reason=\"slow\"} 1\n")
    );
}

/// Asserts the check of a key named by its content against a backend that stops reading its body
/// as soon as it has the declared length, and so never sees it end: the bytes are checked when
/// they are all there, a body that is right is kept, and one that is wrong is `BadDigest` in
/// place of its last chunk, so the backend stores nothing. It fails on a check that waits for
/// the end of the stream, which this backend never reaches.
#[tokio::test]
async fn a_backend_that_stops_at_the_declared_length_is_still_checked() {
    let rig = Rig::writing("tscache-keep-stops-at-length");
    rig.tamper.stop_after.store(2, Ordering::SeqCst);
    let good = content_key(b"good bytes");
    let made = rig
        .cache
        .put_stream(&good, Some(10), chunks(&[b"good ", b"bytes"]));
    assert_eq!(made.await, Ok(()));
    assert_eq!(rig.cache.held(), (10, 1));
    assert_eq!(sidecar(&rig, &good).sha256, digest(b"good bytes"));

    let other = content_key(b"other data");
    let wrong = rig
        .cache
        .put_stream(&other, Some(10), chunks(&[b"evil ", b"data!"]));
    assert_eq!(wrong.await, Err(Error::BadDigest(other.clone())));
    assert!(rig.tamper.memory.head(&other).await.is_err());
    assert_eq!(rig.cache.held(), (10, 1));
    at_rest(&rig).await;
}

/// Asserts what is done with a backend that takes a put without reading the body to the
/// declared length: for a key named by its content the put is `BadDigest` (the body was never
/// checked); for any other key the put succeeds and is not kept, counted as `head`. It fails on
/// a keep that publishes bytes it did not see whole, or skips the check of a key it did not
/// hash.
#[tokio::test]
async fn a_backend_that_stops_short_of_the_length_is_not_trusted() {
    let rig = Rig::writing("tscache-keep-stops-short");
    rig.tamper.stop_after.store(1, Ordering::SeqCst);
    let made = rig
        .cache
        .put_stream("cas/b/short", Some(10), chunks(&[b"short", b"ened!"]));
    assert_eq!(made.await, Ok(()));
    assert_eq!(skipped(&rig, SkippedKeep::Head), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/short"));

    let key = content_key(b"shortened!");
    let checked = rig
        .cache
        .put_stream(&key, Some(10), chunks(&[b"short", b"ened!"]));
    assert_eq!(checked.await, Err(Error::BadDigest(key.clone())));
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(&key));
    at_rest(&rig).await;
}

/// Asserts that a delete which finishes after the copy is linked and before the entry is put in
/// the index (the hook runs in that gap, and does what the end of a delete does) leaves no entry:
/// the check for the delete's mark is made after the insert, and removes what the insert put
/// there. A check made before the insert does not see this delete, and the index then holds an
/// object whose files the delete removed.
#[tokio::test]
async fn a_delete_just_before_the_index_insert_is_caught_by_the_check_after_it() {
    let rig = Rig::writing("tscache-keep-delete-gap");
    let key = "cas/b/gap";
    let inner = Arc::clone(&rig.cache.inner);
    let deleting = Arc::clone(&inner);
    *super::locked(&inner.before_insert) = Some(Box::new(move || deleting.forget(key)));
    let made = rig.cache.put_stream(key, Some(3), whole(b"gap"));
    assert_eq!(made.await, Ok(()));
    *super::locked(&inner.before_insert) = None;

    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(key));
    assert_eq!(skipped(&rig, SkippedKeep::Deleted), 1);
    assert_eq!(cache_files(&rig), (0, 0));
    at_rest(&rig).await;
}

/// Asserts that a keep that is held past `keep_wait` does not hold the put: the put is answered
/// 200 at exactly `keep_wait` of the (paused) clock, counted as expired, with nothing in the cache
/// yet; and when the keep is let go it ends alone with the object whole in the cache (one data
/// file, one sidecar), no stage file and no mark. It fails on a put that waits for the keep.
#[tokio::test(start_paused = true)]
async fn a_keep_held_past_keep_wait_answers_the_put_then_completes_alone() {
    let rig = Rig::with("tscache-keep-wait", |config| {
        config.write_through = true;
        config.keep_wait = Duration::from_secs(7);
    });
    let key = "cas/b/waited";
    let held = rig.tamper.hold_heads();
    let start = Instant::now();
    let made = rig.cache.put_stream(key, Some(6), whole(b"waited"));
    assert_eq!(made.await, Ok(()));
    assert_eq!(start.elapsed(), Duration::from_secs(7));
    assert_eq!(read(&rig.cache.counters().keeps_expired), 1);
    assert!(
        rig.cache
            .metrics()
            .contains("tierstore_cache_keeps_expired_total 1\n")
    );
    assert_eq!(rig.cache.held(), (0, 0));

    held.add_permits(16);
    until("the keep ended", || {
        rig.cache.held() == (6, 1) && super::locked(&rig.cache.inner.flights).is_empty()
    })
    .await;
    assert_eq!(cache_files(&rig), (1, 1));
    assert_eq!(sidecar(&rig, key).sha256, digest(b"waited"));
    assert!(super::locked(&rig.cache.inner.invalidated).is_empty());
    assert_eq!(skipped(&rig, SkippedKeep::Deadline), 0);
    at_rest(&rig).await;
}

/// Asserts that a keep that is abandoned after the put was answered leaves nothing: the put is
/// answered at `keep_wait`, the object is deleted while the keep is held, and when the keep is let
/// go it ends without an entry, a data file, a sidecar or a stage file, with its permit and room
/// back, counted as expired (the put) and `deleted` (the keep).
#[tokio::test(start_paused = true)]
async fn an_abandoned_keep_leaves_no_orphan() {
    let rig = Rig::with("tscache-keep-abandoned", |config| {
        config.write_through = true;
        config.keep_wait = Duration::from_secs(3);
    });
    let key = "cas/b/abandoned";
    let held = rig.tamper.hold_heads();
    let made = rig.cache.put_stream(key, Some(9), whole(b"abandoned"));
    assert_eq!(made.await, Ok(()));
    assert_eq!(read(&rig.cache.counters().keeps_expired), 1);
    assert_eq!(rig.cache.delete(key).await, Ok(()));

    held.add_permits(16);
    until("the keep ended", || {
        super::locked(&rig.cache.inner.flights).is_empty()
    })
    .await;
    assert_eq!(skipped(&rig, SkippedKeep::Deleted), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert_eq!(cache_files(&rig), (0, 0));
    assert!(super::locked(&rig.cache.inner.invalidated).is_empty());
    at_rest(&rig).await;
}

/// Asserts what a disk that hangs does to a body too small to fill the queue: the put is answered
/// at `keep_wait`; a reader that comes meanwhile is served from the upstream at once (the clock
/// has not moved when it is answered, so it did not wait for the keep); and the keep is abandoned
/// at `keep_deadline`, counted as `deadline`, with no stage file, no entry in the table of the
/// fills, no mark, no file in the cache, and permit and room back; the next read fills as usual.
/// It fails on a keep that waits for the writer for ever, or on a reader that waits for the keep.
#[tokio::test(start_paused = true)]
async fn a_keep_on_a_hung_disk_is_abandoned_at_its_deadline_and_readers_do_not_wait() {
    let rig = Rig::with("tscache-keep-deadline", |config| {
        config.write_through = true;
        config.keep_wait = Duration::from_secs(5);
        config.keep_deadline = Duration::from_secs(60);
    });
    let key = "cas/b/hung";
    let _held = rig.disk.hold_writes();
    let start = Instant::now();
    let parts: [&'static [u8]; 3] = [b"small ", b"body ", b"here"];
    let made = rig.cache.put_stream(key, Some(15), chunks(&parts));
    assert_eq!(made.await, Ok(()));
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    assert_eq!(read(&rig.cache.counters().keeps_expired), 1);
    assert!(!super::locked(&rig.cache.inner.flights).is_empty());

    assert_eq!(rig.get(key).await.unwrap(), "small body here");
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    assert_eq!(rig.cache.held(), (0, 0));

    tokio::time::sleep(Duration::from_secs(60)).await;
    until("the keep was abandoned", || {
        super::locked(&rig.cache.inner.flights).is_empty()
    })
    .await;
    assert_eq!(skipped(&rig, SkippedKeep::Deadline), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert_eq!(cache_files(&rig), (0, 0));
    assert!(super::locked(&rig.cache.inner.invalidated).is_empty());
    at_rest(&rig).await;

    assert_eq!(rig.get(key).await.unwrap(), "small body here");
    assert_eq!(rig.cache.held(), (15, 1));
}

/// Asserts that a request dropped while the files of the copy are being linked (on a thread that
/// is held there) still ends with the object kept whole: when the link is let go the object is
/// in the index with one data file and one sidecar, and no stage file, mark or fill-table entry
/// is left. It fails on a keep that is part of the request, which leaves the linked files with no
/// entry.
#[tokio::test]
async fn a_put_dropped_while_the_files_are_linked_still_ends_kept_whole() {
    let rig = Rig::writing("tscache-keep-dropped-link");
    let key = "cas/b/linking";
    let gate = rig.disk.hold_links();
    let (cache, name) = (rig.cache.clone(), key.to_owned());
    let putting =
        tokio::spawn(async move { cache.put_stream(&name, Some(7), whole(b"linking")).await });
    until("the link began", || {
        !rig.disk.linked.lock().unwrap().is_empty()
    })
    .await;
    putting.abort();
    assert!(putting.await.unwrap_err().is_cancelled());

    gate.open();
    until("the keep ended", || {
        rig.cache.held() == (7, 1) && super::locked(&rig.cache.inner.flights).is_empty()
    })
    .await;
    assert_eq!(cache_files(&rig), (1, 1));
    assert!(rig.has_file(key));
    assert!(super::locked(&rig.cache.inner.invalidated).is_empty());
    at_rest(&rig).await;
}

/// Asserts that the put does not wait for the eviction pass its object calls for: the pass is held
/// on its thread (at the question about free space), the put returns, the object is in the cache
/// over the high water mark; when the pass is let go it evicts. It fails on a put that waits for
/// the pass.
#[tokio::test]
async fn a_put_does_not_wait_for_the_eviction_pass() {
    let rig = Rig::writing("tscache-keep-eviction");
    let gate = rig.disk.hold_space();
    let bytes = pattern(960_000);
    let made = rig
        .cache
        .put_stream("cas/b/over", Some(960_000), whole(&bytes));
    assert_eq!(made.await, Ok(()));
    until("the pass asked for the free space", || {
        rig.disk.space_waiting() >= 1
    })
    .await;
    assert_eq!(rig.cache.held(), (960_000, 1));

    gate.open();
    until("the pass evicted the object", || rig.cache.held() == (0, 0)).await;
    at_rest(&rig).await;
}

/// Asserts that the memory of the copy is bounded in bytes and not only in chunks: with the disk
/// held, a body of four chunks of 2 MiB (far fewer than the chunks the queue takes) passes the
/// queue's 4 MiB at its third chunk, and the copy is dropped as `slow` while the upstream gets
/// the whole body.
#[tokio::test]
async fn a_hung_disk_drops_the_copy_when_the_queued_bytes_pass_the_bound() {
    let rig = Rig::with("tscache-keep-queue-bytes", |config| {
        config.write_through = true;
        config.budget = 64 << 20;
    });
    let _held = rig.disk.hold_writes();
    let items: Vec<Result<Bytes, Error>> = (0..4)
        .map(|n| Ok(Bytes::from(vec![n as u8; 2 << 20])))
        .collect();
    let made = rig
        .cache
        .put_stream("cas/b/heavy", Some(8 << 20), stream::iter(items).boxed());
    assert_eq!(made.await, Ok(()));

    assert_eq!(skipped(&rig, SkippedKeep::Slow), 1);
    let stored = rig.tamper.memory.get("cas/b/heavy", None).await.unwrap();
    assert_eq!(stored.len(), 8 << 20);
    assert_eq!(rig.cache.held(), (0, 0));
    at_rest(&rig).await;
}

/// Asserts what a writer does once it has been finished: the chunks it had are written and the
/// stage file comes back; a chunk sent after that is refused as `io` (there is no way to the
/// task any more); and finishing it again is an error, not a second stage file.
#[tokio::test]
async fn a_finished_writer_takes_no_chunk_and_finishes_once() {
    let rig = Rig::writing("tscache-keep-writer");
    let inner = Arc::clone(&rig.cache.inner);
    let staged = Staged::create(&inner).await.unwrap();
    let mut writer = Writer::start(&inner, staged);
    assert_eq!(writer.send(Bytes::from_static(b"abc")), Ok(()));
    let staged = writer.finish().await.unwrap();
    assert_eq!(writer.send(Bytes::from_static(b"x")), Err(SkippedKeep::Io));
    let again = writer.finish().await;
    assert!(again.is_err(), "a second finish gave a stage file");
    drop(staged);
    at_rest(&rig).await;
}

/// Asserts that the teed body ends after an error: the error is given once, and a poll after it
/// gives the end of the stream and not another chunk or error (a backend may poll a body again).
#[tokio::test]
async fn a_teed_body_that_failed_stays_ended() {
    let mut keeping = Keeping::new(None, Some(10), Err(SkippedKeep::Busy));
    let items: Vec<Result<Bytes, Error>> = vec![
        Err(Error::Unavailable("the client went away".to_owned())),
        Ok(Bytes::from_static(b"never given")),
    ];
    let mut teed = tee("cas/b/ended", stream::iter(items).boxed(), &mut keeping);
    assert!(matches!(
        teed.next().await,
        Some(Err(Error::Unavailable(_)))
    ));
    assert!(teed.next().await.is_none());
    assert!(teed.next().await.is_none());
}

/// Asserts that a keep that finds the key's place in the table of the fills taken (a fill is
/// running for it) is skipped as `busy`, and drops the entry and marks the fill like the other
/// skips: the put succeeds with the object at the upstream, nothing is kept, the fill's mark is
/// set, and the permit and the room are back.
#[tokio::test]
async fn a_keep_that_finds_a_fill_running_for_the_key_is_skipped_as_busy() {
    let rig = Rig::writing("tscache-keep-fill-running");
    let key = "cas/b/taken";
    let hex = name_of(key).hex;
    let running: Pending = futures::future::ready(Ok::<Filled, Error>(Filled::Skipped))
        .boxed()
        .shared();
    drop(super::locked(&rig.cache.inner.flights).insert(hex.clone(), running));

    let made = rig.cache.put_stream(key, Some(5), whole(b"taken"));
    assert_eq!(made.await, Ok(()));
    assert_eq!(skipped(&rig, SkippedKeep::Busy), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(key));
    assert_eq!(rig.tamper.memory.get(key, None).await.unwrap(), "taken");
    assert!(super::locked(&rig.cache.inner.invalidated).contains(&hex));
    drop(super::locked(&rig.cache.inner.flights).remove(&hex));
    let _ = super::locked(&rig.cache.inner.invalidated).remove(&hex);
    at_rest(&rig).await;
}

/// Asserts that a HEAD after the put which says the object has a size other than the bytes that
/// were sent is not trusted: the put succeeds, nothing is kept, and it is counted as `head`.
#[tokio::test]
async fn a_head_that_gives_another_size_than_the_body_is_not_kept() {
    let rig = Rig::writing("tscache-keep-size");
    rig.tamper.lie_size.store(true, Ordering::SeqCst);
    let made = rig
        .cache
        .put_stream("cas/b/sized", Some(5), whole(b"sized"));
    assert_eq!(made.await, Ok(()));
    assert_eq!(skipped(&rig, SkippedKeep::Head), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/sized"));
    assert_eq!(through(&rig), 1);
    at_rest(&rig).await;
}

/// Asserts that deleting a key under a prefix the cache does not keep is passed to the upstream
/// and touches nothing of the cache: the object is gone from the upstream, the delete is Ok, and
/// there is no entry, file or mark.
#[tokio::test]
async fn a_delete_of_a_key_that_is_not_kept_only_reaches_the_upstream() {
    let rig = Rig::writing("tscache-keep-delete-unkept");
    rig.put("other/d", b"x").await;
    assert_eq!(rig.cache.delete("other/d").await, Ok(()));
    assert!(rig.tamper.memory.head("other/d").await.is_err());
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(super::locked(&rig.cache.inner.invalidated).is_empty());
    assert!(super::locked(&rig.cache.inner.flights).is_empty());
}

/// Asserts that a put whose upstream answers "already exists" after the HEAD found nothing (it
/// lost the race to another writer) is counted as a repeat, keeps nothing and leaves the other
/// writer's object; and that a put that finds the object at the HEAD is counted too.
#[tokio::test]
async fn a_put_that_exists_is_counted_and_keeps_nothing() {
    let rig = Rig::writing("tscache-keep-exists");
    rig.put("cas/b/there", b"first").await;
    let hidden = Error::NotFound("cas/b/there".to_owned());
    rig.upstream.fail_next_with(Op::Head, hidden);
    let lost = rig
        .cache
        .put_stream("cas/b/there", Some(6), whole(b"second"));
    let exists = Error::AlreadyExists("cas/b/there".to_owned());
    assert_eq!(lost.await, Err(exists.clone()));
    let again = rig
        .cache
        .put_stream("cas/b/there", Some(5), whole(b"third"));
    assert_eq!(again.await, Err(exists));

    assert_eq!(read(&rig.cache.counters().write_exists), 2);
    assert_eq!(through(&rig), 0);
    assert_eq!(rig.cache.held(), (0, 0));
    at_rest(&rig).await;
    let stored = rig.tamper.memory.get("cas/b/there", None).await;
    assert_eq!(stored.unwrap(), "first");
}

/// Asserts that a put under a prefix the cache does not keep is never kept: the upstream has it,
/// the cache has no entry, no stage file is made, the put makes one HEAD only (the one before the
/// body) and no skip is counted.
#[tokio::test]
async fn a_put_under_an_uncached_prefix_is_never_kept() {
    let rig = Rig::writing("tscache-keep-uncached");
    let made = rig.cache.put_stream("other/obj", Some(5), whole(b"bytes"));
    assert_eq!(made.await, Ok(()));
    assert_eq!(rig.calls(Op::Head), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("other/obj"));
    at_rest(&rig).await;
    assert_eq!(through(&rig), 1);
    for why in [
        SkippedKeep::Budget,
        SkippedKeep::Io,
        SkippedKeep::Busy,
        SkippedKeep::Head,
        SkippedKeep::Deleted,
    ] {
        assert_eq!(skipped(&rig, why), 0, "{why:?}");
    }
    assert_eq!(rig.get("other/obj").await.unwrap(), "bytes");
    assert_eq!(rig.cache.held(), (0, 0));
}
