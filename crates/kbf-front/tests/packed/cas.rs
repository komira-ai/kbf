//! The batch CAS calls of a proxy given packs: what an upload writes, what FindMissingBlobs
//! asks, and what BatchReadBlobs serves. Each test says the behaviour it asserts.

use std::sync::Arc;

use bytes::Bytes;
use kbf_auth::Authorizers;
use kbf_front::{Owner, Owners, PackSettings, Proxy};
use kbf_proto::reapi;
use kbf_types::Digest;
use tonic::Code;

use crate::harness;
use crate::support::{Rig, blob, item, missing, node, read, send_update, update};

const OK: i32 = Code::Ok as i32;
const UNAVAILABLE: i32 = Code::Unavailable as i32;
const INVALID: i32 = Code::InvalidArgument as i32;
const NOT_FOUND: i32 = Code::NotFound as i32;

fn digests(blobs: &[(Digest, Bytes)]) -> Vec<Digest> {
    blobs.iter().map(|(digest, _)| *digest).collect()
}

/// Asserts the write rule on the frontend's path: a batch of three blobs is one pack in the
/// object store and a row each in the index when the upload is acknowledged, the store held the
/// pack when the index recorded it, and the storage node holds none of the blobs.
#[tokio::test]
async fn an_upload_is_one_pack_and_in_the_index_when_it_is_acknowledged() {
    let rig = Rig::new().await;
    let blobs = [blob("alpha"), blob("beta"), blob("gamma")];

    assert_eq!(update(&rig.front, &blobs).await, [OK, OK, OK]);

    let keys = rig.store.keys().await;
    assert_eq!(keys.len(), 1, "{keys:?}");
    assert!(keys[0].starts_with("cas/f/fe1/boot1/"), "{keys:?}");
    for place in rig.index.where_is(&digests(&blobs)) {
        assert_eq!(place.expect("a row").pack_key, keys[0]);
    }
    assert_eq!(rig.index.held_at_record(), [true]);
    for (digest, _) in &blobs {
        assert!(!rig.node_holds(digest).await, "{digest} is on the node");
    }
    let metrics = rig.packs.metrics();
    assert_eq!(metrics.writes.get(), 1);
    assert_eq!(metrics.written_blobs.get(), 3);
}

/// Asserts that a pack the store refuses fails the upload and records nothing: every item is
/// UNAVAILABLE, the index has no row, the store holds no pack, and the failure is counted; the
/// same upload sent again, once the store takes writes, is acknowledged.
#[tokio::test]
async fn a_pack_put_that_fails_fails_the_upload_and_records_nothing() {
    let rig = Rig::new().await;
    let blobs = [blob("one"), blob("two")];
    rig.store.reject_puts(10);

    assert_eq!(update(&rig.front, &blobs).await, [UNAVAILABLE, UNAVAILABLE]);

    assert!(rig.store.keys().await.is_empty());
    assert!(rig.index.has_none(&digests(&blobs)));
    assert!(rig.index.held_at_record().is_empty());
    assert_eq!(rig.packs.metrics().write_failures.get(), 1);
    rig.store.reject_puts(0);
    assert_eq!(update(&rig.front, &blobs).await, [OK, OK]);
    assert!(rig.index.has_all(&digests(&blobs)));
}

/// Asserts that an index that cannot record the pack fails the upload: the items are
/// UNAVAILABLE and no row exists, though the store holds the pack (an object no row points at,
/// which garbage collection deletes); sent again, the upload is acknowledged with a pack of its
/// own and the rows point at that one.
#[tokio::test]
async fn an_index_that_cannot_record_fails_the_upload() {
    let rig = Rig::new().await;
    let blobs = [blob("left"), blob("right")];
    rig.index.fail_record(10);

    assert_eq!(update(&rig.front, &blobs).await, [UNAVAILABLE, UNAVAILABLE]);

    assert_eq!(rig.store.keys().await.len(), 1, "the unrecorded pack");
    assert!(rig.index.has_none(&digests(&blobs)));
    rig.index.fail_record(0);
    assert_eq!(update(&rig.front, &blobs).await, [OK, OK]);
    let keys = rig.store.keys().await;
    assert_eq!(keys.len(), 2, "{keys:?}");
    for place in rig.index.where_is(&digests(&blobs)) {
        assert_eq!(place.expect("a row").pack_key, keys[1]);
    }
}

/// Asserts that each item of a batch is answered alone: a blob whose data is not its digest and
/// one with a compressor are refused INVALID_ARGUMENT, and the good blob beside them is stored
/// and in the index.
#[tokio::test]
async fn a_bad_item_of_a_batch_is_refused_alone() {
    let rig = Rig::new().await;
    let good = blob("good");
    let other = blob("another").0;
    let lying = item(&other, &Bytes::from_static(b"not that"));
    let mut compressed = item(&good.0, &good.1);
    compressed.compressor = reapi::compressor::Value::Zstd as i32;
    let requests = vec![item(&good.0, &good.1), lying, compressed];

    let answered = send_update(&rig.front, requests).await;
    assert_eq!(answered, [OK, INVALID, INVALID]);

    assert!(rig.index.has_all(&[good.0]));
    assert!(rig.index.has_none(&[other]));
}

/// Asserts that a blob the index has is not packed again: a second upload is acknowledged, adds
/// no pack, and is counted as skipped.
#[tokio::test]
async fn a_blob_the_index_has_is_not_packed_again() {
    let rig = Rig::new().await;
    let blobs = [blob("same")];

    assert_eq!(update(&rig.front, &blobs).await, [OK]);
    assert_eq!(update(&rig.front, &blobs).await, [OK]);

    assert_eq!(rig.store.keys().await.len(), 1);
    assert_eq!(rig.packs.metrics().skipped_blobs.get(), 1);
}

/// Asserts that FindMissingBlobs counts a blob held by either: one in a pack and one only on
/// the node are present, one in neither is missing; and the index counts what it was asked and
/// what it had.
#[tokio::test]
async fn find_missing_counts_a_blob_in_a_pack_or_on_a_node() {
    let rig = Rig::new().await;
    let packed = blob("in a pack");
    let (on_node, bytes) = blob("on the node");
    let neither = blob("in neither").0;
    assert_eq!(
        update(&rig.front, std::slice::from_ref(&packed)).await,
        [OK]
    );
    rig.node().put(bytes).await.expect("stored on the node");

    let absent = missing(&rig.front, &[packed.0, on_node, neither]).await;

    assert_eq!(absent.expect("answered"), [neither]);
    let metrics = rig.packs.metrics();
    assert_eq!(metrics.asked.get(), 3);
    assert_eq!(metrics.found.get(), 1);
}

/// Asserts that a FindMissingBlobs about blobs that are all in packs asks no storage node:
/// with the only node unreachable the call is answered, while a call about a blob the index
/// lacks reaches the node and fails UNAVAILABLE.
#[tokio::test]
async fn find_missing_of_packed_blobs_asks_no_node() {
    let rig = Rig::over(Owners::single(harness::down()), PackSettings::default()).await;
    let blobs = [blob("p"), blob("q")];
    assert_eq!(update(&rig.front, &blobs).await, [OK, OK]);

    let none = missing(&rig.front, &digests(&blobs)).await;
    assert_eq!(none.expect("answered"), Vec::<Digest>::new());

    let unknown = blob("unknown").0;
    let refused = missing(&rig.front, &[unknown]).await;
    assert_eq!(refused.expect_err("node down").code(), Code::Unavailable);
}

/// Asserts that BatchReadBlobs serves a blob from its pack, one only on the node from the
/// node, and answers a blob in neither NOT_FOUND, each item alone and in order; and counts the
/// two it asked the node for as fallbacks.
#[tokio::test]
async fn a_batch_read_serves_packs_then_nodes() {
    let rig = Rig::new().await;
    let packed = blob("read from a pack");
    let (on_node, node_bytes) = blob("read from the node");
    let neither = blob("read from nowhere").0;
    assert_eq!(
        update(&rig.front, std::slice::from_ref(&packed)).await,
        [OK]
    );
    let stored = rig.node().put(node_bytes.clone()).await;
    stored.expect("stored on the node");

    let got = read(&rig.front, &[neither, packed.0, on_node]).await;

    assert_eq!(got[0].0, NOT_FOUND);
    assert_eq!((got[1].0, got[1].1.as_slice()), (OK, &packed.1[..]));
    assert_eq!((got[2].0, got[2].1.as_slice()), (OK, &node_bytes[..]));
    let metrics = rig.packs.metrics();
    assert_eq!(metrics.reads.get(), 1);
    assert_eq!(metrics.read_bytes.get(), packed.1.len() as u64);
    assert_eq!(metrics.fallbacks.get(), 2);
}

/// Asserts that BatchReadBlobs reads the blobs the packs hold together: three blobs of one pack
/// are fetched with one ranged request of the store, and each is answered with its own bytes in
/// the order asked.
#[tokio::test]
async fn a_batch_read_of_blobs_in_one_pack_is_one_ranged_request() {
    let rig = Rig::new().await;
    let blobs = [
        blob("first of three"),
        blob("second of three"),
        blob("third of three"),
    ];
    assert_eq!(update(&rig.front, &blobs).await, [OK, OK, OK]);

    let got = read(&rig.front, &[blobs[2].0, blobs[0].0, blobs[1].0]).await;

    let order = [&blobs[2], &blobs[0], &blobs[1]];
    for (answer, (_, bytes)) in got.iter().zip(order) {
        assert_eq!((answer.0, answer.1.as_slice()), (OK, &bytes[..]));
    }
    let ranges = rig.store.ranges();
    assert_eq!(ranges.len(), 1, "{ranges:?}");
    assert_eq!(rig.packs.metrics().reads.get(), 3);
}

/// Asserts that an index that cannot answer fails the call and not the blob's absence:
/// FindMissingBlobs is UNAVAILABLE, and a BatchReadBlobs item is UNAVAILABLE and counted as a
/// read error.
#[tokio::test]
async fn an_index_that_cannot_answer_is_unavailable_not_absent() {
    let rig = Rig::new().await;
    let packed = blob("indexed");
    assert_eq!(
        update(&rig.front, std::slice::from_ref(&packed)).await,
        [OK]
    );

    rig.index.fail_find(1);
    let refused = missing(&rig.front, &[packed.0]).await;
    assert_eq!(refused.expect_err("index down").code(), Code::Unavailable);

    rig.index.fail_locate(1);
    let got = read(&rig.front, &[packed.0]).await;
    assert_eq!(got[0].0, UNAVAILABLE);
    assert_eq!(rig.packs.metrics().read_errors.get(), 1);
}

/// Asserts that a proxy given no packs (`with_packs(None)`) stores an upload on the node and
/// reads it from there, and prints without them.
#[tokio::test]
async fn a_proxy_without_packs_keeps_blobs_on_the_node() {
    let (storage, addr) = node();
    let owners = Owners::single(Owner::new("node", harness::channel(addr)));
    let authorizers = Arc::new(Authorizers::allow_all());
    let proxy = Proxy::new(owners, authorizers).with_packs(None);
    assert!(format!("{proxy:?}").contains("packs: None"));
    let front = harness::serve(proxy.routes()).await;
    let blobs = [blob("stays on the node")];

    assert_eq!(update(&front, &blobs).await, [OK]);

    let held = storage.find_missing(&[blobs[0].0]).await.expect("answered");
    assert!(held.is_empty());
    assert_eq!(read(&front, &[blobs[0].0]).await[0].0, OK);
}
