//! Tests of [`Cache`] on the in-memory log and store.

use bytes::Bytes;
use kbf_meta::Role;
use kbf_objstore::ObjectStore;
use kbf_proto::reapi;
use tonic::{Code, Status};

use super::{Cache, Hold, VerifiedBlob};
use crate::meta_log::MetaLog;
use crate::wire;

fn blob(text: &'static str) -> VerifiedBlob {
    VerifiedBlob::hashed(Bytes::from_static(text.as_bytes()))
}

/// An `ActionResult` naming `blobs` as its output files.
fn result_of(blobs: &[&VerifiedBlob]) -> reapi::ActionResult {
    let mut result = reapi::ActionResult::default();
    for blob in blobs {
        result.output_files.push(reapi::OutputFile {
            path: "out".to_owned(),
            digest: Some(wire::digest_to_proto(&blob.digest())),
            ..reapi::OutputFile::default()
        });
    }
    result
}

/// Asserts that `present` answers for each digest by the places that can hold a blob: a
/// blob held in RAM, the empty blob and a blob in the index are present, and a blob
/// nobody has is not.
#[tokio::test]
async fn present_counts_held_empty_and_indexed_blobs() {
    let cache = Cache::memory().with_ram_ack_bytes(1024);
    let held = blob("held in RAM");
    let indexed = blob("in the index");
    cache
        .store_blobs(vec![indexed.clone()])
        .await
        .expect("stored");
    assert_eq!(cache.hold(&held), Hold::Held);
    let asked = [
        held.digest(),
        blob("").digest(),
        indexed.digest(),
        blob("nobody has it").digest(),
    ];
    let found = cache.present(&asked).await.expect("present");
    assert_eq!(found, [true, true, true, false]);
}

/// Asserts that a blob is complete when it is the empty blob, is held in RAM or is in
/// the index, and is not complete when nobody has it.
#[tokio::test]
async fn a_blob_is_complete_when_empty_held_or_indexed() {
    let cache = Cache::memory().with_ram_ack_bytes(1024);
    let held = blob("held in RAM");
    let indexed = blob("in the index");
    cache
        .store_blobs(vec![indexed.clone()])
        .await
        .expect("stored");
    assert_eq!(cache.hold(&held), Hold::Held);
    let asked = [
        blob("").digest(),
        held.digest(),
        indexed.digest(),
        blob("nobody has it").digest(),
    ];
    let mut complete = Vec::new();
    for digest in &asked {
        complete.push(cache.is_complete(digest).await.expect("asked"));
    }
    assert_eq!(complete, [true, true, true, false]);
}

/// Asserts that only the daemon writes the action cache: a client's write is refused as
/// PERMISSION_DENIED and leaves no entry, and the daemon's write is accepted and hit.
#[tokio::test]
async fn only_the_daemon_writes_an_action_result() {
    let cache = Cache::memory();
    let action = blob("an action").digest();
    let result = reapi::ActionResult::default();
    let refused = cache
        .write_action_result(Role::Client, action, &result)
        .await
        .expect_err("a client may not write");
    assert_eq!(Status::from(refused).code(), Code::PermissionDenied);
    let before = cache.action_result(&action).await.expect("lookup");
    assert!(before.is_none());
    cache
        .write_action_result(Role::Daemon, action, &result)
        .await
        .expect("the daemon writes");
    let after = cache.action_result(&action).await.expect("lookup");
    assert!(after.is_some());
}

/// Asserts that an entry naming an output nobody uploaded is refused as
/// FAILED_PRECONDITION, and that the same entry is accepted once the output is stored.
#[tokio::test]
async fn an_entry_is_written_only_once_every_output_is_held() {
    let cache = Cache::memory();
    let output = blob("an output");
    let action = blob("an action").digest();
    let result = result_of(&[&output]);
    let refused = cache
        .write_action_result(Role::Daemon, action, &result)
        .await
        .expect_err("the output is not held");
    assert_eq!(Status::from(refused).code(), Code::FailedPrecondition);
    cache.store_blobs(vec![output]).await.expect("stored");
    cache
        .write_action_result(Role::Daemon, action, &result)
        .await
        .expect("written once the output is held");
}

/// Asserts that an action-cache entry whose objects the store lost is a miss, not a hit:
/// the entry is a hit while the store holds the output's object and the result's, and a
/// miss once both are deleted from it, after which the output blob is reported missing.
#[tokio::test]
async fn an_entry_whose_objects_left_the_store_is_a_miss() {
    let cache = Cache::memory();
    let output = blob("an output");
    let wanted = output.digest();
    let action = blob("an action").digest();
    let result = result_of(&[&output]);
    cache.store_blobs(vec![output]).await.expect("stored");
    cache
        .write_action_result(Role::Daemon, action, &result)
        .await
        .expect("written");
    let hit = cache.action_result(&action).await.expect("lookup");
    assert!(hit.is_some());
    let objects = cache
        .meta()
        .query(move |s| s.stored_objects(&action))
        .await
        .expect("query");
    for object in objects {
        let key = cache.object_key(object).expect("key");
        cache.objects().delete(&key).await.expect("delete");
    }
    let miss = cache.action_result(&action).await.expect("lookup");
    assert!(miss.is_none());
    let gone = cache.find_missing(&[wanted]).await.expect("find");
    assert_eq!(gone, [wanted]);
}
