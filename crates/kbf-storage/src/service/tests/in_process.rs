//! A node called in its own process ([`InProcess`]): the same checks and the same write
//! body as a call over the wire, with no connection and no certificate.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use futures::StreamExt as _;
use kbf_meta::Role;
use kbf_proto::reapi::ActionResult;
use kbf_proto::worker::{
    BeginRequest, CommitRequest, LocateRequest, PutRangeRequest, ReadRangeRequest,
};
use tonic::{Code, Request};

use super::fake::{Held, Mem, patterned, sha};
use crate::replicate::MapSource;
use crate::service::{Admission, Config, InProcess, StorageService};
use crate::{Storage, StorageError};

/// A node over `mem` that judges every call by `held`, called in-process. Certified
/// admission is not asked for: a service in no chain admits whom it is told to, and the
/// in-process path admits no one (there is nobody to admit).
fn node(mem: &Arc<Mem>, held: &Arc<Held>) -> InProcess {
    let storage: Arc<dyn Storage> = Arc::clone(mem) as Arc<dyn Storage>;
    let service = StorageService::new(
        storage,
        Admission::Certified,
        Config {
            write_bytes_in_flight: 1 << 30,
        },
    )
    .with_map(Arc::clone(held) as Arc<dyn MapSource>);
    Arc::new(service).in_process()
}

fn is_stale<T>(answer: &Result<T, StorageError>, current: u64) -> bool {
    matches!(answer, Err(StorageError::StaleMap { current: c }) if *c == current)
}

/// Catches an in-process write that goes around the service: it must be judged by the map
/// generation the caller names, as a call over the wire is. A caller behind the node (an
/// older generation, or 0) gets STALE_MAP naming the node's generation and nothing is
/// stored; one ahead of it is UNAVAILABLE; one at it is stored, with the digest returned.
/// A blob the node holds is answered without a second store, and one of more than a
/// chunk (two messages) is stored whole.
///
/// Mutants: `put` calling the inner storage (no check: the stale write lands); the check
/// removed from the write body; the generation read from the wrong field (the current
/// write is refused).
#[tokio::test]
async fn a_write_is_judged_by_the_generation_the_caller_names_and_stored_once() {
    let mem = Arc::new(Mem::default());
    let held = Held::at(7);
    let node = node(&mem, &held);

    for behind in [5, 0] {
        let answer = node.at(behind).put(Bytes::from_static(b"payload")).await;
        assert!(is_stale(&answer, 7), "generation {behind}: {answer:?}");
    }
    let ahead = node.at(9).put(Bytes::from_static(b"payload")).await;
    assert!(
        matches!(ahead, Err(StorageError::Unavailable(_))),
        "{ahead:?}"
    );
    assert_eq!(
        mem.puts.load(Ordering::SeqCst),
        0,
        "stored under a stale map"
    );

    let current = node.at(7);
    let digest = current
        .put(Bytes::from_static(b"payload"))
        .await
        .expect("under 7");
    assert_eq!(digest, sha(b"payload"));
    assert_eq!(mem.puts.load(Ordering::SeqCst), 1);
    current
        .put(Bytes::from_static(b"payload"))
        .await
        .expect("held already");
    assert_eq!(
        mem.puts.load(Ordering::SeqCst),
        1,
        "a held blob is not stored again"
    );

    let large = patterned((1 << 20) + 5);
    let digest = current
        .put(Bytes::from(large.clone()))
        .await
        .expect("two messages");
    assert_eq!(digest, sha(&large));
    let stored = current.read_blob(&digest).await.expect("read back");
    assert_eq!(stored.as_ref(), large.as_slice());
}

/// Catches a store failure taken for a stale map or swallowed: the store's failure is the
/// write's, and reaches the caller as the store's error.
#[tokio::test]
async fn a_store_that_fails_fails_the_write() {
    let mem = Arc::new(Mem::default());
    let node = node(&mem, &Held::at(7));
    *mem.fail.lock().expect("lock") = Some(StorageError::Internal("disk".to_owned()));
    let answer = node.at(7).put(Bytes::from_static(b"payload")).await;
    assert!(
        matches!(&answer, Err(StorageError::Internal(why)) if why.contains("disk")),
        "{answer:?}"
    );
}

/// Catches an in-process read, find or action call that skips the generation check: a
/// reader on a stale map must be told so (it refetches the map and reads from the right
/// owner), not served from a node that no longer owns the blob. Every `Storage` call of a
/// caller behind the node is STALE_MAP, and the same calls at the node's generation are
/// the store's own answers.
///
/// Mutants: the check removed from any one call (that call answers under generation 3).
#[tokio::test]
async fn every_call_is_judged_by_the_generation_the_caller_names() {
    let mem = Arc::new(Mem::default());
    let held = Held::at(7);
    let node = node(&mem, &held);
    let digest = node
        .at(7)
        .put(Bytes::from_static(b"payload"))
        .await
        .expect("stored");
    let (stale, current) = (node.at(3), node.at(7));
    let action = sha(b"action");
    let result = ActionResult::default();

    assert!(is_stale(&stale.find_missing(&[digest]).await, 7));
    assert!(is_stale(&stale.read_blob(&digest).await, 7));
    assert!(is_stale(&stale.read_range(&digest, 0, 0).await, 7));
    assert!(is_stale(&stale.action_result(&action).await, 7));
    assert!(is_stale(&stale.prepare_action_result(&result).await, 7));
    let commit = stale
        .commit_action_record(
            Role::Daemon,
            action,
            kbf_meta::ActionRecord {
                result: digest,
                closure: Default::default(),
            },
        )
        .await;
    assert!(is_stale(&commit, 7), "{commit:?}");
    assert!(stale.stream(8).is_none());

    assert!(
        current
            .find_missing(&[digest])
            .await
            .expect("find")
            .is_empty()
    );
    assert_eq!(
        current.read_blob(&digest).await.expect("blob").as_ref(),
        b"payload"
    );
    let chunks: Vec<_> = current
        .read_range(&digest, 3, 0)
        .await
        .expect("range")
        .collect()
        .await;
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].as_ref().expect("chunk").as_ref(), b"load");
    assert!(current.action_result(&action).await.expect("get").is_none());
    let record = current
        .prepare_action_result(&result)
        .await
        .expect("prepare");
    current
        .commit_action_record(Role::Daemon, action, record)
        .await
        .expect("commit");
    assert!(current.action_result(&action).await.expect("get").is_some());
    assert!(current.stream(8).is_none());
}

/// Catches `Segments` handlers that skip the generation check, or that judge by the
/// request's header instead of the caller's generation: a caller behind the node is
/// FAILED_PRECONDITION on each of the five and the store's handler is not reached; at the
/// node's generation each reaches the store's handler.
#[tokio::test]
async fn the_segments_handlers_are_judged_too() {
    let mem = Arc::new(Mem::default());
    let node = node(&mem, &Held::at(7));
    let behind = node.at(3).segments();
    let at = node.at(7).segments();

    let refused = [
        behind
            .begin(Request::new(BeginRequest::default()))
            .await
            .err(),
        behind
            .put_range(Request::new(PutRangeRequest::default()))
            .await
            .err(),
        behind
            .commit(Request::new(CommitRequest::default()))
            .await
            .err(),
        behind
            .locate(Request::new(LocateRequest::default()))
            .await
            .err(),
        behind
            .read_range(Request::new(ReadRangeRequest::default()))
            .await
            .err(),
    ];
    for status in refused {
        assert_eq!(status.expect("refused").code(), Code::FailedPrecondition);
    }
    assert!(mem.segments.calls.lock().expect("lock").is_empty());

    at.begin(Request::new(BeginRequest::default()))
        .await
        .expect("begin");
    at.put_range(Request::new(PutRangeRequest::default()))
        .await
        .expect("put_range");
    at.commit(Request::new(CommitRequest::default()))
        .await
        .expect("commit");
    at.locate(Request::new(LocateRequest::default()))
        .await
        .expect("locate");
    at.read_range(Request::new(ReadRangeRequest::default()))
        .await
        .expect("read_range");
    let seen: Vec<_> = mem
        .segments
        .calls
        .lock()
        .expect("lock")
        .iter()
        .map(|(call, _)| *call)
        .collect();
    assert_eq!(
        seen,
        ["begin", "put_range", "commit", "locate", "read_range"]
    );
}
