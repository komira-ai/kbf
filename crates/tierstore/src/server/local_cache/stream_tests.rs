//! The reads of the cache as streams: a read holds one chunk at a time, a miss is given to the
//! reader as the fill writes it, and a body that the fill finds bad never ends cleanly.
//!
//! The rigs here use a chunk of 100 bytes and objects of 1000, so that an object is ten chunks.
//! The upstream's reads are held by a gate that a test opens a permit at a time, which is how a
//! test stops a fill half way without a clock.

use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures::StreamExt as _;
use tokio::sync::Semaphore;

use super::fill::{Filled, Pending};
use super::layout::name_of;
use super::locked;
use super::metrics::{FailedFill, read};
use super::testing::{Quirk, Rig, digest, pattern, until};
use crate::{Backend, ByteRange, Error, Op, ReadBody};

const CHUNK: u64 = 100;
const SIZE: usize = 1000;

/// A cache with a chunk of [`CHUNK`] bytes, called `name`.
fn rig(name: &str) -> Rig {
    Rig::with(name, |config| config.fill_chunk = CHUNK)
}

/// The number of entries in the stage directory of the rig.
fn staged(rig: &Rig) -> usize {
    std::fs::read_dir(rig.config.path.join(".stage"))
        .unwrap()
        .count()
}

/// The chunks of `body` to its end, and the error that ended it, if one did.
async fn drain(mut body: ReadBody) -> (Vec<Bytes>, Option<Error>) {
    let mut chunks = Vec::new();
    while let Some(next) = body.next().await {
        match next {
            Ok(chunk) => chunks.push(chunk),
            Err(error) => return (chunks, Some(error)),
        }
    }
    (chunks, None)
}

/// The bytes of `chunks` joined.
fn joined(chunks: &[Bytes]) -> Vec<u8> {
    chunks.concat()
}

/// Takes chunks from `body` until `need` bytes are in them, and gives them.
async fn take(body: &mut ReadBody, need: usize) -> Vec<u8> {
    let mut taken = Vec::new();
    while taken.len() < need {
        let chunk = body.next().await.expect("a chunk").expect("not an error");
        taken.extend_from_slice(&chunk);
    }
    taken
}

/// Yields until the cache has counted `count` misses (each is counted just before its reader
/// joins or starts the fill of its key, with no wait between).
async fn until_misses(rig: &Rig, count: u64) {
    until("the misses were counted", || {
        read(&rig.cache.counters().misses) >= count
    })
    .await;
}

/// Opens `permits` reads of the upstream.
fn open(gate: &Semaphore, permits: usize) {
    gate.add_permits(permits);
}

/// Asserts that a hit is read from the file a chunk at a time: ten chunks of 100 bytes for an
/// object of 1000, none larger, equal to the object, counted as a hit and as local bytes, and
/// with no call to the upstream. The chunk is the most a read holds, so the memory of a read
/// does not depend on the size of the object.
#[tokio::test]
async fn a_hit_is_streamed_in_chunks_no_larger_than_the_chunk() {
    let rig = rig("tsstream-hit");
    let body = pattern(SIZE);
    rig.put("cas/b/obj", &body).await;
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    let calls = (rig.calls(Op::Head), rig.calls(Op::Get));

    let stream = rig.cache.get_stream("cas/b/obj", None).await.unwrap();
    let (chunks, error) = drain(stream).await;
    assert_eq!(error, None);
    assert_eq!(chunks.len(), 10);
    assert!(chunks.iter().all(|chunk| chunk.len() as u64 <= CHUNK));
    assert_eq!(joined(&chunks), body);
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.hits), 1);
    assert_eq!(read(&counters.bytes_served_local), SIZE as u64);
    assert_eq!((rig.calls(Op::Head), rig.calls(Op::Get)), calls);

    let range = ByteRange::new(150, 300);
    let stream = rig
        .cache
        .get_stream("cas/b/obj", Some(range))
        .await
        .unwrap();
    let (chunks, error) = drain(stream).await;
    assert_eq!(error, None);
    assert_eq!(joined(&chunks), &body[150..450]);
    assert!(chunks.iter().all(|chunk| chunk.len() as u64 <= CHUNK));
}

/// Asserts that a read that goes to the upstream (a key under no policy) asks for one chunk at
/// a time and only when the one before has been taken: after the call returns, one read has
/// been made (the first chunk is read at once, so an object that is not there is the error of
/// the call); each chunk taken makes one more; the size is known from a HEAD, so no read is made to
/// find the end. An object is never read whole, whatever its size.
#[tokio::test]
async fn a_read_that_passes_through_asks_the_upstream_one_chunk_at_a_time() {
    let rig = rig("tsstream-passthrough");
    let body = pattern(SIZE);
    rig.put("plain/obj", &body).await;
    let mut stream = rig.cache.get_stream("plain/obj", None).await.unwrap();
    assert_eq!(rig.calls(Op::Get), 1);
    let mut got = take(&mut stream, 100).await;
    for taken in 2..=10 {
        assert_eq!(rig.calls(Op::Get), taken - 1);
        got.extend_from_slice(&stream.next().await.unwrap().unwrap());
        assert_eq!(rig.calls(Op::Get), taken);
    }
    assert_eq!(got, body);
    assert!(stream.next().await.is_none());
    // One HEAD before the first chunk and one before the last, ten reads, none for the end.
    assert_eq!((rig.calls(Op::Head), rig.calls(Op::Get)), (2, 10));
    assert_eq!(rig.cache.held(), (0, 0));

    let absent = rig.cache.get_stream("plain/none", None).await;
    assert!(matches!(absent, Err(Error::NotFound(_))));
}

/// Asserts that a miss is given to the reader while the fill is still running: with the upstream
/// opened for one chunk only, the first chunk of the object reaches the reader, while the fill
/// has written 100 bytes, nothing is in the cache and one file is staged. Then, a second reader
/// that joins the running fill is given the object from its first byte, from the staged file.
/// When the upstream is opened both have the object,
/// the upstream was read ten times and not twenty, and the object is kept.
#[tokio::test]
async fn a_miss_is_streamed_while_it_is_filled_and_a_late_reader_reads_the_staged_file() {
    let rig = rig("tsstream-miss");
    let body = pattern(SIZE);
    rig.put("cas/b/obj", &body).await;
    let gate = rig.tamper.hold();
    open(&gate, 1);

    let mut first = rig.cache.get_stream("cas/b/obj", None).await.unwrap();
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.staging_files), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/obj"));

    let mut second = rig.cache.get_stream("cas/b/obj", None).await.unwrap();
    assert_eq!(read(&counters.misses), 2);
    assert_eq!(read(&counters.fills), 0);
    let (head, again) = (take(&mut first, 100).await, take(&mut second, 100).await);
    assert_eq!(head, &body[..100]);
    assert_eq!(again, &body[..100]);

    open(&gate, 64);
    for (rest, mut have) in [(first, head), (second, again)] {
        let (chunks, error) = drain(rest).await;
        assert_eq!(error, None);
        have.extend_from_slice(&joined(&chunks));
        assert_eq!(have, body);
    }
    // Two HEADs (before the first chunk and after the last), ten reads, one fill.
    assert_eq!((rig.calls(Op::Head), rig.calls(Op::Get)), (2, 10));
    assert_eq!(read(&counters.fills), 1);
    assert_eq!(read(&counters.bytes_served_upstream), 2 * SIZE as u64);
    assert_eq!(rig.cache.held(), (SIZE as u64, 1));
    assert_eq!(staged(&rig), 0);
    assert!(locked(&rig.cache.inner.flights).is_empty());
}

/// Asserts that many readers of one miss, started together, all get the object and that one
/// fetch is made: ten reads and two HEADs reach the upstream, one fill is counted, and every
/// reader's chunks are no larger than the chunk.
#[tokio::test]
async fn concurrent_readers_of_a_miss_get_the_bytes_from_one_fetch() {
    let rig = rig("tsstream-many");
    let body = pattern(SIZE);
    rig.put("cas/b/obj", &body).await;
    let gate = rig.tamper.hold();
    let readers: Vec<_> = (0..6)
        .map(|_| {
            let cache = rig.cache.clone();
            tokio::spawn(async move {
                let stream = cache.get_stream("cas/b/obj", None).await.unwrap();
                drain(stream).await
            })
        })
        .collect();
    until_misses(&rig, 6).await;
    open(&gate, 64);
    for reader in readers {
        let (chunks, error) = reader.await.unwrap();
        assert_eq!(error, None);
        assert!(chunks.iter().all(|chunk| chunk.len() as u64 <= CHUNK));
        assert_eq!(joined(&chunks), body);
    }
    assert_eq!((rig.calls(Op::Head), rig.calls(Op::Get)), (2, 10));
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.fills), 1);
    assert_eq!(read(&counters.bytes_served_upstream), 6 * SIZE as u64);
    assert_eq!(rig.cache.held(), (SIZE as u64, 1));
}

/// Asserts that a fill that fails the hash after the reader was given bytes ends the reader's body
/// with an error and never cleanly: the upstream is opened for five chunks, the reader takes 500
/// bytes (the fill is running, one file staged, nothing published), then the upstream is opened
/// for the rest; the body gives at most 900 bytes more, all of them the upstream's, the last
/// 100 bytes are never given, and it ends with an `Unavailable` error. Nothing is kept, staged or
/// left in the table of fills, and the failure is counted once.
#[tokio::test]
async fn a_fill_that_fails_the_hash_after_streaming_ends_the_body_with_an_error() {
    let rig = rig("tsstream-mismatch");
    let wrong = vec![7u8; SIZE];
    let key = format!("cas/l/{}", digest(&pattern(SIZE)));
    rig.put(&key, &wrong).await;
    let gate = rig.tamper.hold();
    open(&gate, 5);
    let mut body = rig.cache.get_stream(&key, None).await.unwrap();
    let before = take(&mut body, 500).await;
    assert_eq!(before, &wrong[..500]);
    assert_eq!(read(&rig.cache.counters().staging_files), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(&key));

    open(&gate, 64);
    let (chunks, error) = drain(body).await;
    let error = error.expect("the body ends with the error of the fill");
    assert!(error.is_unavailable(), "{error}");
    let given = before.len() + joined(&chunks).len();
    assert!(given <= SIZE - CHUNK as usize, "{given} bytes were given");
    assert_eq!(joined(&chunks), &wrong[500..given]);
    let counters = rig.cache.counters();
    assert_eq!(counters.failures(FailedFill::HashMismatch), 1);
    assert_eq!(read(&counters.fills), 0);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(&key));
    assert_eq!(staged(&rig), 0);
    assert!(locked(&rig.cache.inner.flights).is_empty());
}

/// Asserts that a fill that ends without an object for a reason of the cache (here a link the
/// file system refuses, after the whole object was written) does not fail a reader that was given
/// bytes: the rest comes from the upstream, from the byte after the last one given, to the end of
/// the window asked. Both a whole read and a ranged one (150 bytes in, 400 long) are equal to
/// the object's bytes, the failure is counted as a local error, and nothing is kept or staged.
#[tokio::test]
async fn a_fill_that_cannot_publish_is_finished_from_the_upstream_mid_stream() {
    for (name, range, need) in [
        ("whole", None, 500),
        ("ranged", Some(ByteRange::new(250, 400)), 250),
    ] {
        let rig = rig(&format!("tsstream-resume-{name}"));
        let body = pattern(SIZE);
        rig.put("cas/b/obj", &body).await;
        rig.disk
            .set(Quirk::LinkFails(std::io::ErrorKind::PermissionDenied));
        let gate = rig.tamper.hold();
        open(&gate, 5);
        let mut stream = rig.cache.get_stream("cas/b/obj", range).await.unwrap();
        let mut got = take(&mut stream, need).await;
        open(&gate, 64);
        let (chunks, error) = drain(stream).await;
        assert_eq!(error, None, "{name}");
        got.extend_from_slice(&joined(&chunks));
        let wanted = range.map_or(&body[..], |r| {
            &body[r.offset() as usize..(r.offset() + r.size()) as usize]
        });
        assert_eq!(got, wanted, "{name}");
        assert_eq!(rig.cache.counters().failures(FailedFill::LocalIo), 1);
        assert_eq!(rig.cache.held(), (0, 0));
        assert_eq!(staged(&rig), 0);
    }
}

/// Asserts that a range past the end of an object that is being filled is the error of the call,
/// as for a hit, and that the fill goes on and keeps the object.
#[tokio::test]
async fn a_range_past_the_end_of_a_miss_is_the_error_of_the_call() {
    let rig = rig("tsstream-past-end");
    rig.put("cas/b/obj", &pattern(SIZE)).await;
    let range = ByteRange::new(SIZE as u64 + 1, 10);
    let error = rig.cache.get_stream("cas/b/obj", Some(range)).await;
    assert!(matches!(error, Err(Error::Other(_))));
    until("the fill ended", || read(&rig.cache.counters().fills) >= 1).await;
    assert_eq!(rig.cache.held(), (SIZE as u64, 1));
}

/// Asserts that a reader that goes away leaves the fill running to its end, and the other reader
/// of the same fill is given the whole object.
#[tokio::test]
async fn a_reader_that_goes_away_does_not_stop_the_fill_for_the_others() {
    let rig = rig("tsstream-dropped");
    let body = pattern(SIZE);
    rig.put("cas/b/obj", &body).await;
    let gate = rig.tamper.hold();
    open(&gate, 1);
    let leaving = rig.cache.get_stream("cas/b/obj", None).await.unwrap();
    let staying = rig.cache.get_stream("cas/b/obj", None).await.unwrap();
    drop(leaving);
    open(&gate, 64);
    let (chunks, error) = drain(staying).await;
    assert_eq!(error, None);
    assert_eq!(joined(&chunks), body);
    assert_eq!(rig.cache.inner.reserved.load(Ordering::SeqCst), 0);
    assert_eq!(rig.cache.held(), (SIZE as u64, 1));
    let name = name_of("cas/b/obj");
    assert!(rig.cache.inner.kept(&name).is_some());
}

/// Asserts that a body that ended with the error of its fill gives nothing more when it is read
/// again: the error is the last item, and the next read is the end of the stream, not a second
/// error and not bytes.
#[tokio::test]
async fn a_body_that_ended_with_an_error_gives_nothing_after_it() {
    let rig = rig("tsstream-after-error");
    let key = format!("cas/l/{}", digest(&pattern(SIZE)));
    rig.put(&key, &vec![7u8; SIZE]).await;
    let gate = rig.tamper.hold();
    open(&gate, 5);
    let mut body = rig.cache.get_stream(&key, None).await.unwrap();
    let _ = take(&mut body, 500).await;
    open(&gate, 64);
    let mut ended = false;
    while let Some(next) = body.next().await {
        if next.is_err() {
            ended = true;
            break;
        }
    }
    assert!(ended);
    assert!(body.next().await.is_none());
}

/// Asserts that the read that follows a fill (the object is in the cache when the reader looks
/// again) is served from the file and counted as bytes from the upstream, not as a hit: no hit is
/// counted, the bytes served from the upstream are the object's size, and the upstream is not read.
#[tokio::test]
async fn the_read_that_follows_a_fill_is_served_from_the_file_and_not_counted_as_a_hit() {
    let rig = rig("tsstream-after-fill");
    let body = pattern(SIZE);
    rig.put("cas/b/obj", &body).await;
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    let counters = rig.cache.counters();
    let before = read(&counters.bytes_served_upstream);
    let gets = rig.calls(Op::Get);
    let name = name_of("cas/b/obj");
    let stream = rig.cache.inner.open_kept(&name, None, false).await;
    let (chunks, error) = drain(stream.unwrap().expect("the object is kept")).await;
    assert_eq!(error, None);
    assert_eq!(joined(&chunks), body);
    assert_eq!(read(&counters.hits), 0);
    assert_eq!(read(&counters.bytes_served_upstream) - before, SIZE as u64);
    assert_eq!(rig.calls(Op::Get), gets);
}

/// A cache that takes writes, with a chunk of [`CHUNK`] bytes, called `name`.
fn writing_rig(name: &str) -> Rig {
    Rig::with(name, |config| {
        config.fill_chunk = CHUNK;
        config.write_through = true;
    })
}

/// A key named by its content: a fill of it is stopped by nothing but a write or a delete.
fn content_key(bytes: &[u8]) -> String {
    format!("cas/l/{}", digest(bytes))
}

/// What a reader of `get_stream` ends with: the error of the call, or the bytes and the error of
/// the body.
type Ended = Result<(Vec<Bytes>, Option<Error>), Error>;

/// Reads `key` through the cache to the end.
async fn read_through(cache: super::LocalCache, key: String, range: Option<ByteRange>) -> Ended {
    let body = cache.get_stream(&key, range).await?;
    Ok(drain(body).await)
}

/// Asserts that a read that goes on past a same-size overwrite never ends cleanly: a pass-through
/// of ten chunks whose object is written over (the same size, other bytes) after the first chunk
/// ends with an `Unavailable` error and gives at most nine chunks, the last never given. Without
/// a check of the version the length is right and the body ends with 200.
#[tokio::test]
async fn an_overwrite_between_the_chunks_of_a_pass_through_ends_the_body_with_an_error() {
    let rig = rig("tsstream-overwrite-pass");
    rig.put("plain/obj", &pattern(SIZE)).await;
    let mut body = rig.cache.get_stream("plain/obj", None).await.unwrap();
    let first = take(&mut body, 100).await;
    rig.replace("plain/obj", &vec![3u8; SIZE]).await;
    let mut body = body;
    let mut given = first.len();
    let error = loop {
        match body.next().await.expect("the body ends with its error") {
            Ok(chunk) => given += chunk.len(),
            Err(error) => break error,
        }
    };
    assert!(error.is_unavailable(), "{error}");
    assert_eq!(given, SIZE - CHUNK as usize);
    // A body that ended with an error gives nothing after it.
    assert!(body.next().await.is_none());
}

/// Asserts the same for a read that goes on from the upstream after a fill that ended without an
/// object (a link the file system refuses): the bytes given came from the first version, so the
/// upstream's other version of the same size ends the body with an `Unavailable` error.
#[tokio::test]
async fn an_overwrite_before_the_rest_of_a_resumed_read_ends_the_body_with_an_error() {
    let rig = rig("tsstream-overwrite-tail");
    rig.put("cas/b/obj", &pattern(SIZE)).await;
    rig.disk
        .set(Quirk::LinkFails(std::io::ErrorKind::PermissionDenied));
    let gate = rig.tamper.hold();
    open(&gate, 64);
    let body = rig.cache.get_stream("cas/b/obj", None).await.unwrap();
    let counters = rig.cache.counters();
    until("the fill failed", || {
        counters.failures(FailedFill::LocalIo) >= 1
    })
    .await;
    rig.replace("cas/b/obj", &vec![3u8; SIZE]).await;
    let (chunks, error) = drain(body).await;
    let error = error.expect("a body of two versions does not end cleanly");
    assert!(error.is_unavailable(), "{error}");
    assert!(joined(&chunks).len() < SIZE);
}

/// Asserts that a pass-through whose upstream gives fewer bytes than the object has is the
/// error of the call, not a short body that ends well.
#[tokio::test]
async fn a_pass_through_that_the_upstream_cuts_short_is_an_error() {
    let rig = rig("tsstream-pass-short");
    rig.put("plain/obj", &pattern(SIZE)).await;
    rig.tamper.short.store(true, Ordering::SeqCst);
    let error = rig.cache.get_stream("plain/obj", None).await;
    assert!(matches!(error, Err(ref e) if e.is_unavailable()));
}

/// Asserts what a delete does to the readers of a fill it overtakes, one of them given no byte
/// (a range of 50 bytes, held until the fill is checked) and one given 500: the first is answered
/// from the upstream, which no longer has the object (`NotFound`); the second, whose bytes came
/// from the object that is gone, ends with that error and no clean end. Nothing is kept or staged.
#[tokio::test]
async fn a_delete_during_a_fill_gives_each_reader_the_upstreams_answer() {
    let rig = writing_rig("tsstream-forget-delete");
    let bytes = pattern(SIZE);
    let key = content_key(&bytes);
    rig.put(&key, &bytes).await;
    let gate = rig.tamper.hold();
    open(&gate, 5);
    let mut mid = rig.cache.get_stream(&key, None).await.unwrap();
    let given = take(&mut mid, 500).await;
    let zero = tokio::spawn(read_through(
        rig.cache.clone(),
        key.clone(),
        Some(ByteRange::new(0, 50)),
    ));
    until_misses(&rig, 2).await;

    assert_eq!(rig.cache.delete(&key).await, Ok(()));
    open(&gate, 64);
    let zero = zero.await.unwrap();
    assert!(matches!(zero, Err(Error::NotFound(_))));
    let (rest, error) = drain(mid).await;
    assert!(matches!(error, Some(Error::NotFound(_))), "{error:?}");
    assert!(given.len() + joined(&rest).len() <= SIZE - CHUNK as usize);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(&key));
    assert_eq!(staged(&rig), 0);
    assert!(locked(&rig.cache.inner.flights).is_empty());
    assert!(locked(&rig.cache.inner.invalidated).is_empty());
}

/// Asserts what a write through the cache does to the readers of a fill it overtakes: the object
/// is deleted and written again with the same bytes (the same tag) and the cache is told, as a
/// write through it tells it. The fill does not keep what it read; the reader given no byte and
/// the reader given 500 both go on from the upstream, which has the same version, and each gets
/// the object's bytes and a clean end.
#[tokio::test]
async fn a_write_during_a_fill_has_each_reader_go_on_from_the_upstream() {
    let rig = writing_rig("tsstream-forget-write");
    let bytes = pattern(SIZE);
    let key = content_key(&bytes);
    rig.put(&key, &bytes).await;
    let gate = rig.tamper.hold();
    open(&gate, 5);
    let mut mid = rig.cache.get_stream(&key, None).await.unwrap();
    let mut given = take(&mut mid, 500).await;
    let zero = tokio::spawn(read_through(
        rig.cache.clone(),
        key.clone(),
        Some(ByteRange::new(0, 50)),
    ));
    until_misses(&rig, 2).await;

    rig.replace(&key, &bytes).await;
    rig.cache.inner.forget(&key);
    open(&gate, 64);
    let (chunks, error) = zero.await.unwrap().expect("the object is there");
    assert_eq!(error, None);
    assert_eq!(joined(&chunks), &bytes[..50]);
    let (rest, error) = drain(mid).await;
    assert_eq!(error, None);
    given.extend_from_slice(&joined(&rest));
    assert_eq!(given, bytes);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(&key));
    assert_eq!(staged(&rig), 0);
    assert!(locked(&rig.cache.inner.invalidated).is_empty());
}

/// Asserts that a fill that panics after a reader was given bytes ends that reader's body with an
/// `Unavailable` error, counted once as aborted, and leaves no key, permit or file behind.
#[tokio::test]
async fn a_fill_that_panics_after_bytes_were_given_ends_the_body_with_an_error() {
    let rig = rig("tsstream-panic");
    rig.put("cas/b/obj", &pattern(SIZE)).await;
    let gate = rig.tamper.hold();
    open(&gate, 5);
    let mut body = rig.cache.get_stream("cas/b/obj", None).await.unwrap();
    let _ = take(&mut body, 500).await;
    rig.tamper.panic_once.store(true, Ordering::SeqCst);
    open(&gate, 64);
    let (_, error) = drain(body).await;
    assert!(error.is_some_and(|error| error.is_unavailable()));
    let inner = &rig.cache.inner;
    assert_eq!(rig.cache.counters().failures(FailedFill::Aborted), 1);
    assert!(locked(&inner.flights).is_empty());
    assert_eq!(
        inner.permits.available_permits(),
        rig.config.max_concurrent_fills
    );
    assert_eq!(staged(&rig), 0);
    assert_eq!(rig.cache.held(), (0, 0));
}

/// Asserts that a staged file that is removed while a reader reads it does not fail the reader
/// (it reads the descriptor, not the name): the fill cannot link the file and ends without an
/// object, and the reader goes on from the upstream, which has the same version, to a clean end
/// with every byte.
#[tokio::test]
async fn a_staged_file_removed_while_it_is_read_does_not_fail_the_reader() {
    let rig = rig("tsstream-removed");
    let body = pattern(SIZE);
    rig.put("cas/b/obj", &body).await;
    let gate = rig.tamper.hold();
    open(&gate, 5);
    let mut stream = rig.cache.get_stream("cas/b/obj", None).await.unwrap();
    let mut got = take(&mut stream, 500).await;
    for entry in std::fs::read_dir(rig.config.path.join(".stage")).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    open(&gate, 64);
    let (rest, error) = drain(stream).await;
    assert_eq!(error, None);
    got.extend_from_slice(&joined(&rest));
    assert_eq!(got, body);
    assert_eq!(rig.cache.counters().failures(FailedFill::LocalIo), 1);
    assert_eq!(rig.cache.held(), (0, 0));
}

/// Asserts that a read of a window that runs past the end of the object (the last 50 bytes of a
/// range of 100) gives the bytes there are and ends cleanly, as a backend's range does, and that
/// an upstream that gives more bytes than were asked is the error of the call.
#[tokio::test]
async fn a_window_past_the_end_is_cut_there_and_an_upstream_that_gives_more_is_an_error() {
    let rig = rig("tsstream-window-end");
    let body = pattern(SIZE);
    rig.put("plain/obj", &body).await;
    let range = ByteRange::new(950, 100);
    let stream = rig
        .cache
        .get_stream("plain/obj", Some(range))
        .await
        .unwrap();
    let (chunks, error) = drain(stream).await;
    assert_eq!(error, None);
    assert_eq!(joined(&chunks), &body[950..]);

    rig.tamper.long.store(true, Ordering::SeqCst);
    let range = ByteRange::new(0, 50);
    let error = rig.cache.get_stream("plain/obj", Some(range)).await;
    assert!(matches!(error, Err(Error::Other(_))));
}

/// Asserts the two ends of an object written over during its fill (a key not named by its
/// content, so the second HEAD finds another tag): a reader given no byte (a range of 50 bytes,
/// held until the fill is checked) is answered from the upstream with the new bytes, as a fresh
/// read would be; a reader given 500 bytes of the old object gets an `Unavailable` error, never a
/// body of two versions. The fill is counted as changed and keeps nothing.
#[tokio::test]
async fn a_reader_given_bytes_of_an_object_written_over_gets_the_error_and_one_given_none_the_new_bytes()
 {
    let rig = rig("tsstream-written-over");
    rig.put("cas/b/obj", &pattern(SIZE)).await;
    let gate = rig.tamper.hold();
    open(&gate, 5);
    let mut mid = rig.cache.get_stream("cas/b/obj", None).await.unwrap();
    let given = take(&mut mid, 500).await;
    let zero = tokio::spawn(read_through(
        rig.cache.clone(),
        "cas/b/obj".to_owned(),
        Some(ByteRange::new(0, 50)),
    ));
    until_misses(&rig, 2).await;

    let new = vec![3u8; SIZE];
    rig.replace("cas/b/obj", &new).await;
    open(&gate, 64);
    let (chunks, error) = zero.await.unwrap().expect("the object is there");
    assert_eq!(error, None);
    assert_eq!(joined(&chunks), &new[..50]);
    let (rest, error) = drain(mid).await;
    let error = error.expect("a body of two versions does not end cleanly");
    assert!(error.is_unavailable(), "{error}");
    assert!(given.len() + joined(&rest).len() <= SIZE - CHUNK as usize);
    assert_eq!(rig.cache.counters().failures(FailedFill::Changed), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/obj"));
    assert_eq!(staged(&rig), 0);
}

/// Asserts that an upstream that gives no content tag is refused where a version is needed: a fill
/// of its object is the reader's error (and nothing is kept), and so is a whole pass-through, which
/// is held to one version; a read of one chunk or less, which needs no version, is served.
#[tokio::test]
async fn an_upstream_without_content_tags_is_refused_where_a_version_is_needed() {
    let rig = rig("tsstream-blank-tags");
    let body = pattern(SIZE);
    rig.put("cas/b/obj", &body).await;
    rig.put("plain/obj", &body).await;
    rig.tamper.blank_tags.store(true, Ordering::SeqCst);
    let fill = rig.cache.get_stream("cas/b/obj", None).await;
    assert!(matches!(fill, Err(Error::Unsupported(_))));
    assert_eq!(rig.cache.held(), (0, 0));
    let whole = rig.cache.get_stream("plain/obj", None).await;
    assert!(matches!(whole, Err(Error::Unsupported(_))));
    let range = ByteRange::new(10, 50);
    let stream = rig
        .cache
        .get_stream("plain/obj", Some(range))
        .await
        .unwrap();
    let (chunks, error) = drain(stream).await;
    assert_eq!(error, None);
    assert_eq!(joined(&chunks), &body[10..60]);
}

/// Asserts that a read that finds the place of a keep in the table of the fills that run (a fill
/// that is over already, with no object) does not wait and starts no fill: it is answered from the
/// upstream, from its first byte to the last, and nothing is kept.
#[tokio::test]
async fn a_read_that_finds_the_place_of_a_keep_is_answered_from_the_upstream() {
    let rig = rig("tsstream-keep-place");
    let body = pattern(SIZE);
    rig.put("cas/b/obj", &body).await;
    let hex = name_of("cas/b/obj").hex;
    drop(locked(&rig.cache.inner.flights).insert(hex, Pending::finished(Filled::Skipped)));
    let stream = rig.cache.get_stream("cas/b/obj", None).await.unwrap();
    let (chunks, error) = drain(stream).await;
    assert_eq!(error, None);
    assert_eq!(joined(&chunks), body);
    assert_eq!(read(&rig.cache.counters().fills), 0);
    assert_eq!(rig.cache.held(), (0, 0));
}
