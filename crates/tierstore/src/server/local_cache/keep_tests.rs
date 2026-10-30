//! The copy that a create-only put leaves in the cache: what is kept and under what tag, what is
//! not kept and why, and what a body that fails or a delete that comes meanwhile leaves behind.
//! The races are driven by the gates of the upstream fixture and of the bodies; no test waits for
//! a time.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use tokio::sync::Semaphore;

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
fn at_rest(rig: &Rig) {
    assert_eq!(rig.staged_files(), 0);
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
    at_rest(&rig);
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
    at_rest(&rig);
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
    assert!(matches!(refused, Err(Error::Other(_))), "{refused:?}");
    assert!(rig.tamper.memory.head(&key).await.is_err());
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(&key));
    at_rest(&rig);
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
    assert!(matches!(refused.await, Err(Error::Other(_))));
    assert!(rig.tamper.memory.head(&key).await.is_err());
    assert_eq!(skipped(&rig, SkippedKeep::Budget), 0);
    at_rest(&rig);
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
    at_rest(&rig);
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
    at_rest(&rig);
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
    at_rest(&rig);
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
    at_rest(&rig);
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
    at_rest(&rig);
    assert!(super::locked(&rig.cache.inner.flights).is_empty());
    assert!(super::locked(&rig.cache.inner.invalidated).is_empty());
    assert!(matches!(rig.get(key).await, Err(Error::NotFound(_))));
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
    at_rest(&rig);
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
    at_rest(&rig);
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
