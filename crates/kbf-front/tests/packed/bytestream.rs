//! ByteStream through a proxy given packs: a Write received whole and packed, a Read served
//! from a pack (all of the blob, or a range of it), and the calls that still go to the node.
//! Each test says the behaviour it asserts.

use std::sync::atomic::Ordering::SeqCst;

use bytes::Bytes;
use futures::stream;
use kbf_front::{Owners, PackSettings, Packs};
use kbf_pack_index::{NewPack, PackBlob, PackIndex as _};
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::{ReadRequest, WriteRequest};
use kbf_types::{Digest, DigestFunction};
use tonic::transport::Channel;
use tonic::{Code, Status};

use crate::harness::{self, Fake, node as fake_node, owner, pattern};
use crate::support::{Rig, blob, index_form, update};

/// The Read name of a blob.
fn name(digest: &Digest) -> String {
    format!("main/blobs/{}/{}", digest.hash_hex(), digest.size_bytes)
}

/// The Write name of a blob.
fn upload(digest: &Digest) -> String {
    format!(
        "main/uploads/u1/blobs/{}/{}",
        digest.hash_hex(),
        digest.size_bytes
    )
}

/// A rig over a fake node that streams, which counts the calls that reach it.
async fn rig_over_fake(settings: PackSettings) -> (Rig, std::sync::Arc<Fake>) {
    let fake = Fake::new(false);
    let owners = Owners::single(owner("a", fake_node(&fake).await));
    (Rig::over(owners, settings).await, fake)
}

/// The bytes a Read of `name` returns, or the status that refused it.
async fn read_range(
    front: &Channel,
    name: String,
    offset: i64,
    limit: i64,
) -> Result<Vec<u8>, Status> {
    let request = ReadRequest {
        resource_name: name,
        read_offset: offset,
        read_limit: limit,
    };
    let mut messages = ByteStreamClient::new(front.clone())
        .read(request)
        .await?
        .into_inner();
    let mut out = Vec::new();
    while let Some(message) = messages.message().await? {
        out.extend_from_slice(&message.data);
    }
    Ok(out)
}

/// One message of a Write: the name (the first message only), the offset, a piece of the blob.
fn message(name: &str, offset: i64, data: &[u8], finish_write: bool) -> WriteRequest {
    WriteRequest {
        resource_name: name.to_owned(),
        write_offset: offset,
        finish_write,
        data: data.to_vec(),
    }
}

/// The size a Write of `messages` is acknowledged with, or the status that ended it.
async fn write(front: &Channel, messages: Vec<WriteRequest>) -> Result<i64, Status> {
    let sent = ByteStreamClient::new(front.clone())
        .write(stream::iter(messages))
        .await?;
    Ok(sent.into_inner().committed_size)
}

/// The size a Write of the whole of `bytes` in one message is acknowledged with, or the status
/// that ended it.
async fn write_whole(front: &Channel, digest: &Digest, bytes: &[u8]) -> Result<i64, Status> {
    write(front, vec![message(&upload(digest), 0, bytes, true)]).await
}

/// A pack the index records for `digest` at offset 0, of `size` bytes, under `key`.
fn row(rig: &Rig, key: &str, size: u64, digest: &Digest) {
    let pack = NewPack {
        key: key.to_owned(),
        writer: "f/fe1".to_owned(),
        size,
        blobs: vec![PackBlob {
            digest: index_form(digest),
            offset: 0,
        }],
    };
    rig.index.record_pack(&pack).expect("recorded");
}

/// The digest the fake node's blobs of `kind` and `size` have in their names.
fn fake_digest(kind: char, size: u64) -> Digest {
    let text = format!("{}/{size}", harness::hash(kind));
    Digest::parse(DigestFunction::Sha256, &text).expect("a digest")
}

/// Asserts that a Write is received whole, packed, and acknowledged with the blob's size after
/// the pack is stored and the index has its row, without a call to the node; and that a Read of
/// it is served from the pack with one ranged request of exactly the blob's bytes, again
/// without a call to the node.
#[tokio::test]
async fn a_write_is_packed_and_a_read_is_served_from_the_pack() {
    let (rig, fake) = rig_over_fake(PackSettings::default()).await;
    let (digest, bytes) = blob("a blob sent in two messages");
    let (head, tail) = bytes.split_at(9);
    let messages = vec![
        message(&upload(&digest), 0, head, false),
        message("", 9, tail, true),
    ];

    let acknowledged = write(&rig.front, messages).await;

    assert_eq!(acknowledged.expect("acknowledged"), bytes.len() as i64);
    assert_eq!(rig.store.keys().await.len(), 1);
    assert!(rig.index.has_all(&[digest]));
    assert_eq!(rig.index.held_at_record(), [true]);
    assert_eq!(fake.calls.load(SeqCst), 0);

    let got = read_range(&rig.front, name(&digest), 0, 0).await;

    assert_eq!(got.expect("read"), bytes.to_vec());
    assert_eq!(fake.calls.load(SeqCst), 0);
    let ranges = rig.store.ranges();
    assert_eq!(ranges.len(), 1, "{ranges:?}");
    assert_eq!(ranges[0].2, bytes.len() as u64);
}

/// Asserts that a read of a range of a blob in a pack fetches the blob's bytes and no others of
/// its pack (one request, at the blob's offset, as long as the blob), cuts the range from the
/// hashed blob (`read_offset`, `read_limit`, a limit past the end, a limit of 0 for the rest),
/// and refuses a negative offset or limit and an offset past the end before it touches the
/// store. The pack here is 60 bytes of blobs and a footer, and the request is for 20.
#[tokio::test]
async fn a_ranged_read_fetches_only_the_blob_and_cuts_the_range() {
    let rig = Rig::new().await;
    let first = blob("0123456789");
    let middle = blob("abcdefghijklmnopqrst");
    let last = blob("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123");
    let uploaded = update(&rig.front, &[first, middle.clone(), last]).await;
    assert_eq!(uploaded, [0, 0, 0]);
    let places = rig.index.where_is(&[middle.0]);
    let offset = places[0].as_ref().expect("a row").offset;

    let got = read_range(&rig.front, name(&middle.0), 3, 5).await;

    assert_eq!(got.expect("read"), b"defgh");
    let ranges = rig.store.ranges();
    assert_eq!(ranges.len(), 1, "{ranges:?}");
    assert_eq!((ranges[0].1, ranges[0].2), (offset, 20));
    let rest = read_range(&rig.front, name(&middle.0), 15, 0).await;
    assert_eq!(rest.expect("read"), b"pqrst");
    let clipped = read_range(&rig.front, name(&middle.0), 18, 100).await;
    assert_eq!(clipped.expect("read"), b"st");
    let empty = read_range(&rig.front, name(&middle.0), 20, 0).await;
    assert!(empty.expect("read").is_empty());

    let reads = rig.store.ranges().len();
    for (offset, limit, code) in [
        (21, 0, Code::OutOfRange),
        (-1, 0, Code::OutOfRange),
        (0, -1, Code::InvalidArgument),
    ] {
        let refused = read_range(&rig.front, name(&middle.0), offset, limit).await;
        assert_eq!(refused.expect_err("refused").code(), code);
    }
    let after = rig.store.ranges().len();
    assert_eq!(after, reads, "a bad range reached the store");
}

/// Asserts that a blob of no bytes is packed and read back: the Write is acknowledged with 0, a
/// Read answers no data and OK.
#[tokio::test]
async fn an_empty_blob_is_packed_and_read_back() {
    let (rig, fake) = rig_over_fake(PackSettings::default()).await;
    let (digest, _) = blob("");

    let acknowledged = write(&rig.front, vec![message(&upload(&digest), 0, b"", true)]).await;

    assert_eq!(acknowledged.expect("acknowledged"), 0);
    assert!(rig.index.has_all(&[digest]));
    let got = read_range(&rig.front, name(&digest), 0, 0).await;
    assert!(got.expect("read").is_empty());
    assert_eq!(fake.calls.load(SeqCst), 0);
}

/// Asserts that a blob the index lacks is read from the node as without packs, and counted as a
/// fallback: a blob uploaded before packs were on is still found.
#[tokio::test]
async fn a_blob_the_index_lacks_is_read_from_the_node() {
    let (rig, fake) = rig_over_fake(PackSettings::default()).await;

    let got = read_range(&rig.front, harness::blob("main", '0', 5), 0, 0).await;

    assert_eq!(got.expect("read"), vec![pattern(0); 5]);
    assert_eq!(fake.calls.load(SeqCst), 1);
    assert_eq!(rig.packs.metrics().fallbacks.get(), 1);
}

/// Asserts that a row whose pack the store lacks does not fail the read: the node is asked, and
/// the loss is counted apart from an ordinary fallback.
#[tokio::test]
async fn a_pack_that_is_gone_falls_back_to_the_node() {
    let (rig, fake) = rig_over_fake(PackSettings::default()).await;
    row(&rig, "cas/f/fe1/gone/0000", 64, &fake_digest('0', 5));

    let got = read_range(&rig.front, harness::blob("main", '0', 5), 0, 0).await;

    assert_eq!(got.expect("read"), vec![pattern(0); 5]);
    assert_eq!(fake.calls.load(SeqCst), 1);
    let metrics = rig.packs.metrics();
    assert_eq!((metrics.lost.get(), metrics.fallbacks.get()), (1, 0));
}

/// Asserts that a pack holding other bytes than the digest names is never served: the Read fails
/// UNAVAILABLE, the node is not asked, and the mismatch is counted.
#[tokio::test]
async fn a_pack_with_other_bytes_is_never_served() {
    let (rig, fake) = rig_over_fake(PackSettings::default()).await;
    let (digest, bytes) = blob("the bytes this digest names");
    rig.store
        .seed("cas/f/fe1/bad/0000", &vec![b'X'; bytes.len()])
        .await;
    row(&rig, "cas/f/fe1/bad/0000", bytes.len() as u64, &digest);

    let refused = read_range(&rig.front, name(&digest), 0, 0).await;

    let refused = refused.expect_err("not served");
    assert_eq!(refused.code(), Code::Unavailable);
    assert!(refused.message().contains("other bytes"), "{refused}");
    assert_eq!(fake.calls.load(SeqCst), 0);
    assert_eq!(rig.packs.metrics().digest_mismatches.get(), 1);
}

/// Asserts what stays on the node: a Write of a blob over the packs' bound, a Write and a Read
/// of a compressed blob, and a Read of a blob over the bound whose row the index has: each is
/// answered by the node, and no pack is written.
#[tokio::test]
async fn compressed_blobs_and_blobs_over_the_bound_stay_on_the_node() {
    let settings = PackSettings {
        max_blob_bytes: 8,
        ..PackSettings::default()
    };
    let (rig, fake) = rig_over_fake(settings).await;
    let big = vec![pattern(0); 16];
    let big_digest = kbf_segments::sha256(&big);

    let messages = vec![message(&upload(&big_digest), 0, &big, true)];
    let sent = write(&rig.front, messages).await;
    assert_eq!(sent.expect("acknowledged"), 16);
    assert_eq!(fake.calls.load(SeqCst), 1);

    let small = vec![pattern(0); 4];
    let digest = kbf_segments::sha256(&small);
    let compressed = format!(
        "main/uploads/u1/compressed-blobs/zstd/{}/{}",
        digest.hash_hex(),
        digest.size_bytes
    );
    let sent = write(&rig.front, vec![message(&compressed, 0, &small, true)]).await;
    assert_eq!(sent.expect("acknowledged"), 4);
    assert_eq!(fake.calls.load(SeqCst), 2);

    // A name with a hash the fake node serves; a real hash may start as one it refuses.
    let compressed = format!("main/compressed-blobs/zstd/{}/4", harness::hash('0'));
    let got = read_range(&rig.front, compressed, 0, 0).await;
    assert_eq!(got.expect("read"), small);
    assert_eq!(fake.calls.load(SeqCst), 3);

    row(&rig, "cas/f/fe1/big/0000", 64, &fake_digest('0', 16));
    let got = read_range(&rig.front, harness::blob("main", '0', 16), 0, 0).await;
    assert_eq!(got.expect("read").len(), 16);
    assert_eq!(fake.calls.load(SeqCst), 4);
    let metrics = rig.packs.metrics();
    assert_eq!((metrics.lost.get(), metrics.fallbacks.get()), (0, 0));
    assert!(rig.store.keys().await.is_empty());
    assert_eq!(rig.store.puts(), 0);
}

/// Asserts that a Write that breaks the rules of an upload is refused INVALID_ARGUMENT and packs
/// nothing: an offset that is not where the bytes stand, a second message that names another
/// resource, more bytes than the digest says, fewer bytes, bytes that are not the digest, and a
/// stream that ends without `finish_write`. None of them is counted as a failure to write a
/// pack.
#[tokio::test]
async fn a_write_that_breaks_the_rules_is_refused_and_packs_nothing() {
    let (rig, _fake) = rig_over_fake(PackSettings::default()).await;
    let (digest, bytes) = blob("exactly this");
    let target = upload(&digest);
    let (head, tail) = bytes.split_at(5);
    let cases = [
        ("offset", vec![message(&target, 3, &bytes, true)]),
        (
            "resource",
            vec![
                message(&target, 0, head, false),
                message("main/uploads/u2/blobs/other/1", 5, tail, true),
            ],
        ),
        (
            "more",
            vec![message(&target, 0, b"exactly this and more", true)],
        ),
        ("fewer", vec![message(&target, 0, head, true)]),
        ("digest", vec![message(&target, 0, b"exactly that", true)]),
        ("unfinished", vec![message(&target, 0, &bytes, false)]),
    ];

    for (case, messages) in cases {
        let refused = write(&rig.front, messages).await.expect_err(case);
        assert_eq!(refused.code(), Code::InvalidArgument, "{case}: {refused}");
    }

    assert!(rig.store.keys().await.is_empty());
    assert!(rig.index.has_none(&[digest]));
    assert_eq!(rig.store.puts(), 0);
    assert_eq!(rig.packs.metrics().write_failures.get(), 0);
}

/// Asserts that a pack the store refuses fails the Write UNAVAILABLE and records nothing, and
/// that the same Write is acknowledged once the store takes writes.
#[tokio::test]
async fn a_write_whose_pack_cannot_be_stored_is_unavailable() {
    let (rig, _fake) = rig_over_fake(PackSettings::default()).await;
    let (digest, bytes) = blob("will be stored later");
    rig.store.reject_puts(10);

    let refused = write(&rig.front, vec![message(&upload(&digest), 0, &bytes, true)]).await;

    assert_eq!(refused.expect_err("refused").code(), Code::Unavailable);
    assert!(rig.index.has_none(&[digest]));
    rig.store.reject_puts(0);
    let retried = write(&rig.front, vec![message(&upload(&digest), 0, &bytes, true)]).await;
    assert_eq!(retried.expect("acknowledged"), bytes.len() as i64);
    assert!(rig.index.has_all(&[digest]));
}

/// Asserts that Writes whose blobs together are over all of the room all succeed: with room for
/// 100 bytes, six Writes of 40 bytes sent together are all acknowledged and stored, and the
/// bytes reserved at once never pass 100 and are given back.
#[tokio::test]
async fn writes_that_together_are_over_the_room_all_succeed() {
    let settings = PackSettings {
        inflight_bytes: 100,
        ..PackSettings::default()
    };
    let rig = Rig::with(settings).await;
    let blobs: Vec<(Digest, Bytes)> = (0..6).map(|i| blob(&format!("{i:0>40}"))).collect();
    let writes = blobs
        .iter()
        .map(|(digest, bytes)| write_whole(&rig.front, digest, bytes));

    let acknowledged = futures::future::join_all(writes).await;

    for sent in acknowledged {
        assert_eq!(sent.expect("acknowledged"), 40);
    }
    let metrics = rig.packs.metrics();
    assert!(metrics.inflight_peak_bytes() >= 40);
    assert!(metrics.inflight_peak_bytes() <= 100);
    assert_eq!(metrics.inflight_bytes(), 0);
    for (digest, _) in &blobs {
        assert!(rig.index.has_all(&[*digest]));
    }
}

/// Asserts that a Write of a blob larger than all of the room is refused RESOURCE_EXHAUSTED,
/// naming the flag, and stores nothing, while a Write that fits is acknowledged.
#[tokio::test]
async fn a_write_larger_than_all_the_room_is_refused() {
    let settings = PackSettings {
        inflight_bytes: 16,
        ..PackSettings::default()
    };
    let rig = Rig::with(settings).await;
    let (big, big_bytes) = blob("a blob of more than sixteen bytes");
    let (small, small_bytes) = blob("sixteen bytes!!!");

    let refused = write_whole(&rig.front, &big, &big_bytes).await;

    let refused = refused.expect_err("refused");
    assert_eq!(refused.code(), Code::ResourceExhausted);
    let said = refused.message();
    assert!(said.contains("--pack-inflight-bytes"), "{said}");
    assert!(rig.index.has_none(&[big]));
    assert_eq!(rig.packs.metrics().upload_refusals.get(), 1);
    let fits = write_whole(&rig.front, &small, &small_bytes).await;
    assert_eq!(fits.expect("acknowledged"), 16);
}

// The idle timeout of an upload is not tested over gRPC, where it would wait on a real clock.
// The tests in `src/proxy/packed/bytestream.rs` run it on a paused clock: a stalled upload is
// ended DEADLINE_EXCEEDED once the timeout has passed since its last byte and not before, stores
// nothing, is counted, and gives its room back; an upload that keeps sending is never cut; an
// empty frame is not progress, and the tests run it with a timeout that is not the default.
// `the_defaults_are_the_frontends` (`store.rs`) asserts only the default timeout, and
// `the_configured_idle_timeout_is_the_one_the_packs_keep` (`store.rs`) asserts that a timeout
// given in `PackSettings` is the one `Packs` keeps.

/// Asserts that the families of the pack layer are in the text a scrape gets, with the counts of
/// what was done.
#[tokio::test]
async fn the_counters_are_rendered() {
    let rig = Rig::new().await;
    let (digest, bytes) = blob("counted");
    let sent = write(&rig.front, vec![message(&upload(&digest), 0, &bytes, true)]).await;
    sent.expect("acknowledged");
    let read = read_range(&rig.front, name(&digest), 0, 0).await;
    read.expect("read");

    let mut none = String::from("kept\n");
    Packs::render_metrics_of(None, &mut none);
    assert_eq!(none, "kept\n");
    let mut text = String::from("kept\n");
    Packs::render_metrics_of(Some(&rig.packs), &mut text);

    assert!(text.starts_with("kept\n"), "{text}");
    for line in [
        "kbf_pack_writes_total 1\n",
        "kbf_pack_written_blobs_total 1\n",
        "kbf_pack_reads_total 1\n",
        "kbf_pack_read_fallbacks_total 0\n",
        "kbf_pack_upload_waits_total 0\n",
        "kbf_pack_upload_refusals_total 0\n",
        "kbf_pack_upload_idle_timeouts_total 0\n",
        "kbf_pack_inflight_bytes 0\n",
        "kbf_pack_inflight_peak_bytes 7\n",
    ] {
        assert!(text.contains(line), "{line:?} missing from\n{text}");
    }
    assert!(text.contains("# TYPE kbf_pack_digest_mismatches_total counter"));
    assert!(text.contains("# TYPE kbf_pack_inflight_bytes gauge"));
}
