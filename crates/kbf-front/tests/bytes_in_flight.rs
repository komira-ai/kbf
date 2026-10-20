//! The bytes the process holds for requests in flight (`kbf_bytes_in_flight` and
//! `kbf_bytes_in_flight_peak`), read after real ByteStream and CAS calls over gRPC: the
//! exact oracle for memory bounds. A small blob is held at its size, a large one a few
//! messages at a time, a read one window at a time, and every byte is given back however
//! the call ended.
//!
//! The gauge is the process's, so the tests take turns and each starts its peak over.

mod common;

use std::time::Duration;

use common::{Blob, Farm, pseudo_random};
use futures::channel::mpsc;
use kbf_front::{MAX_UNSTAGED_BLOB_BYTES, READ_WINDOW_BYTES};
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use kbf_proto::reapi::{BatchReadBlobsRequest, Digest as Wire};
use kbf_storage::in_flight::process;
use tokio::sync::{Mutex, MutexGuard};

const MIB: usize = 1 << 20;

static TURN: Mutex<()> = Mutex::const_new(());

/// Waits for this test's turn at the process gauge, which must be empty; starts the peak
/// over.
async fn turn() -> MutexGuard<'static, ()> {
    let turn = TURN.lock().await;
    given_back("before the test").await;
    process().reset_peak();
    turn
}

/// Waits for the count to reach 0: a handler's answer can reach the caller a moment
/// before the task that held the last clone of a piece has dropped it.
async fn given_back(when: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while process().current() != 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{when}: {} bytes still counted",
            process().current()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn write_name(blob: &Blob) -> String {
    format!(
        "main/uploads/7d5e6f1a-test/blobs/{}/{}",
        blob.digest.hash_hex(),
        blob.digest.size_bytes
    )
}

/// The write requests that send `data` for `name` in `chunk`-byte pieces from offset 0.
fn chunks(name: &str, data: &[u8], chunk: usize) -> Vec<WriteRequest> {
    let pieces: Vec<&[u8]> = data.chunks(chunk).collect();
    let last = pieces.len() - 1;
    pieces
        .iter()
        .enumerate()
        .map(|(i, piece)| WriteRequest {
            resource_name: if i == 0 {
                name.to_owned()
            } else {
                String::new()
            },
            write_offset: (i * chunk) as i64,
            finish_write: i == last,
            data: piece.to_vec(),
        })
        .collect()
}

async fn write(farm: &Farm, blob: &Blob, chunk: usize) -> Result<(), tonic::Status> {
    let requests = chunks(&write_name(blob), &blob.data, chunk);
    farm.bytestream()
        .write(futures::stream::iter(requests))
        .await
        .map(drop)
}

/// Catches: a blob that fits in memory (at most `MAX_UNSTAGED_BLOB_BYTES`) not counted
/// from the allocation of its receive buffer, counted twice, or not given back once the
/// cache has stored it. Held once: between its size and its size plus the messages in
/// flight.
///
/// Mutants: the capacity of the receive buffer not counted (peak 0); the guard leaked
/// (current above 0 after).
#[tokio::test]
async fn a_small_blob_is_held_at_its_size_and_given_back() {
    let _turn = turn().await;
    let farm = Farm::start().await;
    let blob = Blob::new(pseudo_random(4 * MIB, 1));

    write(&farm, &blob, MIB).await.expect("Write");

    let peak = process().peak();
    assert!(peak >= blob.data.len() as u64, "held whole: {peak}");
    assert!(peak <= 2 * blob.data.len() as u64, "held once: {peak}");
    given_back("after the write").await;
}

/// Catches: a blob over `MAX_UNSTAGED_BLOB_BYTES` held whole (the pieces collected
/// before they are written: peak near 48 MiB), a piece not counted (peak 0) and a leak.
/// Streamed, it is held a piece at a time: the 6 MiB bound is three 2 MiB pieces.
///
/// Mutant: whole-buffer, `write_streamed` collecting `body` before the store.
#[tokio::test]
async fn a_large_blob_is_streamed_not_held_whole() {
    let _turn = turn().await;
    let farm = Farm::start().await;
    let blob = Blob::new(pseudo_random(3 * MAX_UNSTAGED_BLOB_BYTES as usize, 2));

    write(&farm, &blob, 2 * MIB).await.expect("Write");
    assert!(farm.find_missing(&[&blob]).await.is_empty(), "stored");

    let peak = process().peak();
    assert!(peak >= 2 * MIB as u64, "a piece in hand is counted: {peak}");
    assert!(peak <= 6 * MIB as u64, "a few pieces, not the blob: {peak}");
    given_back("after the write").await;
}

/// Catches: a read that holds more than a window at a time (a blob gathered before it is
/// sent), a window not counted, and one still counted after the stream is drained.
///
/// Mutants: a window kept (a clone stashed) until the next is read, so two are held at
/// once (peak 16 MiB, over the 12 MiB bound). Removing the `drop(take(window))` in
/// `bytestream.rs` alone is not a mutant today: `Bytes::split_to` of a whole window
/// already releases its buffer (bytes 1.12), so the drop is belt and braces.
#[tokio::test]
async fn a_read_holds_one_window_and_gives_it_back() {
    let _turn = turn().await;
    let farm = Farm::start().await;
    let blob = Blob::new(pseudo_random(2 * MAX_UNSTAGED_BLOB_BYTES as usize + 5, 3));
    write(&farm, &blob, 2 * MIB).await.expect("Write");
    given_back("after the write").await;
    process().reset_peak();

    let mut stream = farm
        .bytestream()
        .read(ReadRequest {
            resource_name: format!(
                "main/blobs/{}/{}",
                blob.digest.hash_hex(),
                blob.digest.size_bytes
            ),
            ..Default::default()
        })
        .await
        .expect("Read")
        .into_inner();
    let mut got = 0;
    while let Some(message) = stream.message().await.expect("a message") {
        got += message.data.len();
    }
    assert_eq!(got, blob.data.len());
    drop(stream);

    let window = READ_WINDOW_BYTES;
    let peak = process().peak();
    assert!(peak >= window, "a window is counted: {peak}");
    // One window and the odd bytes of the message in hand, never two: the spent window
    // is given back before the next is read, so a half window of slack tells one window
    // from two.
    assert!(
        peak < window + window / 2,
        "about one window, not two or the blob: {peak}"
    );
    given_back("after the read").await;
}

/// Catches: a whole-blob read (`Cache::read_blob`, what a batch read and a decode make) of
/// a blob of several windows whose joined buffer is not counted (it is the whole blob, held
/// by the caller), or counted at the wrong size: the windows are given back as they are
/// joined, so what is counted once the read returns is the blob, exactly, and the most
/// counted at once reached the blob's size.
///
/// Mutant: `gather` holding 0 bytes for the joined buffer (`hold(self.range_len())` made
/// `hold(0)`: the count after the read is 0).
#[tokio::test]
async fn a_whole_read_of_several_windows_counts_the_joined_blob() {
    let _turn = turn().await;
    let farm = Farm::start().await;
    let size = 2 * READ_WINDOW_BYTES as usize + 5;
    let blob = Blob::new(pseudo_random(size, 6));
    write(&farm, &blob, 2 * MIB).await.expect("Write");
    given_back("after the write").await;
    process().reset_peak();

    let whole = farm.cache.read_blob(&blob.digest).await.expect("read_blob");
    assert_eq!(whole.len(), size);
    assert_eq!(
        process().current(),
        size as u64,
        "the joined blob, and no window"
    );
    let peak = process().peak();
    assert!(peak >= size as u64, "the blob is held whole: {peak}");
    drop(whole);
    given_back("after the blob is dropped").await;
}

/// Catches: bytes left counted by a call that failed or was abandoned: a digest that is
/// not the bytes' (small and large), a sender that goes away after the first message (a
/// receive buffer allocated for a blob that never comes), and a client that drops a large
/// write half way.
#[tokio::test]
async fn a_failed_or_abandoned_write_gives_everything_back() {
    let _turn = turn().await;
    let farm = Farm::start().await;
    for size in [4 * MIB, 2 * MAX_UNSTAGED_BLOB_BYTES as usize] {
        let blob = Blob::new(pseudo_random(size, 4));

        let mut wrong = blob.data.clone();
        wrong[size / 2] ^= 1;
        let requests = chunks(&write_name(&blob), &wrong, MIB);
        let refused = farm
            .bytestream()
            .write(futures::stream::iter(requests))
            .await
            .expect_err("bytes that are not the digest's");
        assert_eq!(refused.code(), tonic::Code::InvalidArgument, "{refused:?}");
        given_back(&format!("wrong bytes, {size}")).await;

        // The first message, then nothing more: the stream is dropped open.
        let (tx, rx) = mpsc::unbounded();
        let first = chunks(&write_name(&blob), &blob.data, MIB).remove(0);
        tx.unbounded_send(WriteRequest {
            finish_write: false,
            ..first
        })
        .expect("send");
        let mut client = farm.bytestream();
        let task = tokio::spawn(async move { client.write(rx).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        task.abort();
        let _ = task.await;
        drop(tx);
        given_back(&format!("abandoned, {size}")).await;
    }
}

/// Catches: a BatchUpdateBlobs whose blobs are not counted (the request's data is in
/// memory until its segment is stored), or are never given back.
///
/// Mutant: `verify_upload` not counting (peak 0).
#[tokio::test]
async fn a_batch_update_is_counted_and_given_back() {
    let _turn = turn().await;
    let farm = Farm::start().await;
    let blobs: Vec<Blob> = (0..3)
        .map(|i| Blob::new(pseudo_random(MIB, 10 + i)))
        .collect();
    farm.upload(&blobs.iter().collect::<Vec<_>>()).await;

    let peak = process().peak();
    assert!(peak >= 3 * MIB as u64, "the batch is held: {peak}");
    assert!(peak <= 6 * MIB as u64, "held once: {peak}");
    given_back("after the batch").await;
}

/// Catches: a BatchReadBlobs of blobs that lie back to back in one segment not counted: the
/// one range GET that serves the run (`Cache::read_joined`, every blob a slice of it) and
/// the copies the response carries (`CasService::batch_read_blobs`). At the point the
/// handler has built its answer both are alive, so the peak is the batch twice over; the
/// run alone, or the copies alone, is the batch once. All of it is given back.
///
/// Mutants: `read_joined` returning the GET's bytes uncounted (peak one batch); the
/// response copies not held (peak one batch).
#[tokio::test]
async fn a_batch_read_counts_the_joined_get_and_the_response_copies() {
    let _turn = turn().await;
    let farm = Farm::start().await;
    // Odd seeds: the generator makes `seed | 1`, so 100 and 101 would be one blob.
    let blobs: Vec<Blob> = (0..12)
        .map(|i| Blob::new(pseudo_random(MIB, 101 + 2 * i)))
        .collect();
    farm.upload(&blobs.iter().collect::<Vec<_>>()).await;
    given_back("after the upload").await;
    process().reset_peak();
    let digests = blobs
        .iter()
        .map(|b| Wire {
            hash: b.digest.hash_hex(),
            size_bytes: b.digest.size_bytes as i64,
        })
        .collect();

    let answer = farm
        .cas()
        .batch_read_blobs(BatchReadBlobsRequest {
            instance_name: "main".into(),
            digests,
            ..Default::default()
        })
        .await
        .expect("BatchReadBlobs")
        .into_inner();

    let batch = (12 * MIB) as u64;
    let got: usize = answer.responses.iter().map(|r| r.data.len()).sum();
    assert_eq!(got as u64, batch, "every blob answered");
    let peak = process().peak();
    assert!(peak >= 2 * batch, "the GET and the copies are held: {peak}");
    assert!(
        peak <= 2 * batch + READ_WINDOW_BYTES,
        "held once each: {peak}"
    );
    given_back("after the batch read").await;
}

/// Catches: the gauge missing from `/metrics` (a renamed or dropped family), or exporting
/// something other than the process's current and peak.
#[tokio::test]
async fn the_metrics_export_the_current_and_the_peak() {
    let _turn = turn().await;
    let farm = Farm::start().await;
    let blob = Blob::new(pseudo_random(2 * MIB, 5));
    write(&farm, &blob, MIB).await.expect("Write");
    given_back("after the write").await;

    let text = farm.cache.render_metrics();
    let peak = process().peak();
    assert!(peak >= 2 * MIB as u64, "{peak}");
    assert!(
        text.contains("\nkbf_bytes_in_flight 0\n"),
        "{}",
        grep(&text)
    );
    assert!(
        text.contains(&format!("\nkbf_bytes_in_flight_peak {peak}\n")),
        "{}",
        grep(&text)
    );
}

fn grep(text: &str) -> String {
    text.lines()
        .filter(|l| l.contains("in_flight"))
        .collect::<Vec<_>>()
        .join("\n")
}
