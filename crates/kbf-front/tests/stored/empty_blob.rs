//! The empty blob in a frontend with no storage node. A client may leave it out of its uploads
//! (Buck2 never uploads it, and REAPI allows that), so every call that asks whether a blob is
//! held, or reads one, answers for it without a row in the pack index.
//!
//! Every test says the behaviour it asserts.

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use kbf_auth::Authorizers;
use kbf_front::{Dispatch, ExecutionService, Stage, Submission, Ticket};
use kbf_proto::reapi::execution_server::Execution as _;
use kbf_proto::reapi::{Action, Command, Directory, DirectoryNode, ExecuteRequest, FileNode};
use kbf_types::Digest;
use prost::Message as _;
use tokio::sync::watch;
use tonic::{Code, Status};

use crate::bytestream::{read, read_name, status, write};
use crate::cas::{item, missing, read as batch_read, update};
use crate::rig::{Rig, blob_of};

/// A dispatch that queues every submission and never runs it.
#[derive(Default)]
struct Queue {
    submitted: Mutex<Vec<watch::Sender<Stage>>>,
}

impl Dispatch for Queue {
    fn submit(&self, submission: Submission) -> Result<Ticket, Status> {
        let (tx, stage) = watch::channel(Stage::Queued);
        let mut held = self
            .submitted
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let name = format!("operations/{}", held.len());
        held.push(tx);
        let key = kbf_types::ActionKey::try_from(&submission.request.key).expect("an action's key");
        Ok(Ticket {
            name,
            instance: key.instance,
            action: key.action,
            stage,
        })
    }

    fn wait(&self, _name: &str) -> Option<Ticket> {
        None
    }
}

fn proto(digest: &Digest) -> kbf_proto::reapi::Digest {
    kbf_proto::reapi::Digest {
        hash: digest.hash_hex(),
        size_bytes: digest.size_bytes as i64,
    }
}

fn file(name: &str, digest: &Digest) -> FileNode {
    FileNode {
        name: name.to_owned(),
        digest: Some(proto(digest)),
        ..FileNode::default()
    }
}

/// Asserts that an Execute whose input tree names an empty file that was never uploaded (the
/// root and a subdirectory each name one, as a toolchain unpack does) is taken and queued, not
/// refused as "1 input blob(s) are not in the CAS"; and that a blob that is not empty and was
/// not uploaded is still refused alone. Catches the input walk asking the pack index for the
/// empty blob, which has no row.
#[tokio::test]
async fn an_execute_naming_an_empty_file_that_was_never_uploaded_is_taken() {
    let rig = Rig::new().await;
    let (empty, _) = blob_of(b"");
    let (data, data_bytes) = blob_of(b"data");
    let (lost, _) = blob_of(b"never uploaded");
    let sub = Directory {
        files: vec![file("empty", &empty)],
        ..Directory::default()
    };
    let (sub_digest, sub_bytes) = blob_of(&sub.encode_to_vec());
    let root = Directory {
        files: vec![file("empty", &empty), file("data", &data)],
        directories: vec![DirectoryNode {
            name: "sub".to_owned(),
            digest: Some(proto(&sub_digest)),
        }],
        ..Directory::default()
    };
    let (root_digest, root_bytes) = blob_of(&root.encode_to_vec());
    let command = Command {
        arguments: vec!["true".to_owned()],
        ..Command::default()
    };
    let (command_digest, command_bytes) = blob_of(&command.encode_to_vec());
    let action = Action {
        command_digest: Some(proto(&command_digest)),
        input_root_digest: Some(proto(&root_digest)),
        ..Action::default()
    };
    let (action_digest, action_bytes) = blob_of(&action.encode_to_vec());
    update(
        &rig,
        vec![
            item(&action_digest, &action_bytes),
            item(&command_digest, &command_bytes),
            item(&root_digest, &root_bytes),
            item(&sub_digest, &sub_bytes),
            item(&data, &data_bytes),
        ],
    )
    .await
    .expect("uploaded");
    let (_, closing) = kbf_front::closing();
    let queue = Arc::new(Queue::default());
    let service = ExecutionService::with_authorizers(
        rig.backend.clone(),
        Arc::clone(&queue),
        closing,
        Arc::new(Authorizers::allow_all()),
    );
    let execute = |digest: &Digest| {
        service.execute(tonic::Request::new(ExecuteRequest {
            instance_name: "main".to_owned(),
            action_digest: Some(proto(digest)),
            ..ExecuteRequest::default()
        }))
    };

    let taken = execute(&action_digest).await;

    assert!(taken.is_ok(), "the Execute was refused");
    assert_eq!(queue.submitted.lock().expect("lock").len(), 1);

    // The same tree with one more file that is neither empty nor held.
    let worse = Directory {
        files: vec![file("empty", &empty), file("lost", &lost)],
        ..Directory::default()
    };
    let (worse_digest, worse_bytes) = blob_of(&worse.encode_to_vec());
    let action = Action {
        command_digest: Some(proto(&command_digest)),
        input_root_digest: Some(proto(&worse_digest)),
        ..Action::default()
    };
    let (worse_action, worse_action_bytes) = blob_of(&action.encode_to_vec());
    update(
        &rig,
        vec![
            item(&worse_action, &worse_action_bytes),
            item(&worse_digest, &worse_bytes),
        ],
    )
    .await
    .expect("uploaded");

    let Err(refused) = execute(&worse_action).await else {
        panic!("an Execute with a lost input was taken");
    };

    assert_eq!(refused.code(), Code::FailedPrecondition);
    assert_eq!(queue.submitted.lock().expect("lock").len(), 1);
}

/// Asserts that FindMissingBlobs never lists the empty blob, with or without an upload, and
/// still lists a blob that is neither empty nor held; and that the backend's own check (the
/// one Execute and the action cache use) agrees.
#[tokio::test]
async fn find_missing_holds_the_empty_blob_without_an_upload() {
    let rig = Rig::new().await;
    let (empty, _) = blob_of(b"");
    let (never, _) = blob_of(b"never");

    let before = missing(&rig, &[never, empty]).await.expect("answered");
    let backend = rig.backend.find_missing(&[empty, never]).await;
    update(&rig, vec![item(&empty, &Bytes::new())])
        .await
        .expect("an upload of the empty blob is taken");
    let after = missing(&rig, &[empty, never]).await.expect("answered");

    assert_eq!(before, vec![never]);
    assert_eq!(backend.expect("answered"), vec![never]);
    assert_eq!(after, vec![never]);
}

/// Asserts that a batch read answers the empty blob as OK with no bytes though it was never
/// uploaded, and the others as before; and that the backend reads it as no bytes, alone and in
/// a batch.
#[tokio::test]
async fn the_empty_blob_reads_as_no_bytes() {
    let rig = Rig::new().await;
    let (empty, _) = blob_of(b"");
    let (held, held_bytes) = blob_of(b"held");
    update(&rig, vec![item(&held, &held_bytes)])
        .await
        .expect("stored");

    let batch = batch_read(&rig, &[empty, held]).await.expect("answered");
    let one = rig.backend.read_blob(&empty).await;
    let many = rig.backend.read_many(&[held, empty]).await;

    assert_eq!(batch, vec![(0, Vec::new()), (0, held_bytes.to_vec())]);
    assert_eq!(one.expect("read"), Bytes::new());
    assert_eq!(many[0].as_ref().expect("read"), &held_bytes);
    assert_eq!(many[1].as_ref().expect("read"), &Bytes::new());
}

/// Asserts that a ByteStream read of the empty blob is no bytes and not NOT_FOUND, that
/// QueryWriteStatus says it is complete, and that a Write of it is answered at once as for any
/// blob that is held.
#[tokio::test]
async fn bytestream_holds_the_empty_blob() {
    let rig = Rig::new().await;
    let (empty, _) = blob_of(b"");

    let bytes = read(&rig, &read_name(&empty), 0, 0).await;
    let state = status(&rig, &crate::bytestream::upload_name(&empty)).await;
    let written = write(&rig, crate::bytestream::pieces(&empty, b"", 8)).await;

    assert_eq!(bytes.expect("read"), Vec::<u8>::new());
    assert_eq!(state.expect("status"), (0, true));
    assert_eq!(written.expect("written"), 0);
}
