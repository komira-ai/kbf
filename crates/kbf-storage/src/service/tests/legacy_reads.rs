//! The reads a node serves from its own copy ([`crate::legacy`]): each one is counted once on
//! the row of the call that served it, and nothing that serves no stored bytes is counted.

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt as _;
use kbf_meta::Role;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{LocateRequest, ObjectRef, ReadRangeRequest};
use tonic::Request;

use super::fake::{Held, Mem, connect, patterned, serve_routes, sha};
use crate::legacy::{LegacyReads, Path};
use crate::replicate::MapSource;
use crate::service::{Admission, Config, RemoteStorage, StorageService};
use crate::{READ_CHUNK_BYTES, Storage, StorageError};

/// A plain-text service over `mem` counting into the returned handle, and a client of it over a
/// connection (judged by `held`, generation 7).
async fn node(mem: &Arc<Mem>) -> (RemoteStorage, Arc<LegacyReads>) {
    let reads = Arc::new(LegacyReads::default());
    let storage: Arc<dyn Storage> = Arc::clone(mem) as Arc<dyn Storage>;
    let service = StorageService::new(
        storage,
        Admission::Open,
        Config {
            write_bytes_in_flight: 1 << 30,
        },
    )
    .with_map(Held::at(7) as Arc<dyn MapSource>)
    .with_legacy_reads(Arc::clone(&reads));
    let service = Arc::new(service);
    let channel = connect(serve_routes(service.shared_routes(), None).await).await;
    (RemoteStorage::new(channel, 7), reads)
}

fn hold(mem: &Mem, bytes: &[u8]) -> kbf_types::Digest {
    let digest = sha(bytes);
    mem.blobs
        .lock()
        .expect("lock")
        .insert(digest, Bytes::copy_from_slice(bytes));
    digest
}

fn hold_action(mem: &Mem, name: &[u8]) -> kbf_types::Digest {
    let action = sha(name);
    mem.actions
        .lock()
        .expect("lock")
        .insert(action, (sha(b"result"), ActionResult::default()));
    action
}

async fn drain(mut stream: crate::ReadStream) {
    while let Some(chunk) = stream.next().await {
        chunk.expect("a chunk");
    }
}

fn rows(reads: &LegacyReads) -> Vec<u64> {
    Path::ALL.iter().map(|path| reads.get(*path)).collect()
}

/// Catches a blob read that is counted per chunk, per message or not at all, and one counted
/// on the wrong row: a blob of two and a half chunks read over ByteStream is one read on
/// `cas/bytestream` and nothing else, and a second read is a second count.
#[tokio::test]
async fn a_bytestream_read_of_a_held_blob_counts_once_on_its_own_row() {
    let mem = Arc::new(Mem::default());
    let (remote, reads) = node(&mem).await;
    let large = patterned(2 * READ_CHUNK_BYTES + READ_CHUNK_BYTES / 2);
    let digest = hold(&mem, &large);
    assert_eq!(reads.total(), 0, "nothing is counted before a read");

    drain(remote.read_range(&digest, 0, 0).await.expect("read")).await;
    assert_eq!(rows(&reads), [1, 0, 0]);
    assert_eq!(reads.get(Path::CasByteStream), 1);

    remote.read_blob(&digest).await.expect("read whole");
    assert_eq!(
        reads.get(Path::CasByteStream),
        2,
        "a second read is counted"
    );
    assert_eq!(reads.total(), 2);
}

/// Catches a read counted before it is known to have found its blob: a blob the node does not
/// hold, a read the store fails, an offset past the end and the empty blob (which no store
/// holds) add nothing.
#[tokio::test]
async fn a_read_that_finds_nothing_is_not_counted() {
    let mem = Arc::new(Mem::default());
    let (remote, reads) = node(&mem).await;

    let absent = sha(b"absent");
    assert!(matches!(
        remote.read_range(&absent, 0, 0).await,
        Err(StorageError::NotFound(_))
    ));
    assert!(matches!(
        remote.read_blob(&absent).await,
        Err(StorageError::NotFound(_))
    ));

    let held = hold(&mem, b"held");
    assert!(
        remote.read_range(&held, 99, 0).await.is_err(),
        "past the end"
    );

    let empty = sha(b"");
    drain(remote.read_range(&empty, 0, 0).await.expect("empty")).await;
    assert!(remote.read_blob(&empty).await.expect("empty").is_empty());

    *mem.fail.lock().expect("lock") = Some(StorageError::Internal("disk".into()));
    assert!(remote.read_range(&held, 0, 0).await.is_err());
    assert_eq!(rows(&reads), [0, 0, 0]);
}

/// Catches an action-cache lookup counted on a miss or on the wrong row: a hit is one read on
/// `ac/action_store`, a miss none, and the blob rows stay at 0.
#[tokio::test]
async fn an_action_store_get_counts_a_hit_and_not_a_miss() {
    let mem = Arc::new(Mem::default());
    let (remote, reads) = node(&mem).await;
    let action = hold_action(&mem, b"action");

    assert!(
        remote
            .action_entry(&sha(b"other"))
            .await
            .expect("miss")
            .is_none()
    );
    assert_eq!(reads.total(), 0, "a miss");

    assert!(remote.action_entry(&action).await.expect("hit").is_some());
    assert_eq!(rows(&reads), [0, 0, 1]);
}

/// Catches a segment read counted as one per blob or not at all: a `ReadRange` that returns
/// bytes is one read on `cas/segments`; `Locate` and a write to the node add none.
#[tokio::test]
async fn a_segments_read_range_counts_and_locate_does_not() {
    let mem = Arc::new(Mem::default());
    let (remote, reads) = node(&mem).await;

    // Named by its trait, which is not imported: `RemoteStorage` is a `Storage` and a
    // `Segments`, and both have a `read_range`.
    let locate = kbf_proto::worker::segments_server::Segments::locate;
    let located = locate(&remote, Request::new(LocateRequest::default()));
    located.await.expect("locate");
    assert_eq!(reads.total(), 0, "locate");

    let range = ReadRangeRequest {
        object: Some(ObjectRef::default()),
        offset: 0,
        length: 2,
    };
    let read_range = kbf_proto::worker::segments_server::Segments::read_range;
    let read = read_range(&remote, Request::new(range));
    read.await.expect("read range");
    assert_eq!(rows(&reads), [0, 1, 0]);
}

/// Catches a call that serves no stored bytes being counted: writes, FindMissingBlobs, an
/// action-cache write and a read-your-write of a blob the caller just put add nothing.
#[tokio::test]
async fn writes_and_existence_checks_are_not_reads() {
    let mem = Arc::new(Mem::default());
    let (remote, reads) = node(&mem).await;

    let digest = remote
        .put(Bytes::from_static(b"payload"))
        .await
        .expect("put");
    assert_eq!(digest, sha(b"payload"));
    assert!(
        remote
            .find_missing(&[digest])
            .await
            .expect("held")
            .is_empty()
    );
    assert_eq!(remote.find_missing(&[sha(b"x")]).await.expect("x").len(), 1);
    let record = remote
        .prepare_action_result(&ActionResult::default())
        .await
        .expect("prepared");
    remote
        .commit_action_record(Role::Daemon, sha(b"action"), record)
        .await
        .expect("committed");
    assert_eq!(rows(&reads), [0, 0, 0]);
}
