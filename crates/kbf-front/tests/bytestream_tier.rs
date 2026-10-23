//! ByteStream `Write` of a blob over `MAX_UNSTAGED_BLOB_BYTES` onto the write tier
//! (`Cache::stage_streamed`): its messages appended to the tier as they arrive, then
//! the blob committed only if the bytes hash to the digest named.
//!
//! The service is served over a loopback port and called as a client does, over the
//! same [`Farm`] `write_tier.rs` runs: a metadata log, a tier directory on a `FaultFs`
//! and a store. Each test's comment names the defect it is there to catch.

mod common;
#[allow(dead_code)]
#[path = "write_tier/support.rs"]
mod support;

use std::net::SocketAddr;
use std::sync::Arc;

use futures::channel::mpsc;
use kbf_front::{MAX_UNSTAGED_BLOB_BYTES, READ_CHUNK_BYTES};
use kbf_meta::BlobAnswer;
use kbf_objstore::Capabilities;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use support::{Farm, TestBlob, TierCache};
use tonic::Code;
use tonic::transport::server::TcpIncoming;
use tonic::transport::{Channel, Endpoint, Server};

const MIB: usize = 1 << 20;

/// A blob one byte over [`MAX_UNSTAGED_BLOB_BYTES`]: written as a stream, not whole.
fn large() -> TestBlob {
    TestBlob::new(3, MAX_UNSTAGED_BLOB_BYTES as usize + 1)
}

/// The cache's ByteStream service on a loopback port, and a client of it.
async fn serve(cache: &Arc<TierCache>) -> ByteStreamClient<Channel> {
    let incoming = TcpIncoming::bind(SocketAddr::from(([127, 0, 0, 1], 0))).expect("bind");
    let addr = incoming.local_addr().expect("address");
    let routes = kbf_front::routes(Arc::clone(cache));
    tokio::spawn(async move {
        Server::builder()
            .add_routes(routes)
            .serve_with_incoming(incoming)
            .await
            .expect("serve");
    });
    let channel = Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    ByteStreamClient::new(channel)
}

/// `bytes` as write messages of `chunk` bytes named `blob`'s resource.
fn messages(blob: &TestBlob, bytes: &[u8], chunk: usize) -> Vec<WriteRequest> {
    let name = format!(
        "main/uploads/7d5e6f1a-test/blobs/{}/{}",
        blob.digest.hash_hex(),
        blob.digest.size_bytes
    );
    let pieces: Vec<&[u8]> = bytes.chunks(chunk).collect();
    let last = pieces.len() - 1;
    pieces
        .iter()
        .enumerate()
        .map(|(i, piece)| WriteRequest {
            resource_name: if i == 0 { name.clone() } else { String::new() },
            write_offset: (i * chunk) as i64,
            finish_write: i == last,
            data: piece.to_vec(),
        })
        .collect()
}

/// Catches: a large write the tier has room for stored anywhere but on the tier (sent
/// whole to the store, so the flusher never has it to write later), left staged but not
/// readable, committed short or with another size than the client sent, or its bytes
/// altered on the way (read back different).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_write_the_tier_has_room_for_is_staged_on_the_tier_and_read_back() {
    let farm = Farm::new(Capabilities::default());
    let cache = Arc::new(farm.start(1 << 30).await);
    let mut client = serve(&cache).await;
    let blob = large();

    let written = client
        .write(futures::stream::iter(messages(&blob, &blob.bytes, MIB)))
        .await
        .expect("Write")
        .into_inner();
    assert_eq!(written.committed_size, blob.digest.size_bytes as i64);
    assert!(
        farm.log.located(&blob.digest).1.is_some(),
        "the blob is not staged on the tier"
    );

    let mut read = client
        .read(ReadRequest {
            resource_name: format!(
                "main/blobs/{}/{}",
                blob.digest.hash_hex(),
                blob.digest.size_bytes
            ),
            read_offset: 0,
            read_limit: 0,
        })
        .await
        .expect("Read")
        .into_inner();
    let mut got = Vec::new();
    while let Some(message) = read.message().await.expect("a message") {
        assert!(message.data.len() <= READ_CHUNK_BYTES);
        got.extend_from_slice(&message.data);
    }
    assert!(
        got == blob.bytes,
        "the bytes read back differ from the bytes written"
    );
}

/// Catches: a large write whose bytes do not hash to the digest in its name stored on
/// the tier (the digest taken on trust), answered as anything but INVALID_ARGUMENT, or
/// leaving the blob findable (a later FindMissing would call it present).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_write_of_other_bytes_than_its_digest_names_is_refused_and_not_stored() {
    let farm = Farm::new(Capabilities::default());
    let cache = Arc::new(farm.start(1 << 30).await);
    let mut client = serve(&cache).await;
    let blob = large();
    let mut altered = blob.bytes.to_vec();
    altered[MIB + 5] ^= 1;

    let refused = client
        .write(futures::stream::iter(messages(&blob, &altered, MIB)))
        .await
        .expect_err("the digest does not match");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");
    assert_eq!(
        farm.log.answer(&blob.digest),
        BlobAnswer::Absent,
        "bytes that fail their digest are stored"
    );
}

/// Catches: a large write whose stream ends before its last message (no `finish_write`)
/// committing the part it got, answered with the tier's error instead of the stream's own
/// refusal (INVALID_ARGUMENT, so a client knows its stream was at fault), or leaving the
/// blob findable. Catches: the stream's refusal dropped in `write`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_write_that_is_cut_short_stores_nothing() {
    let farm = Farm::new(Capabilities::default());
    let cache = Arc::new(farm.start(1 << 30).await);
    let mut client = serve(&cache).await;
    let blob = large();
    let mut cut = messages(&blob, &blob.bytes, MIB);
    cut.truncate(4);
    cut[3].finish_write = false;
    let (tx, rx) = mpsc::unbounded();
    for message in cut {
        tx.unbounded_send(message).expect("send");
    }
    drop(tx);

    let refused = client.write(rx).await.expect_err("cut short");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused:?}");
    assert_eq!(farm.log.answer(&blob.digest), BlobAnswer::Absent);
}
