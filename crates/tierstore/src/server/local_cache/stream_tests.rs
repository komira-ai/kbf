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

use super::layout::name_of;
use super::locked;
use super::metrics::{FailedFill, read};
use super::testing::{Quirk, Rig, digest, pattern};
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
    while read(&rig.cache.counters().misses) < count {
        tokio::task::yield_now().await;
    }
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
/// the call); each chunk taken makes one more; and the end is known by a read that gives
/// nothing. An object is never read whole, whatever its size.
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
    assert_eq!(rig.calls(Op::Get), 11);
    assert_eq!(rig.cache.held(), (0, 0));

    let absent = rig.cache.get_stream("plain/none", None).await;
    assert!(matches!(absent, Err(Error::NotFound(_))));
}

/// Asserts that a miss is given to the reader while the fill is still running: with the upstream
/// opened for one chunk only, the first chunk of the object reaches the reader, while the fill
/// has written 100 bytes, nothing is in the cache and one file is staged. Then, a second reader
/// that joins the running fill is given the object from its first byte, from the staged file. When the upstream is opened both have the object,
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
    while read(&rig.cache.counters().fills) < 1 {
        tokio::task::yield_now().await;
    }
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
