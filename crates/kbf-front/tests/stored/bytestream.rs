//! ByteStream of a frontend with no storage node, for blobs that are packed: Read, the rules of
//! a Write, and QueryWriteStatus.

use futures::{StreamExt as _, stream};
use kbf_front::PackSettings;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{QueryWriteStatusRequest, ReadRequest, WriteRequest};
use kbf_types::Digest;
use tonic::{Code, Status};

use crate::cas::missing;
use crate::rig::{Rig, blob_len, blob_of};

pub fn read_name(digest: &Digest) -> String {
    format!("blobs/{}/{}", digest.hash_hex(), digest.size_bytes)
}

pub fn upload_name(digest: &Digest) -> String {
    format!(
        "uploads/u1/blobs/{}/{}",
        digest.hash_hex(),
        digest.size_bytes
    )
}

/// The messages that write `bytes` of `digest` in pieces of `piece` bytes.
pub fn pieces(digest: &Digest, bytes: &[u8], piece: usize) -> Vec<WriteRequest> {
    let chunks: Vec<&[u8]> = if bytes.is_empty() {
        vec![&[]]
    } else {
        bytes.chunks(piece).collect()
    };
    let last = chunks.len() - 1;
    let mut offset = 0;
    let mut out = Vec::new();
    for (i, chunk) in chunks.into_iter().enumerate() {
        out.push(WriteRequest {
            resource_name: if i == 0 {
                upload_name(digest)
            } else {
                String::new()
            },
            write_offset: offset,
            finish_write: i == last,
            data: chunk.to_vec(),
        });
        offset += chunk.len() as i64;
    }
    out
}

/// What a Write of `messages` is acknowledged with, or the status that ended it.
pub async fn write(rig: &Rig, messages: Vec<WriteRequest>) -> Result<i64, Status> {
    let sent = ByteStreamClient::new(rig.front.clone())
        .write(stream::iter(messages))
        .await?;
    Ok(sent.into_inner().committed_size)
}

/// The bytes a Read of `name` from `offset`, at most `limit` (0: to the end), delivers.
pub async fn read(rig: &Rig, name: &str, offset: i64, limit: i64) -> Result<Vec<u8>, Status> {
    let request = ReadRequest {
        resource_name: name.to_owned(),
        read_offset: offset,
        read_limit: limit,
    };
    let mut messages = ByteStreamClient::new(rig.front.clone())
        .read(request)
        .await?
        .into_inner();
    let mut out = Vec::new();
    while let Some(message) = messages.message().await? {
        out.extend_from_slice(&message.data);
    }
    Ok(out)
}

/// What QueryWriteStatus of `name` says: the committed size and whether it is complete.
pub async fn status(rig: &Rig, name: &str) -> Result<(i64, bool), Status> {
    let answer = ByteStreamClient::new(rig.front.clone())
        .query_write_status(QueryWriteStatusRequest {
            resource_name: name.to_owned(),
        })
        .await?
        .into_inner();
    Ok((answer.committed_size, answer.complete))
}

/// Asserts that a blob written in several messages is acknowledged at its size, is held, and is
/// read back whole and by range (an offset, a limit, an offset and a limit, the empty range at
/// the end) in the bytes that were written.
#[tokio::test]
async fn a_written_blob_is_read_back_whole_and_by_range() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(900, 1);

    let size = write(&rig, pieces(&digest, &bytes, 300)).await;

    assert_eq!(size.expect("written"), 900);
    assert!(missing(&rig, &[digest]).await.expect("asked").is_empty());
    let name = read_name(&digest);
    assert_eq!(read(&rig, &name, 0, 0).await.expect("read"), bytes.to_vec());
    assert_eq!(read(&rig, &name, 100, 0).await.expect("read"), bytes[100..]);
    assert_eq!(read(&rig, &name, 0, 50).await.expect("read"), bytes[..50]);
    assert_eq!(
        read(&rig, &name, 10, 20).await.expect("read"),
        bytes[10..30]
    );
    assert_eq!(
        read(&rig, &name, 900, 0).await.expect("read"),
        Vec::<u8>::new()
    );
    assert_eq!(
        read(&rig, &name, 880, 100).await.expect("read"),
        bytes[880..]
    );
}

/// Asserts that an empty blob is written and read.
#[tokio::test]
async fn an_empty_blob_is_written_and_read() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_of(b"");

    let size = write(&rig, pieces(&digest, &bytes, 10)).await;

    assert_eq!(size.expect("written"), 0);
    let read = read(&rig, &read_name(&digest), 0, 0).await;
    assert_eq!(read.expect("read"), Vec::<u8>::new());
}

/// Asserts that a blob in a pack and no storage node to fall back to is NOT_FOUND when the
/// index lacks it, the compressed spelling of its name is INVALID_ARGUMENT, and names that are
/// not blobs are INVALID_ARGUMENT.
#[tokio::test]
async fn a_read_of_what_is_not_held_or_not_plain_is_refused() {
    let rig = Rig::new().await;
    let (digest, _) = blob_of(b"absent");
    let hash = digest.hash_hex();

    let absent = read(&rig, &read_name(&digest), 0, 0)
        .await
        .expect_err("absent");
    let zipped = format!("compressed-blobs/zstd/{hash}/{}", digest.size_bytes);
    let compressed = read(&rig, &zipped, 0, 0).await.expect_err("compressed");
    let nonsense = read(&rig, "nothing", 0, 0).await.expect_err("not a blob");

    assert_eq!(absent.code(), Code::NotFound);
    assert_eq!(compressed.code(), Code::InvalidArgument);
    assert!(compressed.message().contains("compressed"), "{compressed}");
    assert_eq!(nonsense.code(), Code::InvalidArgument);
}

/// Asserts that a negative offset or an offset past the end is OUT_OF_RANGE and a negative limit
/// is INVALID_ARGUMENT, whether or not the blob is held.
#[tokio::test]
async fn a_read_range_that_is_not_one_is_refused() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(100, 2);
    write(&rig, pieces(&digest, &bytes, 100))
        .await
        .expect("written");
    let name = read_name(&digest);

    let negative = read(&rig, &name, -1, 0).await.expect_err("negative offset");
    let past = read(&rig, &name, 101, 0).await.expect_err("past the end");
    let limit = read(&rig, &name, 0, -1).await.expect_err("negative limit");

    assert_eq!(negative.code(), Code::OutOfRange);
    assert_eq!(past.code(), Code::OutOfRange);
    assert_eq!(limit.code(), Code::InvalidArgument);
}

/// Asserts that a blob whose bytes are not its digest is refused INVALID_ARGUMENT and is not
/// held, and that a blob of fewer or more bytes than its digest names is too.
#[tokio::test]
async fn a_write_of_other_bytes_than_its_digest_is_refused() {
    let rig = Rig::new().await;
    let (digest, _) = blob_len(100, 3);
    let (_, other) = blob_len(100, 4);
    let (_, short) = blob_len(60, 3);
    let (_, long) = blob_len(140, 3);

    for bytes in [other, short, long] {
        let sent = write(&rig, pieces(&digest, &bytes, 40)).await;
        assert_eq!(
            sent.expect_err("not the blob").code(),
            Code::InvalidArgument
        );
    }

    assert_eq!(missing(&rig, &[digest]).await.expect("asked"), vec![digest]);
}

/// Asserts the rules of an upload: it starts at offset 0 and each message is at the next offset,
/// names no other resource, and the stream ends with `finish_write`, and each breach is
/// INVALID_ARGUMENT that holds nothing.
#[tokio::test]
async fn a_write_that_breaks_the_rules_of_an_upload_is_refused() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(100, 5);
    let name = upload_name(&digest);
    let message = |name: &str, offset: i64, finish: bool, data: &[u8]| WriteRequest {
        resource_name: name.to_owned(),
        write_offset: offset,
        finish_write: finish,
        data: data.to_vec(),
    };
    let broken = vec![
        // Starts past 0.
        vec![message(&name, 10, true, &bytes[..90])],
        // Skips bytes.
        vec![
            message(&name, 0, false, &bytes[..50]),
            message("", 60, true, &bytes[50..]),
        ],
        // Names another resource halfway.
        vec![
            message(&name, 0, false, &bytes[..50]),
            message("uploads/u1/blobs/other/1", 50, true, &bytes[50..]),
        ],
        // Ends without finish_write.
        vec![message(&name, 0, false, &bytes)],
        // More than the digest names, in one message.
        vec![message(&name, 0, true, &[bytes.to_vec(), vec![0]].concat())],
        // No message at all.
        Vec::new(),
        // A name that is not an upload of a blob.
        vec![message("blobs/x/1", 0, true, b"x")],
        // The compressed spelling.
        vec![message(
            &format!("uploads/u1/compressed-blobs/zstd/{}/100", digest.hash_hex()),
            0,
            true,
            &bytes,
        )],
    ];

    for messages in broken {
        let sent = write(&rig, messages).await;
        assert_eq!(sent.expect_err("refused").code(), Code::InvalidArgument);
    }

    assert_eq!(missing(&rig, &[digest]).await.expect("asked"), vec![digest]);
}

/// Asserts that a blob the index already holds is acknowledged at once, without its data: a
/// write that sends its first message and no more is answered, because there is nothing left to
/// receive.
#[tokio::test]
async fn a_blob_already_held_is_acknowledged_without_its_data() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(100, 6);
    write(&rig, pieces(&digest, &bytes, 100))
        .await
        .expect("written");
    let puts = rig.store.puts();
    let first = WriteRequest {
        resource_name: upload_name(&digest),
        write_offset: 0,
        finish_write: false,
        data: bytes[..10].to_vec(),
    };

    let size = write(&rig, vec![first]).await;

    assert_eq!(size.expect("acknowledged"), 100);
    assert_eq!(rig.store.puts(), puts, "nothing was stored again");
}

/// Asserts that an upload that the object store refuses is UNAVAILABLE and holds nothing, and
/// the same upload sent again, once the store takes it, is stored.
#[tokio::test]
async fn a_write_the_store_refuses_is_unavailable_and_can_be_sent_again() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(100, 7);
    rig.store.reject_puts(8);

    let refused = write(&rig, pieces(&digest, &bytes, 100)).await;
    rig.store.reject_puts(0);
    let again = write(&rig, pieces(&digest, &bytes, 100)).await;

    assert_eq!(refused.expect_err("refused").code(), Code::Unavailable);
    assert_eq!(again.expect("written"), 100);
    assert!(missing(&rig, &[digest]).await.expect("asked").is_empty());
}

/// Asserts that a blob that is alone over all of the room that uploads share is
/// RESOURCE_EXHAUSTED before it is read, while smaller blobs are not.
#[tokio::test]
async fn a_blob_over_all_of_the_room_is_resource_exhausted() {
    let settings = PackSettings {
        max_blob_bytes: 1024,
        inflight_bytes: 64,
        ..PackSettings::default()
    };
    let rig = Rig::with(settings).await;
    let (big, big_bytes) = blob_len(100, 8);
    let (small, small_bytes) = blob_len(50, 8);

    let refused = write(&rig, pieces(&big, &big_bytes, 100)).await;
    let taken = write(&rig, pieces(&small, &small_bytes, 50)).await;

    assert_eq!(
        refused.expect_err("over the room").code(),
        Code::ResourceExhausted
    );
    assert_eq!(taken.expect("written"), 50);
}

/// Asserts that an upload that sends its first message and then no byte for the idle timeout is
/// ended DEADLINE_EXCEEDED and holds nothing. The clock is the runtime's, paused, so the
/// timeout passes when nothing else can run, not by waiting.
#[tokio::test(start_paused = true)]
async fn an_upload_that_goes_silent_is_ended_deadline_exceeded() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(100, 9);
    let first = WriteRequest {
        resource_name: upload_name(&digest),
        write_offset: 0,
        finish_write: false,
        data: bytes[..10].to_vec(),
    };
    let silent = stream::once(async move { first }).chain(stream::pending());

    let ended = ByteStreamClient::new(rig.front.clone()).write(silent).await;

    assert_eq!(ended.expect_err("ended").code(), Code::DeadlineExceeded);
    assert_eq!(missing(&rig, &[digest]).await.expect("asked"), vec![digest]);
}

/// Asserts QueryWriteStatus: a blob that is not held and not being uploaded is 0 and incomplete,
/// one in flight here reports the bytes that have arrived and is incomplete, and once held it
/// reports its size and complete. A compressed name is INVALID_ARGUMENT.
#[tokio::test(start_paused = true)]
async fn the_write_status_follows_the_upload() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(100, 10);
    let name = upload_name(&digest);
    assert_eq!(status(&rig, &name).await.expect("asked"), (0, false));
    let first = WriteRequest {
        resource_name: name.clone(),
        write_offset: 0,
        finish_write: false,
        data: bytes[..40].to_vec(),
    };
    let (rest, finish) = tokio::sync::oneshot::channel::<()>();
    let tail = WriteRequest {
        resource_name: String::new(),
        write_offset: 40,
        finish_write: true,
        data: bytes[40..].to_vec(),
    };
    let held_open = stream::once(async move { first }).chain(stream::once(async move {
        finish.await.ok();
        tail
    }));
    let client = rig.front.clone();
    let writing = tokio::spawn(async move { ByteStreamClient::new(client).write(held_open).await });

    // The 40 bytes of the first message arrive, and the upload waits for the rest.
    let mut seen = status(&rig, &name).await.expect("asked");
    while seen != (40, false) {
        tokio::task::yield_now().await;
        seen = status(&rig, &name).await.expect("asked");
    }
    rest.send(()).expect("the upload is waiting");
    writing.await.expect("joined").expect("written");

    assert_eq!(status(&rig, &name).await.expect("asked"), (100, true));
    let zipped = format!("uploads/u1/compressed-blobs/zstd/{}/100", digest.hash_hex());
    let refused = status(&rig, &zipped).await.expect_err("compressed");
    assert_eq!(refused.code(), Code::InvalidArgument);
}

/// Asserts that two uploads of one name in flight leave the status of the later, and that the
/// earlier ending does not remove the later from the table.
#[tokio::test(start_paused = true)]
async fn an_upload_ending_does_not_remove_a_later_upload_of_its_name() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(100, 11);
    let name = upload_name(&digest);
    let open = |end: tokio::sync::oneshot::Receiver<()>, upto: usize| {
        let first = WriteRequest {
            resource_name: name.clone(),
            write_offset: 0,
            finish_write: false,
            data: bytes[..upto].to_vec(),
        };
        let client = rig.front.clone();
        let sent = stream::once(async move { first }).chain(stream::once(async move {
            end.await.ok();
            WriteRequest {
                resource_name: String::new(),
                write_offset: 0,
                finish_write: false,
                data: Vec::new(),
            }
        }));
        tokio::spawn(async move { ByteStreamClient::new(client).write(sent).await })
    };
    let (end_first, first_ends) = tokio::sync::oneshot::channel();
    let (end_second, second_ends) = tokio::sync::oneshot::channel();
    let first = open(first_ends, 10);
    while status(&rig, &name).await.expect("asked") != (10, false) {
        tokio::task::yield_now().await;
    }
    let second = open(second_ends, 20);
    while status(&rig, &name).await.expect("asked") != (20, false) {
        tokio::task::yield_now().await;
    }

    // The first upload ends (its next message is out of order, so it is refused).
    end_first.send(()).expect("waiting");
    let refused = first.await.expect("joined").expect_err("out of order");

    assert_eq!(refused.code(), Code::InvalidArgument);
    assert_eq!(status(&rig, &name).await.expect("asked"), (20, false));
    end_second.send(()).expect("waiting");
    second.await.expect("joined").expect_err("out of order");
    assert_eq!(status(&rig, &name).await.expect("asked"), (0, false));
}

/// Asserts that a pack index that cannot answer fails Write and QueryWriteStatus UNAVAILABLE and
/// a Read of a small blob UNAVAILABLE, rather than reporting the blob absent.
#[tokio::test]
async fn an_index_that_cannot_answer_fails_unavailable() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob_len(100, 12);
    write(&rig, pieces(&digest, &bytes, 100))
        .await
        .expect("written");

    rig.index.fail_find(1);
    let query = status(&rig, &upload_name(&digest))
        .await
        .expect_err("index down");
    rig.index.fail_find(1);
    let sent = write(&rig, pieces(&digest, &bytes, 100))
        .await
        .expect_err("index down");
    rig.index.fail_locate(1);
    let got = read(&rig, &read_name(&digest), 0, 0)
        .await
        .expect_err("index down");

    for failed in [query, sent, got] {
        assert_eq!(failed.code(), Code::Unavailable);
    }
}
