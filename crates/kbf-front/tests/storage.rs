//! The cache through the storage boundary: [`CacheStorage`] as an `Arc<dyn Storage>`.
//! The data plane's checks run over it in `kbf-all-in-one` (`tests/storage_seam.rs`);
//! these cover what they do not: the action cache, and that every error answers the
//! gRPC code and message the cache's own error answers.

use std::sync::Arc;

use bytes::Bytes;
use kbf_front::{Cache, CacheError, CacheStorage, MetaLogError, SegmentsConfig};
use kbf_meta::{ActionWriteError, Role};
use kbf_objstore::ObjectStoreError;
use kbf_proto::reapi::{ActionResult, OutputFile};
use kbf_storage::{Storage, StorageError};
use kbf_types::Digest;
use tonic::Status;

fn storage() -> Arc<dyn Storage> {
    Arc::new(CacheStorage::new(
        Arc::new(Cache::memory()),
        SegmentsConfig::DEFAULT,
    ))
}

fn digest_of(bytes: &[u8]) -> Digest {
    kbf_segments::sha256(bytes)
}

fn result_with(output: Digest) -> ActionResult {
    ActionResult {
        output_files: vec![OutputFile {
            path: "out".to_owned(),
            digest: Some(kbf_front::digest_to_proto(&output)),
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Catches a call the adapter answers otherwise than the cache: a put that answers
/// another digest than the bytes', a FindMissing out of order or naming a held blob, an
/// absent blob read as anything but NOT_FOUND.
#[tokio::test]
async fn blobs_answer_through_the_trait() {
    let storage = storage();
    let bytes = Bytes::from_static(b"an output");
    let held = storage.put(bytes.clone()).await.expect("put");
    assert_eq!(held, digest_of(&bytes));
    let absent = digest_of(b"never stored");
    let asked = [absent, held];
    assert_eq!(storage.find_missing(&asked).await.expect("find"), [absent]);
    assert_eq!(storage.read_blob(&held).await.expect("read"), bytes);
    assert!(
        matches!(storage.read_blob(&absent).await, Err(StorageError::NotFound(d)) if d == absent)
    );
}

/// Catches an action-cache entry written by a client, an entry prepared over an output
/// that is not held, and a committed entry not served back as it was written.
#[tokio::test]
async fn the_action_cache_answers_through_the_trait() {
    let storage = storage();
    let action = digest_of(b"an action");
    let output = storage
        .put(Bytes::from_static(b"its output"))
        .await
        .expect("put");
    assert!(storage.action_result(&action).await.expect("get").is_none());

    let absent = digest_of(b"an output never stored");
    let refused = storage.prepare_action_result(&result_with(absent)).await;
    assert!(
        matches!(refused, Err(StorageError::ActionWrite(ActionWriteError::Absent(d))) if d == absent),
        "{refused:?}"
    );

    let result = result_with(output);
    let record = storage
        .prepare_action_result(&result)
        .await
        .expect("prepare");
    let as_client = storage
        .commit_action_record(Role::Client, action, record.clone())
        .await;
    assert!(
        matches!(
            as_client,
            Err(StorageError::ActionWrite(ActionWriteError::NotDaemon))
        ),
        "{as_client:?}"
    );
    assert!(storage.action_result(&action).await.expect("get").is_none());

    let stored = record.result;
    storage
        .commit_action_record(Role::Daemon, action, record)
        .await
        .expect("commit");
    assert_eq!(
        storage.action_result(&action).await.expect("get"),
        Some((stored, result))
    );
}

/// Catches the adapter offering a stream the cache cannot take: today a caller stores
/// a blob whole.
#[test]
fn the_cache_offers_no_stream_yet() {
    assert!(storage().stream(1).is_none());
}

/// Catches an error that answers a caller differently through the boundary than the
/// cache's own error did: another gRPC code (a store that could not answer taken as an
/// absent blob, a refused write retried) or another message.
#[test]
fn errors_answer_as_the_cache_answers() {
    let (a, b) = (digest_of(b"a"), digest_of(b"b"));
    let errors = [
        CacheError::NotFound(a),
        CacheError::Unreachable(a),
        CacheError::DigestMismatch {
            claimed: a,
            actual: b,
        },
        CacheError::Invalid("bad".to_owned()),
        CacheError::ActionWrite(ActionWriteError::NotDaemon),
        CacheError::ActionWrite(ActionWriteError::Absent(a)),
        CacheError::ActionWrite(ActionWriteError::Unreachable(a)),
        CacheError::Meta(MetaLogError::Unavailable("no leader".to_owned())),
        CacheError::Store(ObjectStoreError::Unsupported("retention")),
        CacheError::Internal("torn".to_owned()),
    ];
    for error in errors {
        let cache = Status::from(error.clone());
        let storage = Status::from(StorageError::from(error));
        assert_eq!(storage.code(), cache.code(), "{}", cache.message());
        assert_eq!(storage.message(), cache.message());
    }
}
