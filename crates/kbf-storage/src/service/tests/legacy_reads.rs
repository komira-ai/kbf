//! The reads a node serves from its own copy ([`crate::legacy`]): each one is counted once on
//! the row of the call that served it, and nothing that serves no stored bytes is counted.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures::StreamExt as _;
use kbf_meta::Role;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{LocateRequest, ObjectRef, ReadRangeRequest};
use tonic::Request;

use super::fake::{Held, Mem, connect, patterned, serve_routes, sha};
use crate::legacy::{LegacyReads, Path};
use crate::replicate::MapSource;
use crate::service::{Admission, Config, InProcess, RemoteStorage, StorageService};
use crate::{READ_CHUNK_BYTES, Storage, StorageError};

/// A plain-text service over `mem` counting into the returned handle, a client of it over a
/// connection, and the same service called in-process (judged by `held`, generation 7).
async fn node(mem: &Arc<Mem>) -> (RemoteStorage, InProcess, Arc<LegacyReads>) {
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
    (RemoteStorage::new(channel, 7), service.in_process(), reads)
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
    let (remote, _, reads) = node(&mem).await;
    let large = patterned(2 * READ_CHUNK_BYTES + READ_CHUNK_BYTES / 2);
    let digest = hold(&mem, &large);
    assert_eq!(reads.total(), 0, "nothing is counted before a read");

    drain(remote.read_range(&digest, 0, 0).await.expect("read")).await;
    assert_eq!(rows(&reads), [1, 0, 0, 0, 0]);
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
    let (remote, _, reads) = node(&mem).await;

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
    assert_eq!(rows(&reads), [0, 0, 0, 0, 0]);
}

/// Catches an action-cache lookup counted on a miss or on the wrong row: a hit is one read on
/// `ac/action_store`, a miss none, and the blob rows stay at 0.
#[tokio::test]
async fn an_action_store_get_counts_a_hit_and_not_a_miss() {
    let mem = Arc::new(Mem::default());
    let (remote, _, reads) = node(&mem).await;
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
    assert_eq!(rows(&reads), [0, 0, 0, 1, 0]);
}

/// Catches a segment read counted as one per blob or not at all: a `ReadRange` that returns
/// bytes is one read on `cas/segments`; `Locate` and a write to the node add none.
#[tokio::test]
async fn a_segments_read_range_counts_and_locate_does_not() {
    let mem = Arc::new(Mem::default());
    let (remote, _, reads) = node(&mem).await;

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
    assert_eq!(rows(&reads), [0, 1, 0, 0, 0]);
}

/// Catches a call that serves no stored bytes being counted: writes, FindMissingBlobs, an
/// action-cache write and a read-your-write of a blob the caller just put add nothing.
#[tokio::test]
async fn writes_and_existence_checks_are_not_reads() {
    let mem = Arc::new(Mem::default());
    let (remote, _, reads) = node(&mem).await;

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
    assert_eq!(rows(&reads), [0, 0, 0, 0, 0]);
}

/// Catches an in-process caller (the daemon half of a storage-and-daemon process) that reads
/// around the counter: a blob read whole, a ranged read, an action entry, an action result and
/// a segment range each count once on the in-process or segments row, and nothing is
/// counted on the wire rows.
#[tokio::test]
async fn an_in_process_read_counts_on_the_in_process_rows() {
    let mem = Arc::new(Mem::default());
    let (_, in_process, reads) = node(&mem).await;
    let node = in_process.at(7);
    let digest = hold(&mem, b"blob");
    let action = hold_action(&mem, b"action");

    node.read_blob(&digest).await.expect("blob");
    assert_eq!(rows(&reads), [0, 0, 1, 0, 0]);
    drain(node.read_range(&digest, 0, 0).await.expect("range")).await;
    assert_eq!(rows(&reads), [0, 0, 2, 0, 0]);

    assert!(node.action_entry(&action).await.expect("entry").is_some());
    assert_eq!(rows(&reads), [0, 0, 2, 0, 1]);
    assert!(node.action_result(&action).await.expect("result").is_some());
    assert_eq!(rows(&reads), [0, 0, 2, 0, 2], "one count for one result");

    let range = ReadRangeRequest {
        object: Some(ObjectRef::default()),
        offset: 0,
        length: 2,
    };
    node.segments()
        .read_range(Request::new(range))
        .await
        .expect("segment range");
    assert_eq!(rows(&reads), [0, 1, 2, 0, 2]);
}

/// Catches an in-process read counted when the node refused or missed it: a caller behind the
/// node's map generation (STALE_MAP), a blob and an action the node does not hold, and the
/// empty blob add nothing.
#[tokio::test]
async fn an_in_process_miss_or_stale_read_is_not_counted() {
    let mem = Arc::new(Mem::default());
    let (_, in_process, reads) = node(&mem).await;
    let digest = hold(&mem, b"blob");
    let action = hold_action(&mem, b"action");

    let stale = in_process.at(5);
    assert!(stale.read_blob(&digest).await.is_err());
    assert!(stale.read_range(&digest, 0, 0).await.is_err());
    assert!(stale.action_entry(&action).await.is_err());

    let node = in_process.at(7);
    assert!(node.read_blob(&sha(b"absent")).await.is_err());
    assert!(node.read_range(&sha(b"absent"), 0, 0).await.is_err());
    assert!(
        node.action_entry(&sha(b"none"))
            .await
            .expect("miss")
            .is_none()
    );
    node.read_blob(&sha(b"")).await.expect("empty");
    assert_eq!(rows(&reads), [0, 0, 0, 0, 0]);
}

/// Catches a daemon half of a node without a frontend that reads its storage directly and so
/// around the counter: through `CountedStorage` a blob read whole, a ranged read, an action
/// entry and an action result each count once on an `in_process` row; a miss, the empty
/// blob, a store failure, a write and an existence check count nothing; and every call
/// still returns what the inner storage returns.
#[tokio::test]
async fn a_direct_read_of_the_node_storage_counts_on_the_in_process_rows() {
    let mem = Arc::new(Mem::default());
    let reads = Arc::new(LegacyReads::default());
    let storage: Arc<dyn Storage> = Arc::clone(&mem) as Arc<dyn Storage>;
    let node = crate::legacy::CountedStorage::new(storage, Arc::clone(&reads));
    let digest = hold(&mem, b"blob");
    let action = hold_action(&mem, b"action");

    assert_eq!(node.read_blob(&digest).await.expect("blob"), &b"blob"[..]);
    assert_eq!(rows(&reads), [0, 0, 1, 0, 0]);
    drain(node.read_range(&digest, 0, 0).await.expect("range")).await;
    assert_eq!(rows(&reads), [0, 0, 2, 0, 0]);
    assert!(node.action_entry(&action).await.expect("entry").is_some());
    assert!(node.action_result(&action).await.expect("result").is_some());
    assert_eq!(rows(&reads), [0, 0, 2, 0, 2]);

    assert!(node.read_blob(&sha(b"absent")).await.is_err());
    assert!(node.read_range(&sha(b"absent"), 0, 0).await.is_err());
    assert!(
        node.action_entry(&sha(b"none"))
            .await
            .expect("miss")
            .is_none()
    );
    assert!(
        node.action_result(&sha(b"none"))
            .await
            .expect("miss")
            .is_none()
    );
    node.read_blob(&sha(b"")).await.expect("empty");
    node.put(Bytes::from_static(b"payload")).await.expect("put");
    node.find_missing(&[digest]).await.expect("asked");
    *mem.fail.lock().expect("lock") = Some(StorageError::Internal("disk".into()));
    assert!(node.read_blob(&digest).await.is_err());
    assert_eq!(rows(&reads), [0, 0, 2, 0, 2]);
}

/// Catches a `CountedStorage` that drops, alters or double-answers a call it only passes on:
/// each existence check, write, stream, segments handle, action-cache write and RAM-ack
/// query reaches the inner storage exactly once and returns its answer (a refusal included),
/// and none of them counts as a read.
#[tokio::test]
async fn every_other_call_reaches_the_inner_storage_and_counts_nothing() {
    let mem = Arc::new(Mem::default());
    let reads = Arc::new(LegacyReads::default());
    let storage: Arc<dyn Storage> = Arc::clone(&mem) as Arc<dyn Storage>;
    let node = crate::legacy::CountedStorage::new(storage, Arc::clone(&reads));
    let held = hold(&mem, b"held");

    node.find_durable(&[held]).await.expect("durable");
    node.find_durable_waiting(&[held]).await.expect("waited");
    assert_eq!(mem.durable_asked.load(Ordering::SeqCst), 1);
    assert_eq!(mem.waited.load(Ordering::SeqCst), 1);

    let unowned = node
        .put_unowned(Bytes::from_static(b"unowned"))
        .await
        .expect("put");
    assert_eq!(unowned, sha(b"unowned"));
    assert!(mem.blobs.lock().expect("lock").contains_key(&unowned));
    assert_eq!(mem.puts.load(Ordering::SeqCst), 1);

    assert!(node.stream(5).is_none(), "the inner storage offers none");
    mem.knobs.enabled.store(true, Ordering::SeqCst);
    assert!(node.stream(5).is_some(), "the inner storage's stream");
    assert_eq!(mem.knobs.begun.load(Ordering::SeqCst), 1);

    let streamed = sha(b"streamed");
    let pieces = futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"streamed"))]);
    let claim = Box::pin(async move { Some(streamed) });
    let done = node.put_streamed(8, pieces.boxed(), claim).await;
    assert_eq!(done.expect("streamed"), Some(streamed));
    assert_eq!(mem.streamed.load(Ordering::SeqCst), 1);

    let begun = node
        .segments()
        .begin(Request::new(kbf_proto::worker::BeginRequest::default()))
        .await;
    begun.expect("begin");
    let seen = mem.segments.calls.lock().expect("lock").len();
    assert_eq!(seen, 1, "the inner segments service took the call");
    let open = crate::service::OpenWrites::new();
    node.segments_counted(&open);
    assert!(mem.counted_with.lock().expect("lock").is_some());

    let result = ActionResult {
        stdout_raw: b"out".to_vec(),
        exit_code: 2,
        ..ActionResult::default()
    };
    let record = node.prepare_action_result(&result).await.expect("prepared");
    assert_eq!((record.result, record.closure.len()), (sha(b"out"), 2));
    let action = sha(b"an action");
    let refused = node
        .commit_action_record(Role::Client, action, record.clone())
        .await;
    assert!(refused.is_err(), "the inner storage's refusal is returned");
    assert!(mem.actions.lock().expect("lock").is_empty());
    node.commit_action_record(Role::Daemon, action, record)
        .await
        .expect("committed");
    assert!(mem.actions.lock().expect("lock").contains_key(&action));

    assert!(!node.ram_ack());
    mem.knobs.ram_ack.store(true, Ordering::SeqCst);
    assert!(node.ram_ack());
    node.unconfirmed(3);
    assert_eq!(mem.knobs.unconfirmed.load(Ordering::SeqCst), 3);

    assert_eq!(rows(&reads), [0, 0, 0, 0, 0], "none of these is a read");
}
