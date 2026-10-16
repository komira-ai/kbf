//! The S3 store's streamed writes against the loopback S3 service of `s3::fake`: a large
//! object goes as a multipart upload of bounded parts, and every failure aborts the
//! upload, so the service is left with no incomplete one.

use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::stream;
use kbf_objstore::s3::fake::FakeS3;
use kbf_objstore::s3::{MULTIPART_THRESHOLD, PART_BYTES};
use kbf_objstore::{BodyStream, ByteRange, Capabilities, ObjectKey, ObjectStore, ObjectStoreError};

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

/// Pieces of a size that does not divide the part size.
const PIECE: usize = (1 << 20) + 7;

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(format!("cas/{name}")).unwrap()
}

/// `len` bytes that differ from their neighbours, so a part sent twice, dropped or out
/// of order reads back wrong.
fn pattern(len: usize) -> Bytes {
    (0..len)
        .map(|i| (i % 251) as u8 ^ (i >> 16) as u8)
        .collect::<Vec<u8>>()
        .into()
}

/// `bytes` in [`PIECE`]-byte pieces, then `tail` if given.
fn body(bytes: &Bytes, tail: Option<ObjectStoreError>) -> BodyStream {
    let mut pieces: Vec<Result<Bytes, ObjectStoreError>> = (0..bytes.len())
        .step_by(PIECE)
        .map(|at| Ok(bytes.slice(at..(at + PIECE).min(bytes.len()))))
        .collect();
    pieces.extend(tail.map(Err));
    stream::iter(pieces).boxed()
}

/// Catches: a large object sent as one PUT (S3 refuses one over 5 GiB, and the whole
/// object is held to send it), parts larger than the part size (memory not bounded by a
/// part), parts cut where the body's pieces end, and parts completed out of order or
/// with bytes lost. Two parts and a short third, read back whole.
#[tokio::test]
async fn a_large_object_goes_in_parts_of_the_part_size_and_reads_back() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let bytes = pattern(2 * PART_BYTES as usize + 1000);
    let k = key("large");
    store
        .put_stream(&k, bytes.len() as u64, body(&bytes, None))
        .await
        .expect("streamed");
    let stats = s3.stats();
    assert_eq!(
        (stats.single_puts, stats.parts, stats.completed),
        (0, 3, 1),
        "{stats:?}"
    );
    assert!(
        stats.largest_body <= PART_BYTES,
        "a request carried {} bytes",
        stats.largest_body
    );
    assert_eq!(s3.incomplete_uploads(), 0);
    let all = ByteRange::new(0, bytes.len() as u64).unwrap();
    assert_eq!(store.get_range(&k, all).await.unwrap(), bytes);
    let requests = s3.requests();
    let order: Vec<&str> = requests
        .iter()
        .map(|r| r.split('?').next().unwrap_or_default())
        .collect();
    assert_eq!(
        order,
        [
            "POST /kbf-fake/cas/large",
            "PUT /kbf-fake/cas/large",
            "PUT /kbf-fake/cas/large",
            "PUT /kbf-fake/cas/large",
            "POST /kbf-fake/cas/large",
            "GET /kbf-fake/cas/large",
        ]
    );
    assert!(requests[1].contains("partNumber=1"), "{requests:?}");
    assert!(requests[3].contains("partNumber=3"), "{requests:?}");
}

/// Catches: the threshold off by one either way: an object of exactly
/// [`MULTIPART_THRESHOLD`] bytes is one PUT, one byte more is a multipart upload.
#[tokio::test]
async fn the_threshold_is_the_largest_single_put() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let at = pattern(MULTIPART_THRESHOLD as usize);
    store
        .put_stream(&key("at"), at.len() as u64, body(&at, None))
        .await
        .unwrap();
    assert_eq!((s3.stats().single_puts, s3.stats().parts), (1, 0));
    let over = pattern(MULTIPART_THRESHOLD as usize + 1);
    store
        .put_stream(&key("over"), over.len() as u64, body(&over, None))
        .await
        .unwrap();
    assert_eq!((s3.stats().single_puts, s3.stats().parts), (1, 2));
    assert_eq!(s3.object_len(&key("over")), Some(over.len() as u64));
}

/// Catches: a failed part that leaves its upload behind (an incomplete upload holds
/// storage until a lifecycle rule removes it), or a write answered as stored.
#[tokio::test]
async fn a_failed_part_aborts_the_upload() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    s3.fail_part(2);
    let bytes = pattern(3 * PART_BYTES as usize);
    let k = key("part-fails");
    let wrote = store
        .put_stream(&k, bytes.len() as u64, body(&bytes, None))
        .await;
    assert!(
        matches!(wrote, Err(ObjectStoreError::Service { status: 500, .. })),
        "{wrote:?}"
    );
    assert_eq!(s3.incomplete_uploads(), 0, "the upload was left behind");
    assert_eq!(s3.stats().aborted, 1);
    assert_eq!(s3.object_len(&k), None);
}

/// Catches: a body that failed, or ended short or long, completed as if whole, or its
/// upload left behind.
#[tokio::test]
async fn a_broken_body_aborts_the_upload() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let bytes = pattern(2 * PART_BYTES as usize + 5);
    let len = bytes.len() as u64;
    let broke = ObjectStoreError::Body("the source broke".into());
    let cases = [
        ("failed", len, body(&bytes, Some(broke))),
        ("short", len + 1, body(&bytes, None)),
        ("long", len - 1, body(&bytes, None)),
    ];
    for (name, declared, piece) in cases {
        let k = key(name);
        let wrote = store.put_stream(&k, declared, piece).await;
        assert!(
            matches!(wrote, Err(ObjectStoreError::Body(_))),
            "{name}: {wrote:?}"
        );
        assert_eq!(s3.incomplete_uploads(), 0, "{name}: left behind");
        assert_eq!(s3.object_len(&k), None, "{name}");
    }
    assert_eq!(s3.stats().completed, 0);
    assert_eq!(s3.stats().aborted, 3);
}

/// Catches: a completion answered 200 with an `<Error>` body taken for success (an
/// object reported written that is not there), and its upload left behind.
#[tokio::test]
async fn a_completion_that_fails_late_aborts_the_upload() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    s3.fail_complete();
    let bytes = pattern(PART_BYTES as usize + 1);
    let k = key("late");
    let wrote = store
        .put_stream(&k, bytes.len() as u64, body(&bytes, None))
        .await;
    assert!(
        matches!(&wrote, Err(ObjectStoreError::Service { status: 200, code, .. }) if code == "InternalError"),
        "{wrote:?}"
    );
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(s3.object_len(&k), None);
}

/// Catches: a multipart write that overwrites an existing key on a store claiming
/// conditional writes (the flusher relies on `AlreadyExists` to find a conflict), or
/// that leaves its refused upload behind.
#[tokio::test]
async fn a_conditional_completion_over_an_existing_key_is_refused_and_aborted() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("taken");
    store
        .put_new(&k, Bytes::from_static(b"first"), None)
        .await
        .unwrap();
    let bytes = pattern(PART_BYTES as usize + 1);
    let wrote = store
        .put_stream(&k, bytes.len() as u64, body(&bytes, None))
        .await;
    assert!(
        matches!(wrote, Err(ObjectStoreError::AlreadyExists(_))),
        "{wrote:?}"
    );
    assert_eq!(s3.incomplete_uploads(), 0);
    assert_eq!(s3.object_len(&k), Some(5));
    let condition = s3.requests();
    assert!(
        condition
            .iter()
            .any(|r| r.starts_with("POST /kbf-fake/cas/taken?uploadId=")),
        "{condition:?}"
    );
}

/// Catches: a write dropped mid-upload (its caller went away: a client's stream cut, a
/// flush cancelled at shutdown) that leaves its upload behind.
#[tokio::test]
async fn a_write_dropped_mid_upload_aborts_it() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let first = pattern(PART_BYTES as usize + 3);
    // One part's worth and more, then a body that never ends.
    let pending = stream::iter(vec![Ok(first)]).chain(stream::pending());
    let k = key("dropped");
    let wrote = tokio::time::timeout(
        Duration::from_millis(500),
        store.put_stream(&k, 4 * PART_BYTES, pending.boxed()),
    )
    .await;
    assert!(wrote.is_err(), "the write cannot finish");
    assert_eq!(s3.stats().parts, 1, "the first part was sent");
    for _ in 0..100 {
        if s3.incomplete_uploads() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the dropped write's upload was not aborted");
}
