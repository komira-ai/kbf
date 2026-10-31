//! The writes of a cache that writes through, called on the cache as a backend: what each sends
//! to the upstream, in what order, and what the cache does with its own copy.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;

use super::testing::{Rig, pattern};
use crate::{Backend, Error, Op, PartBody};

/// A body of these chunks that counts the chunks the reader takes from it.
fn counted(chunks: &[&'static [u8]], polled: &Arc<AtomicUsize>) -> PartBody<'static> {
    let polled = Arc::clone(polled);
    stream::iter(chunks.to_vec())
        .map(move |chunk| {
            let _ = polled.fetch_add(1, Ordering::SeqCst);
            Ok(Bytes::from_static(chunk))
        })
        .boxed()
}

/// A body of one chunk.
fn body(bytes: &'static [u8]) -> PartBody<'static> {
    stream::once(async move { Ok(Bytes::from_static(bytes)) }).boxed()
}

/// The write calls of the backend with arguments that are valid for the keys given, in the
/// order of the trait: each gives what the call answered.
async fn write_calls(rig: &Rig) -> Vec<Result<(), Error>> {
    let cache = &rig.cache;
    let tag = "tag".to_owned();
    vec![
        cache.put("cas/b/w", Bytes::from_static(b"x")).await,
        cache.put_stream("cas/b/w", Some(1), body(b"x")).await,
        cache
            .put_if_match("cas/b/w", Bytes::from_static(b"x"), "tag")
            .await
            .map(|_| ()),
        cache.delete("cas/b/w").await,
        cache.create_upload("cas/b/w").await.map(|_| ()),
        cache
            .upload_part("cas/b/w", "id", 1, body(b"x"))
            .await
            .map(|_| ()),
        cache
            .upload_part_with_len("cas/b/w", "id", 1, Some(1), body(b"x"))
            .await
            .map(|_| ()),
        cache
            .complete_upload("cas/b/w", "id", &[(1, tag)])
            .await
            .map(|_| ()),
        cache.abort_upload("cas/b/w", "id").await,
    ]
}

/// The calls the upstream has had of the kinds a write makes.
fn writes_seen(rig: &Rig) -> u64 {
    let ops = [
        Op::Put,
        Op::PutIfMatch,
        Op::Delete,
        Op::CreateUpload,
        Op::UploadPart,
        Op::CompleteUpload,
        Op::AbortUpload,
        Op::Head,
    ];
    ops.iter().map(|op| rig.calls(*op)).sum()
}

/// Asserts that a cache without `write_through` answers every write call of the backend with
/// `Unsupported` and makes no call to the upstream, not even a HEAD: the flag off leaves the
/// cache a read cache below the front as well as in it. It fails on a cache that forwards a write
/// whatever the setting is.
#[tokio::test]
async fn without_write_through_every_write_call_is_unsupported() {
    let rig = Rig::new("tscache-write-off");
    for answer in write_calls(&rig).await {
        assert!(matches!(answer, Err(Error::Unsupported(_))), "{answer:?}");
    }
    assert_eq!(writes_seen(&rig), 0);
    assert!(!rig.cache.needs_length());
}

/// Asserts that with `write_through` every write call reaches the upstream, except the
/// replace on a tag, which stays `Unsupported` and reaches nothing: a create-only put (whole and
/// streamed), a delete, and the four calls of a multipart upload (the part in both its forms).
#[tokio::test]
async fn with_write_through_every_write_but_a_replace_reaches_the_upstream() {
    let rig = Rig::writing("tscache-write-on");
    let answers = write_calls(&rig).await;
    // `put_if_match` is the third call.
    assert!(
        matches!(answers[2], Err(Error::Unsupported(_))),
        "{:?}",
        answers[2]
    );
    assert_eq!(rig.calls(Op::PutIfMatch), 0);
    // The put and the streamed put are of one key: the second finds it, and the delete after
    // them removes it.
    assert_eq!(answers[0], Ok(()));
    assert!(matches!(answers[1], Err(Error::AlreadyExists(_))));
    assert_eq!(answers[3], Ok(()));
    // The upload calls reach the upstream, which refuses the id that was never given.
    assert_eq!(rig.calls(Op::CreateUpload), 1);
    assert_eq!(rig.calls(Op::UploadPart), 2);
    assert_eq!(rig.calls(Op::CompleteUpload), 1);
    assert_eq!(rig.calls(Op::AbortUpload), 1);
    assert_eq!(rig.calls(Op::Delete), 1);
}

/// Asserts a create-only put: it makes the object at the upstream with one HEAD and one put (the
/// HEAD before the body), under a prefix the cache keeps and under one it does not; the same key
/// again is `AlreadyExists` and the upstream gets no second put; the stage directory is empty
/// throughout, since nothing is kept.
#[tokio::test]
async fn a_create_only_put_heads_once_and_puts_once() {
    let rig = Rig::writing("tscache-write-create");
    for key in ["cas/b/new", "other/new"] {
        let chunks: [&'static [u8]; 3] = [b"one ", b"two ", b"three"];
        let polled = Arc::new(AtomicUsize::new(0));
        let (heads, puts) = (rig.calls(Op::Head), rig.calls(Op::Put));
        let made = rig
            .cache
            .put_stream(key, Some(13), counted(&chunks, &polled));
        assert_eq!(made.await, Ok(()));
        assert_eq!(rig.calls(Op::Head), heads + 1, "{key}");
        assert_eq!(rig.calls(Op::Put), puts + 1, "{key}");
        let stored = rig.tamper.memory.get(key, None).await.unwrap();
        assert_eq!(stored, Bytes::from_static(b"one two three"));

        let again = rig.cache.put_stream(key, Some(1), body(b"x")).await;
        assert_eq!(again, Err(Error::AlreadyExists(key.to_owned())));
        assert_eq!(rig.calls(Op::Put), puts + 1, "{key}");
        let stored = rig.tamper.memory.get(key, None).await.unwrap();
        assert_eq!(stored, Bytes::from_static(b"one two three"));
    }
    assert_eq!(rig.staged_files(), 0);
    assert_eq!(rig.cache.held(), (0, 0));
}

/// Asserts that a create-only put of a key that exists is refused before the body is read: no
/// chunk is taken from the body, the upstream gets no put, and the object is as it was. This is
/// what spares a large upload that the upstream would only refuse at the end. It catches a put
/// that streams first and learns of the duplicate from the upstream's conditional put.
#[tokio::test]
async fn a_duplicate_create_reads_no_byte_of_the_body() {
    let rig = Rig::writing("tscache-write-duplicate");
    rig.put("cas/b/there", b"first").await;
    let chunks: [&'static [u8]; 2] = [b"second", b"third"];
    let polled = Arc::new(AtomicUsize::new(0));
    let refused = rig
        .cache
        .put_stream("cas/b/there", Some(11), counted(&chunks, &polled))
        .await;
    assert_eq!(refused, Err(Error::AlreadyExists("cas/b/there".to_owned())));
    assert_eq!(polled.load(Ordering::SeqCst), 0);
    assert_eq!(rig.calls(Op::Put), 0);
    assert_eq!(rig.calls(Op::Head), 1);
    let stored = rig.tamper.memory.get("cas/b/there", None).await.unwrap();
    assert_eq!(stored, Bytes::from_static(b"first"));
}

/// Asserts that the HEAD before a put only saves work and is not the arbiter: when the upstream
/// answers the HEAD with "not found" and then refuses the put because another writer got there,
/// the put is `AlreadyExists`; an upstream that cannot answer the HEAD fails the put with its
/// error and no body is read.
#[tokio::test]
async fn the_upstream_conditional_put_decides_a_race_and_a_failed_head_fails_the_put() {
    let rig = Rig::writing("tscache-write-race");
    rig.upstream
        .fail_next_with(Op::Put, Error::AlreadyExists("cas/b/raced".to_owned()));
    let lost = rig
        .cache
        .put_stream("cas/b/raced", Some(1), body(b"x"))
        .await;
    assert_eq!(lost, Err(Error::AlreadyExists("cas/b/raced".to_owned())));
    assert!(rig.tamper.memory.head("cas/b/raced").await.is_err());

    let down = Error::Unavailable("head".to_owned());
    rig.upstream.fail_next_with(Op::Head, down.clone());
    let polled = Arc::new(AtomicUsize::new(0));
    let chunks: [&'static [u8]; 1] = [b"x"];
    let puts = rig.calls(Op::Put);
    let failed = rig
        .cache
        .put_stream("cas/b/other", Some(1), counted(&chunks, &polled))
        .await;
    assert_eq!(failed, Err(down));
    assert_eq!(
        (polled.load(Ordering::SeqCst), rig.calls(Op::Put)),
        (0, puts)
    );
}

/// Asserts that the keys a storage node keeps for itself are refused with `Reserved` by every
/// write call that names a key, and that the upstream is not called.
#[tokio::test]
async fn reserved_keys_are_refused_by_every_write() {
    let rig = Rig::writing("tscache-write-reserved");
    for key in ["_journal/p1/0001", "_tiering/lease"] {
        let cache = &rig.cache;
        let results = [
            cache.put(key, Bytes::from_static(b"x")).await,
            cache.put_stream(key, Some(1), body(b"x")).await,
            cache.delete(key).await,
            cache.create_upload(key).await.map(|_| ()),
            cache
                .upload_part(key, "id", 1, body(b"x"))
                .await
                .map(|_| ()),
            cache
                .complete_upload(key, "id", &[(1, "t".to_owned())])
                .await
                .map(|_| ()),
            cache.abort_upload(key, "id").await,
        ];
        for result in results {
            assert_eq!(result, Err(Error::Reserved(key.to_owned())), "{key}");
        }
    }
    assert_eq!(writes_seen(&rig), 0);
}

/// Asserts a multipart upload through the cache: the object is made at the upstream from the
/// parts in the order named, the completion is create-only, and an aborted upload makes nothing.
#[tokio::test]
async fn a_multipart_upload_passes_through_to_the_upstream() {
    let rig = Rig::writing("tscache-write-multipart");
    let cache = &rig.cache;
    let id = cache.create_upload("cas/b/big").await.unwrap();
    let one = cache
        .upload_part("cas/b/big", &id, 1, body(b"hello "))
        .await;
    let two = cache
        .upload_part_with_len("cas/b/big", &id, 2, Some(5), body(b"world"))
        .await;
    let parts = [(1, one.unwrap()), (2, two.unwrap())];
    cache
        .complete_upload("cas/b/big", &id, &parts)
        .await
        .unwrap();
    let stored = rig.tamper.memory.get("cas/b/big", None).await.unwrap();
    assert_eq!(stored, Bytes::from_static(b"hello world"));
    assert_eq!(rig.cache.held(), (0, 0));

    let again = cache.create_upload("cas/b/big").await.unwrap();
    let part = cache.upload_part("cas/b/big", &again, 1, body(b"x")).await;
    let parts = [(1, part.unwrap())];
    let refused = cache.complete_upload("cas/b/big", &again, &parts).await;
    assert!(
        matches!(refused, Err(Error::AlreadyExists(_))),
        "{refused:?}"
    );
    assert_eq!(
        stored,
        rig.tamper.memory.get("cas/b/big", None).await.unwrap()
    );

    let gone = cache.create_upload("cas/b/gone").await.unwrap();
    cache.abort_upload("cas/b/gone", &gone).await.unwrap();
    assert!(rig.tamper.memory.head("cas/b/gone").await.is_err());
    assert_eq!(
        rig.tamper
            .memory
            .list_uploads("", None, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

/// Asserts that a write that makes an object drops the cache's own entry for the key, when the
/// cache has one: the entry is of an object that was deleted at the upstream behind the cache's
/// back (a create-only write only succeeds where nothing is), and the next read is of the new
/// object. It covers the completion of an upload and the streamed put; a cache that kept the
/// entry would serve the old bytes.
#[tokio::test]
async fn a_write_that_makes_an_object_drops_the_entry_for_the_key() {
    let rig = Rig::writing("tscache-write-stale");
    let old = pattern(100);
    for key in ["cas/b/up", "cas/b/put"] {
        rig.put(key, &old).await;
        assert_eq!(rig.get(key).await.unwrap(), old);
        rig.tamper.memory.delete(key).await.unwrap();
    }
    assert_eq!(rig.cache.held(), (200, 2));

    let id = rig.cache.create_upload("cas/b/up").await.unwrap();
    let part = rig
        .cache
        .upload_part("cas/b/up", &id, 1, body(b"new up"))
        .await;
    let parts = [(1, part.unwrap())];
    rig.cache
        .complete_upload("cas/b/up", &id, &parts)
        .await
        .unwrap();
    assert!(!rig.has_file("cas/b/up"));
    assert_eq!(rig.cache.held(), (100, 1));
    assert_eq!(
        rig.get("cas/b/up").await.unwrap(),
        Bytes::from_static(b"new up")
    );

    let made = rig
        .cache
        .put_stream("cas/b/put", Some(7), body(b"new put"))
        .await;
    assert_eq!(made, Ok(()));
    assert!(!rig.has_file("cas/b/put"));
    assert_eq!(rig.cache.held(), (6, 1));
    assert_eq!(
        rig.get("cas/b/put").await.unwrap(),
        Bytes::from_static(b"new put")
    );
}

/// Asserts a delete: the entry is dropped before the upstream is asked, so that a delete the
/// upstream fails still leaves no local copy to serve, and the delete that goes through removes
/// the object from the cache and the upstream, and deleting what is not there succeeds.
#[tokio::test]
async fn a_delete_drops_the_entry_then_deletes_at_the_upstream() {
    let rig = Rig::writing("tscache-write-delete");
    rig.put("cas/b/obj", &pattern(100)).await;
    assert_eq!(rig.get("cas/b/obj").await.unwrap().len(), 100);
    assert_eq!(rig.cache.held(), (100, 1));

    let down = Error::Unavailable("delete".to_owned());
    rig.upstream.fail_next_with(Op::Delete, down.clone());
    assert_eq!(rig.cache.delete("cas/b/obj").await, Err(down));
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/obj"));
    assert!(rig.tamper.memory.head("cas/b/obj").await.is_ok());

    assert_eq!(rig.get("cas/b/obj").await.unwrap().len(), 100);
    assert_eq!(rig.cache.held(), (100, 1));
    assert_eq!(rig.cache.delete("cas/b/obj").await, Ok(()));
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/obj"));
    assert!(rig.tamper.memory.head("cas/b/obj").await.is_err());
    assert_eq!(rig.cache.delete("cas/b/obj").await, Ok(()));
    assert!(matches!(
        rig.get("cas/b/obj").await,
        Err(Error::NotFound(_))
    ));
}

/// Asserts that the cache asks for the length of a streamed body when its upstream does (an S3
/// upstream holds a body whose length it is not told), and only when it writes: a read cache
/// takes no body.
#[tokio::test]
async fn the_cache_needs_a_length_when_it_writes_and_its_upstream_does() {
    let writing = Rig::writing("tscache-write-length");
    assert!(!writing.cache.needs_length());
    writing.tamper.needs_len.store(true, Ordering::SeqCst);
    assert!(writing.cache.needs_length());
    let reading = Rig::new("tscache-read-length");
    reading.tamper.needs_len.store(true, Ordering::SeqCst);
    assert!(!reading.cache.needs_length());
}
