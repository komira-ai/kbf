//! Blobs over the packs' bound, written by the frontend as `cas/l/<sha256>` objects and recorded
//! in the pack index, and read back from them, with no storage node.

use futures::{StreamExt as _, stream};
use kbf_front::{READ_WINDOW_BYTES, large_key};
use kbf_pack_index::{NewPack, PackBlob, PackIndex as _};
use kbf_proto::google::bytestream::WriteRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use tonic::Code;

use crate::bytestream::{pieces, read, read_name, upload_name, write};
use crate::cas::missing;
use crate::rig::{Rig, blob_len, index_form};

/// Asserts that a blob over the packs' bound is stored as the object `cas/l/<sha256>` in one
/// streamed put, recorded in the index as a pack of that one blob at offset 0, held, and read
/// back whole and by range (to the end, past the end, empty) in the bytes written.
#[tokio::test]
async fn a_large_blob_is_an_object_recorded_in_the_index_and_read_back() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(5000, 1);

    let size = write(&rig, pieces(&digest, &bytes, 700)).await;

    assert_eq!(size.expect("written"), 5000);
    let key = large_key(&digest);
    assert_eq!(key, format!("cas/l/{}", digest.hash_hex()));
    assert_eq!(rig.store.held(&key).await, Some(bytes.clone()));
    assert_eq!(rig.store.stream_puts(), 1);
    let location = rig.index.where_is(&[digest]).remove(0).expect("recorded");
    assert_eq!(
        (location.pack_key.as_str(), location.offset, location.length),
        (key.as_str(), 0, 5000)
    );
    assert!(missing(&rig, &[digest]).await.expect("asked").is_empty());
    let name = read_name(&digest);
    assert_eq!(read(&rig, &name, 0, 0).await.expect("read"), bytes.to_vec());
    assert_eq!(
        read(&rig, &name, 4000, 0).await.expect("read"),
        bytes[4000..]
    );
    assert_eq!(
        read(&rig, &name, 100, 50).await.expect("read"),
        bytes[100..150]
    );
    assert_eq!(
        read(&rig, &name, 4990, 100).await.expect("read"),
        bytes[4990..]
    );
    assert_eq!(
        read(&rig, &name, 5000, 0).await.expect("read"),
        Vec::<u8>::new()
    );
}

/// Asserts that a blob of more than one read window is read in ranged requests of at most the
/// window, never whole, and comes back as it was written, whole and by a range across the
/// boundary of two windows.
#[tokio::test]
async fn a_blob_of_several_windows_is_read_in_windows() {
    let rig = Rig::new().await;
    let window = READ_WINDOW_BYTES as usize;
    let (digest, bytes) = blob_len(2 * window + 12345, 2);
    write(&rig, pieces(&digest, &bytes, 1 << 20))
        .await
        .expect("written");
    let key = large_key(&digest);

    let whole = read(&rig, &read_name(&digest), 0, 0).await.expect("read");
    let across = read(&rig, &read_name(&digest), window as i64 - 10, 100)
        .await
        .expect("read");

    assert!(whole == bytes.to_vec(), "the whole blob is read back");
    assert_eq!(across, bytes[window - 10..window + 90]);
    let reads: Vec<(u64, u64)> = rig
        .store
        .ranges()
        .into_iter()
        .filter(|(k, _, _)| *k == key)
        .map(|(_, offset, len)| (offset, len))
        .collect();
    let w = window as u64;
    assert_eq!(
        reads,
        vec![(0, w), (w, w), (2 * w, 12345), (w - 10, 100)],
        "each read is a window of the object"
    );
}

/// Asserts that a large blob whose bytes are not its digest, or are fewer or more than it names,
/// is INVALID_ARGUMENT, leaves no object under its key and no row in the index.
#[tokio::test]
async fn a_large_blob_that_is_not_its_digest_stores_nothing() {
    let rig = Rig::new().await;
    let (digest, _) = blob_len(5000, 3);
    let (_, other) = blob_len(5000, 4);
    let (_, short) = blob_len(4000, 3);
    let (_, long) = blob_len(6000, 3);

    for bytes in [other, short, long] {
        let sent = write(&rig, pieces(&digest, &bytes, 700)).await;
        assert_eq!(
            sent.expect_err("not the blob").code(),
            Code::InvalidArgument
        );
    }

    assert_eq!(rig.store.held(&large_key(&digest)).await, None);
    assert_eq!(missing(&rig, &[digest]).await.expect("asked"), vec![digest]);
}

/// Asserts that a large upload that breaks the rules of an upload (a message out of order, a
/// stream that ends without `finish_write`) is INVALID_ARGUMENT with the client's own message
/// and stores nothing.
#[tokio::test]
async fn a_large_upload_that_breaks_the_rules_stores_nothing() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(5000, 5);
    let name = upload_name(&digest);
    let message = |name: &str, offset: i64, finish: bool, data: &[u8]| WriteRequest {
        resource_name: name.to_owned(),
        write_offset: offset,
        finish_write: finish,
        data: data.to_vec(),
    };
    let broken = vec![
        vec![
            message(&name, 0, false, &bytes[..2000]),
            message("", 2500, true, &bytes[2000..]),
        ],
        vec![message(&name, 0, false, &bytes[..2000])],
    ];

    for messages in broken {
        let sent = write(&rig, messages).await;
        assert_eq!(sent.expect_err("refused").code(), Code::InvalidArgument);
    }

    assert_eq!(rig.store.held(&large_key(&digest)).await, None);
    assert_eq!(missing(&rig, &[digest]).await.expect("asked"), vec![digest]);
}

/// Asserts that a large upload with messages that carry no data between its pieces (a client
/// that flushes an empty message) is stored whole: the empty messages add nothing to the blob and
/// do not count against it.
#[tokio::test]
async fn a_large_upload_with_empty_messages_between_its_pieces_is_stored_whole() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(5000, 6);
    let name = upload_name(&digest);
    let message = |name: &str, offset: i64, finish: bool, data: &[u8]| WriteRequest {
        resource_name: name.to_owned(),
        write_offset: offset,
        finish_write: finish,
        data: data.to_vec(),
    };
    let messages = vec![
        message(&name, 0, false, &[]),
        message("", 0, false, &bytes[..2000]),
        message("", 2000, false, &[]),
        message("", 2000, true, &bytes[2000..]),
    ];

    let size = write(&rig, messages).await;

    assert_eq!(size.expect("written"), 5000);
    assert_eq!(rig.store.held(&large_key(&digest)).await, Some(bytes));
}

/// Asserts that a large upload that goes silent is ended DEADLINE_EXCEEDED, and stores nothing.
#[tokio::test(start_paused = true)]
async fn a_large_upload_that_goes_silent_is_ended_deadline_exceeded() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(5000, 6);
    let first = WriteRequest {
        resource_name: upload_name(&digest),
        write_offset: 0,
        finish_write: false,
        data: bytes[..1000].to_vec(),
    };
    let silent = stream::once(async move { first }).chain(stream::pending());

    let ended = ByteStreamClient::new(rig.front.clone()).write(silent).await;

    assert_eq!(ended.expect_err("ended").code(), Code::DeadlineExceeded);
    assert_eq!(rig.store.held(&large_key(&digest)).await, None);
}

/// Asserts that when the object store refuses the put the write is UNAVAILABLE and records
/// nothing, and the same write sent again is stored.
#[tokio::test]
async fn a_large_put_the_store_refuses_is_unavailable_and_can_be_sent_again() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(5000, 7);
    rig.store.reject_puts(1);

    let refused = write(&rig, pieces(&digest, &bytes, 700)).await;
    let missing_after = missing(&rig, &[digest]).await.expect("asked");
    let again = write(&rig, pieces(&digest, &bytes, 700)).await;

    assert_eq!(refused.expect_err("refused").code(), Code::Unavailable);
    assert_eq!(missing_after, vec![digest]);
    assert_eq!(again.expect("written"), 5000);
    assert!(missing(&rig, &[digest]).await.expect("asked").is_empty());
}

/// Asserts that when the object was stored and the index could not record it, the write is
/// UNAVAILABLE, and the write sent again records the object that is there without sending the
/// blob again.
#[tokio::test]
async fn an_object_the_index_could_not_record_is_recorded_by_the_next_write() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(5000, 8);
    rig.index.fail_record(1);

    let refused = write(&rig, pieces(&digest, &bytes, 700)).await;
    let again = write(&rig, pieces(&digest, &bytes, 700)).await;

    assert_eq!(refused.expect_err("not recorded").code(), Code::Unavailable);
    assert_eq!(again.expect("written"), 5000);
    assert_eq!(rig.store.stream_puts(), 1, "the object was put once");
    assert!(missing(&rig, &[digest]).await.expect("asked").is_empty());
}

/// Asserts that an object of the right length already under the key is recorded and not put
/// again (the key is content-bound), and an object of another length is neither replaced nor
/// recorded: the write is UNAVAILABLE and the object is as it was.
#[tokio::test]
async fn an_object_already_under_the_key_is_trusted_by_length_only() {
    let rig = Rig::new().await;
    let (present, present_bytes) = blob_len(5000, 9);
    let (wrong, wrong_bytes) = blob_len(5000, 10);
    rig.store.seed(&large_key(&present), &present_bytes).await;
    rig.store.seed(&large_key(&wrong), b"too short").await;

    let recorded = write(&rig, pieces(&present, &present_bytes, 700)).await;
    let refused = write(&rig, pieces(&wrong, &wrong_bytes, 700)).await;

    assert_eq!(recorded.expect("recorded"), 5000);
    assert_eq!(rig.store.stream_puts(), 0, "nothing was put");
    assert!(missing(&rig, &[present]).await.expect("asked").is_empty());
    let refused = refused.expect_err("another length");
    assert_eq!(refused.code(), Code::Unavailable);
    assert!(refused.message().contains("not replaced"), "{refused}");
    assert_eq!(
        rig.store.held(&large_key(&wrong)).await.as_deref(),
        Some(&b"too short"[..])
    );
    assert_eq!(missing(&rig, &[wrong]).await.expect("asked"), vec![wrong]);
}

/// Asserts that an object store that cannot say what is under the key fails the write
/// UNAVAILABLE, rather than writing blind.
#[tokio::test]
async fn a_store_that_cannot_say_what_is_under_the_key_fails_the_write() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(5000, 11);
    rig.store.fail_len_of(&large_key(&digest));

    let refused = write(&rig, pieces(&digest, &bytes, 700)).await;

    assert_eq!(refused.expect_err("cannot say").code(), Code::Unavailable);
    assert_eq!(rig.store.stream_puts(), 0);
}

/// Asserts that a large blob already in the index is acknowledged without being read.
#[tokio::test]
async fn a_large_blob_already_held_is_acknowledged_without_its_data() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(5000, 12);
    write(&rig, pieces(&digest, &bytes, 700))
        .await
        .expect("written");
    let first = WriteRequest {
        resource_name: upload_name(&digest),
        write_offset: 0,
        finish_write: false,
        data: bytes[..10].to_vec(),
    };

    let size = write(&rig, vec![first]).await;

    assert_eq!(size.expect("acknowledged"), 5000);
    assert_eq!(rig.store.stream_puts(), 1);
}

/// Records the object under the key of `digest` as the pack of that one blob, as a write would.
fn record(rig: &Rig, digest: &kbf_types::Digest, key: &str, size: u64) {
    rig.index
        .record_pack(&NewPack {
            key: key.to_owned(),
            writer: "test".to_owned(),
            size,
            blobs: vec![PackBlob {
                digest: index_form(digest),
                offset: 0,
            }],
        })
        .expect("recorded");
}

/// Asserts that an object that is not the blob its row says is never served whole: the read
/// fails UNAVAILABLE before it sends the last window, the row is dropped so the blob is missing
/// and a client uploads it again, and a read of part of it is served (a range is not checked).
#[tokio::test]
async fn an_object_that_is_not_its_digest_is_not_served_and_its_row_is_dropped() {
    let rig = Rig::new().await;
    let (digest, _) = blob_len(5000, 13);
    let (_, damaged) = blob_len(5000, 14);
    rig.store.seed(&large_key(&digest), &damaged).await;
    record(&rig, &digest, &large_key(&digest), 5000);
    let name = read_name(&digest);

    let part = read(&rig, &name, 10, 20).await.expect("a range is served");
    let whole = read(&rig, &name, 0, 0).await.expect_err("damaged");
    let after = missing(&rig, &[digest]).await.expect("asked");

    assert_eq!(part, damaged[10..30]);
    assert_eq!(whole.code(), Code::Unavailable);
    assert!(whole.message().contains("other bytes"), "{whole}");
    assert_eq!(after, vec![digest]);
}

/// Asserts that an object shorter than its row says fails the read UNAVAILABLE, and that an
/// object the store cannot read does too, and the same read is served once the store can.
#[tokio::test]
async fn an_object_that_cannot_be_read_whole_fails_the_read_unavailable() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(5000, 15);
    let (cut, _) = blob_len(5000, 16);
    rig.store.seed(&large_key(&cut), &bytes[..100]).await;
    record(&rig, &cut, &large_key(&cut), 5000);
    write(&rig, pieces(&digest, &bytes, 700))
        .await
        .expect("written");
    rig.store.reject_reads(1);

    let refused = read(&rig, &read_name(&digest), 0, 0)
        .await
        .expect_err("store down");
    let served = read(&rig, &read_name(&digest), 0, 0).await.expect("read");
    let short = read(&rig, &read_name(&cut), 0, 0)
        .await
        .expect_err("short object");

    assert_eq!(refused.code(), Code::Unavailable);
    assert_eq!(served, bytes.to_vec());
    assert_eq!(short.code(), Code::Unavailable);
}

/// Asserts that a read that fails after its first window ends its stream with the failure, and
/// does not end as if it were whole.
#[tokio::test]
async fn a_read_that_fails_after_its_first_window_ends_with_the_failure() {
    let rig = Rig::new().await;
    let window = READ_WINDOW_BYTES as usize;
    let (digest, bytes) = blob_len(window + 5000, 17);
    write(&rig, pieces(&digest, &bytes, 1 << 20))
        .await
        .expect("written");
    // The first range read of the next call passes and the second fails.
    rig.store
        .reject_read_after(rig.store.ranges().len() as u64 + 1);

    let failed = read(&rig, &read_name(&digest), 0, 0)
        .await
        .expect_err("second window");

    assert_eq!(failed.code(), Code::Unavailable);
}

/// Asserts that an index that cannot answer fails a read of a large blob UNAVAILABLE, not
/// NOT_FOUND, and that a row naming a key the store cannot hold does too.
#[tokio::test]
async fn a_large_read_the_index_cannot_place_is_unavailable() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(5000, 18);
    write(&rig, pieces(&digest, &bytes, 700))
        .await
        .expect("written");
    let (strange, _) = blob_len(5000, 19);
    record(&rig, &strange, &"x".repeat(5000), 5000);
    rig.index.fail_locate(1);

    let down = read(&rig, &read_name(&digest), 0, 0)
        .await
        .expect_err("index down");
    let unkeyed = read(&rig, &read_name(&strange), 0, 0)
        .await
        .expect_err("no such key");

    assert_eq!(down.code(), Code::Unavailable);
    assert_eq!(unkeyed.code(), Code::Unavailable);
}

/// Asserts that a large blob the index lacks is NOT_FOUND.
#[tokio::test]
async fn a_large_blob_that_is_not_held_is_not_found() {
    let rig = Rig::new().await;
    let (digest, _) = blob_len(5000, 20);

    let absent = read(&rig, &read_name(&digest), 0, 0)
        .await
        .expect_err("absent");

    assert_eq!(absent.code(), Code::NotFound);
}
