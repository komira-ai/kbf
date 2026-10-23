//! The bytes a `Write` holds, read from the process gauge (`kbf_bytes_in_flight`): a blob
//! of any size is held a few messages at a time, a small buffered one at its size, and
//! every byte is given back however the write ended.

use std::sync::atomic::Ordering;
use tokio::sync::{Mutex, MutexGuard};

use kbf_proto::google::bytestream::WriteRequest;
use kbf_types::Digest;

use super::fake::{Mem, patterned, serve, sha};
use super::{raw_write, upload_name};
use crate::Storage as _;
use crate::in_flight::process;
use crate::service::{RemoteStorage, STREAM_FROM_BYTES};

const MIB: usize = 1 << 20;

/// The tests share the process gauge: one at a time, from a peak of its own.
static TURN: Mutex<()> = Mutex::const_new(());

/// This test's turn at the gauge, which must be empty, with the peak started over; `None`
/// (the test passes without running) outside cargo-nextest.
///
/// The gauge is the process's, and the crate's other service tests write through it too.
/// Under nextest every test has a process of its own, so nothing else is counted; under
/// `cargo test` the tests of the crate share a process and an exact count cannot be read.
/// CI runs nextest (`.config/nextest.toml`).
async fn turn() -> Option<MutexGuard<'static, ()>> {
    if std::env::var_os("NEXTEST").is_none() {
        eprintln!(
            "skipped: the process gauge is exact only in a process of its own (cargo nextest)"
        );
        return None;
    }
    let turn = TURN.lock().await;
    assert_eq!(process().current(), 0, "an earlier test left bytes held");
    process().reset_peak();
    Some(turn)
}

/// Waits for the count to reach 0: a handler's answer can reach the caller a moment before
/// a blocking-pool task drops its last clone of a message.
async fn given_back(when: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while process().current() != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "{when}: {} bytes still counted",
            process().current()
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

/// Empties the fake store: it keeps the `Bytes` it is given (a real store copies them), and
/// bytes it keeps are still held.
fn forget(mem: &Mem) {
    mem.blobs.lock().expect("lock").clear();
}

/// `data` for `digest` in messages of `piece` bytes, the last with `finish_write`;
/// `stop_after` messages only, if given (a sender that goes away).
fn messages(
    digest: &Digest,
    data: &[u8],
    piece: usize,
    stop_after: Option<usize>,
) -> Vec<WriteRequest> {
    let count = data.len().div_ceil(piece);
    data.chunks(piece)
        .enumerate()
        .take(stop_after.unwrap_or(count))
        .map(|(i, part)| WriteRequest {
            resource_name: if i == 0 {
                upload_name(digest)
            } else {
                String::new()
            },
            write_offset: (i * piece) as i64,
            finish_write: i + 1 == count,
            data: part.to_vec(),
        })
        .collect()
}

/// A streamed `Write` (the store takes bytes as they arrive) holds a few messages, not
/// the blob (peak between one message and 4 MiB for a 20 MiB blob), counts the message in
/// hand, and gives every byte back (current 0 after).
#[tokio::test]
async fn a_streamed_write_holds_a_few_messages_and_gives_them_back() {
    let Some(_turn) = turn().await else {
        return;
    };
    let mem = std::sync::Arc::new(Mem::default());
    mem.knobs.enabled.store(true, Ordering::SeqCst);
    let channel = serve(&mem, 1 << 30).await;
    let blob = patterned(20 * MIB);
    let digest = sha(&blob);

    let answered = raw_write(&channel, messages(&digest, &blob, MIB, None))
        .await
        .expect("a streamed write");
    assert_eq!(answered.committed_size, blob.len() as i64);
    assert_eq!(mem.knobs.begun.load(Ordering::SeqCst), 1, "streamed");

    let peak = process().peak();
    assert!(peak >= MIB as u64, "a message in hand is counted: {peak}");
    assert!(
        peak <= 4 * MIB as u64,
        "a few messages, not the blob: {peak}"
    );
    given_back("after the write").await;
}

/// A buffered write (the store cannot stream, the blob is at most `STREAM_FROM_BYTES`)
/// counts its receive buffer at the size it is allocated at, keeps it counted while the
/// store holds the blob, and gives it back afterwards.
#[tokio::test]
async fn a_buffered_write_holds_its_blob_and_gives_it_back() {
    let Some(_turn) = turn().await else {
        return;
    };
    let mem = std::sync::Arc::new(Mem::default());
    let channel = serve(&mem, 1 << 30).await;
    let blob = patterned(4 * MIB);
    let digest = sha(&blob);

    raw_write(&channel, messages(&digest, &blob, MIB, None))
        .await
        .expect("a buffered write");
    assert_eq!(mem.knobs.begun.load(Ordering::SeqCst), 0, "not streamed");
    // The fake store keeps the buffer it was given, and so it stays counted.
    assert_eq!(process().current(), blob.len() as u64, "kept by the store");
    forget(&mem);

    let peak = process().peak();
    assert!(peak >= blob.len() as u64, "the blob is held whole: {peak}");
    assert!(
        peak <= 2 * blob.len() as u64,
        "held once, not twice: {peak}"
    );
    given_back("after the write").await;
}

/// A blob over `STREAM_FROM_BYTES` on a store that cannot stream is passed on a message at
/// a time through `put_streamed`, not held whole, and its messages are counted.
#[tokio::test]
async fn a_large_buffered_write_is_passed_on_a_message_at_a_time() {
    let Some(_turn) = turn().await else {
        return;
    };
    let mem = std::sync::Arc::new(Mem::default());
    let channel = serve(&mem, 1 << 30).await;
    let blob = patterned(STREAM_FROM_BYTES as usize + 4 * MIB);
    let digest = sha(&blob);

    raw_write(&channel, messages(&digest, &blob, MIB, None))
        .await
        .expect("a large write");
    assert_eq!(
        mem.streamed.load(Ordering::SeqCst),
        1,
        "put_streamed took it"
    );

    let peak = process().peak();
    assert!(peak >= MIB as u64, "a message in hand is counted: {peak}");
    assert!(
        peak <= 4 * MIB as u64,
        "a few messages, not the blob: {peak}"
    );
    given_back("after the write").await;
}

/// A write that failed or whose sender went away leaves no bytes counted, on each of the
/// paths: a digest that is not the bytes', a sender that stops after some messages and a
/// buffer allocated for a blob that never comes.
#[tokio::test]
async fn a_write_that_fails_or_is_abandoned_gives_everything_back() {
    let Some(_turn) = turn().await else {
        return;
    };
    for streams in [true, false] {
        let mem = std::sync::Arc::new(Mem::default());
        mem.knobs.enabled.store(streams, Ordering::SeqCst);
        let channel = serve(&mem, 1 << 30).await;
        for size in [4 * MIB, STREAM_FROM_BYTES as usize + 2 * MIB] {
            let blob = patterned(size);
            let digest = sha(&blob);

            let mut wrong = blob.clone();
            wrong[size / 2] ^= 1;
            let refused = raw_write(&channel, messages(&digest, &wrong, MIB, None)).await;
            assert!(
                refused.is_err(),
                "streams={streams} size={size}: {refused:?}"
            );
            forget(&mem);
            given_back(&format!("wrong bytes, streams={streams} size={size}")).await;

            let stopped = raw_write(&channel, messages(&digest, &blob, MIB, Some(2))).await;
            assert!(
                stopped.is_err(),
                "streams={streams} size={size}: {stopped:?}"
            );
            given_back(&format!("sender gone, streams={streams} size={size}")).await;
        }
    }
    assert!(process().peak() >= MIB as u64);
}

/// A whole-blob read through a storage client (`RemoteStorage::read_blob`, what the proxy
/// frontend reads a blob with) counts its buffer once, at the blob's size, and builds it
/// once: the peak is one buffer and a chunk or two (the blob and a few MiB), not two
/// whole-blob copies. The count is exact while the caller holds the blob, 0 once it is
/// dropped.
#[tokio::test]
async fn a_remote_whole_blob_read_is_held_once_and_given_back() {
    let Some(_turn) = turn().await else {
        return;
    };
    let mem = std::sync::Arc::new(Mem::default());
    let channel = serve(&mem, 1 << 30).await;
    let blob = patterned(20 * MIB);
    let digest = sha(&blob);
    mem.blobs
        .lock()
        .expect("lock")
        .insert(digest, bytes::Bytes::from(blob.clone()));

    let remote = RemoteStorage::new(channel, 0);
    let got = remote.read_blob(&digest).await.expect("read_blob");
    assert_eq!(got.len(), blob.len());
    assert!(got[..] == blob[..], "the bytes");
    assert_eq!(
        process().current(),
        blob.len() as u64,
        "the blob, counted once, while the caller holds it"
    );
    let peak = process().peak();
    assert!(peak >= blob.len() as u64, "the blob is held whole: {peak}");
    assert!(
        peak <= blob.len() as u64 + 4 * MIB as u64,
        "one buffer and a chunk or two, not two copies: {peak}"
    );
    drop(got);
    forget(&mem);
    given_back("after the blob is dropped").await;
}
