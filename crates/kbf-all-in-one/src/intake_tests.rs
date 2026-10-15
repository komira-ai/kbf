//! The intake's batches: when they are sent, how many fly at once, and what is stored.
//! The walk's side (one read per file) is `tests/one_read.rs`.

use std::sync::mpsc;
use std::time::Duration;

use kbf_front::{Cache, SegmentsConfig};
use kbf_server::LocalStore;
use kbf_transfer::{RecordHasher, digest_of, proto_digest};

use super::*;

/// A runtime standing in for the server half, with one worker.
fn server() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime")
}

/// The plane over a new in-memory cache, its calls run on `server`.
fn plane(server: &tokio::runtime::Runtime) -> LocalDataPlane {
    let _context = server.enter();
    let store = LocalStore::new(Arc::new(Cache::memory()), SegmentsConfig::DEFAULT);
    LocalDataPlane::new(store, server.handle().clone())
}

/// Hands `bytes` to `taking` as one file, in two chunks, and finishes it.
fn take(taking: &Taking, bytes: &[u8]) -> kbf_types::Digest {
    let mut file = taking.file(bytes.len() as u64).expect("taken");
    let (first, rest) = bytes.split_at(bytes.len() / 2);
    file.write(first);
    file.write(rest);
    let mut hasher = RecordHasher::new();
    hasher.update(bytes);
    let (digest, crc32c) = hasher.finish();
    file.finish(digest, crc32c);
    digest
}

fn sent(taking: &Taking) -> usize {
    lock(&taking.0.state).uploads.len()
}

/// Catches a batch held until the walk ends, or sent before it is full, and files lost
/// at the end: with a 10-byte batch, a 4-byte file waits, a 6-byte one fills the batch
/// and both are sent at once, a 3-byte one waits for `finish`, which sends it and
/// answers once all three are held (mutants: the size never compared, `finish` not
/// sending the rest). Each file's bytes are the chunks handed over, in order.
#[test]
fn a_full_batch_is_sent_at_once_and_the_rest_at_the_finish() {
    let server = server();
    let plane = plane(&server);
    let taking = Taking::with(plane.clone(), 10, 4);
    let a = take(&taking, b"aaaa");
    assert_eq!(sent(&taking), 0);
    let b = take(&taking, b"bbbbbb");
    assert_eq!(sent(&taking), 1);
    let c = take(&taking, b"ccc");
    assert_eq!(sent(&taking), 1);
    server.block_on(taking.finish()).expect("stored");
    assert_eq!(sent(&taking), 0, "every upload awaited");
    server.block_on(async {
        for (digest, bytes) in [(a, &b"aaaa"[..]), (b, b"bbbbbb"), (c, b"ccc")] {
            assert_eq!(plane.get(&proto_digest(&digest)).await, Ok(bytes.to_vec()));
        }
    });
}

/// Catches a batch of many small files held until it reaches its size: 16,384 files
/// (the server's most records in one upload) are sent as one batch however few bytes
/// they hold (mutant: the record count not compared).
#[test]
fn a_batch_of_the_most_records_is_sent_however_small() {
    let server = server();
    let taking = Taking::with(plane(&server), u64::MAX, 4);
    for n in 0..MAX_UPLOAD_RECORDS - 1 {
        take(&taking, &(n as u64).to_le_bytes());
    }
    assert_eq!(sent(&taking), 0);
    let last = take(&taking, &u64::MAX.to_le_bytes());
    assert_eq!(sent(&taking), 1);
    server.block_on(taking.finish()).expect("stored");
    let plane = plane_of(&taking);
    server.block_on(async {
        assert!(plane.missing(&[proto_digest(&last)]).await.is_empty());
    });
}

fn plane_of(taking: &Taking) -> LocalDataPlane {
    taking.0.plane.clone()
}

/// Catches more batches in flight than the bound, and a walk that waits for a batch
/// that ended without being told: with one batch in flight at most, while the server
/// half's one worker is held the second file's finish does not return, and once the
/// worker is free it does (mutants: no wait, an ended upload not counted down or not
/// signalled, which hangs).
#[test]
fn a_walk_past_the_batches_in_flight_waits_for_one_to_end() {
    let server = server();
    let taking = Arc::new(Taking::with(plane(&server), 1, 1));
    let (hold, held) = mpsc::channel::<()>();
    let (started, running) = mpsc::channel::<()>();
    server.spawn(async move {
        started.send(()).expect("started");
        // Holds the server half's only worker: no upload runs meanwhile.
        let _ = held.recv();
    });
    running.recv().expect("the worker is held");
    take(&taking, b"first");
    assert_eq!(lock(&taking.0.state).in_flight, 1);
    let (done, finished) = mpsc::channel();
    let second = {
        let taking = Arc::clone(&taking);
        std::thread::spawn(move || {
            take(&taking, b"second");
            done.send(()).expect("done");
        })
    };
    assert!(
        finished.recv_timeout(Duration::from_millis(300)).is_err(),
        "the second file was finished with a batch in flight"
    );
    hold.send(()).expect("release the worker");
    finished
        .recv_timeout(Duration::from_secs(30))
        .expect("the second file is finished once the first batch ends");
    second.join().expect("joined");
    server.block_on(taking.finish()).expect("stored");
    assert_eq!(lock(&taking.0.state).in_flight, 0);
}

/// Catches one digest stored twice, and a file left unfinished stored anyway: two files
/// with the same bytes make one batch of one record, and a file handed over but never
/// finished (the walk found it changed) is in no batch (mutants: no `seen` check, an
/// unfinished file added on drop).
#[test]
fn one_digest_is_stored_once_and_an_unfinished_file_not_at_all() {
    let server = server();
    let taking = Taking::with(plane(&server), 1, 4);
    take(&taking, b"same");
    take(&taking, b"same");
    assert_eq!(sent(&taking), 1);
    let mut left = taking.file(4).expect("taken");
    left.write(b"left");
    drop(left);
    assert_eq!(sent(&taking), 1);
    server.block_on(taking.finish()).expect("stored");
    let plane = plane_of(&taking);
    server.block_on(async {
        let left = digest_of(b"left");
        assert_eq!(plane.missing(std::slice::from_ref(&left)).await, [left]);
    });
}

/// Catches a file over the server's largest blob taken (held whole in memory, then
/// refused by the server) or one of exactly that size declined: the bound is the
/// largest blob the server accepts.
#[test]
fn a_file_over_the_largest_blob_is_not_taken() {
    assert_eq!(LARGEST_TAKEN, kbf_front::MAX_BLOB_BYTES);
    let server = server();
    let taking = Taking::new(plane(&server));
    assert!(taking.file(LARGEST_TAKEN + 1).is_none());
    // Reserved, never written: no memory is touched.
    assert!(taking.file(LARGEST_TAKEN).is_some());
}

/// Catches a batch whose server half is gone taken for stored, or hanging: the upload
/// never runs, the in-flight count comes back down, and `finish` fails UNAVAILABLE.
#[test]
fn a_server_half_that_is_gone_fails_the_finish() {
    let server = server();
    let taking = Taking::with(plane(&server), 1, 1);
    drop(server);
    take(&taking, b"lost");
    assert_eq!(lock(&taking.0.state).in_flight, 0);
    let here = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let why = here.block_on(taking.finish()).expect_err("gone");
    assert!(
        matches!(&why, CasError::Unavailable(blob, message)
            if blob == "outputs" && message.contains("did not answer")),
        "{why}"
    );
}
