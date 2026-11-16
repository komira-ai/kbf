//! The action cache of a frontend with no storage node: a row of the state store per action,
//! served only while the blobs its result names are held.

use bytes::Bytes;
use kbf_pack_index::action::ActionStore as _;
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::{
    self, ActionResult, Directory, FileNode, GetActionResultRequest, OutputDirectory, OutputFile,
    Tree, UpdateActionResultRequest,
};
use kbf_storage::StorageError;
use kbf_types::Digest;
use prost::Message as _;
use tonic::{Code, Status};

use crate::bytestream::{pieces, write};
use crate::rig::{Rig, blob_len, blob_of, index_form};

fn proto(digest: &Digest) -> reapi::Digest {
    reapi::Digest {
        hash: digest.hash_hex(),
        size_bytes: digest.size_bytes as i64,
    }
}

/// The result a client gets for `action`.
async fn get(rig: &Rig, action: &Digest) -> Result<ActionResult, Status> {
    let got = ActionCacheClient::new(rig.front.clone())
        .get_action_result(GetActionResultRequest {
            action_digest: Some(proto(action)),
            ..GetActionResultRequest::default()
        })
        .await?;
    Ok(got.into_inner())
}

/// A result whose one output file is `output`.
fn names(output: &Digest) -> ActionResult {
    ActionResult {
        output_files: vec![OutputFile {
            path: "out".to_owned(),
            digest: Some(proto(output)),
            ..OutputFile::default()
        }],
        stdout_digest: None,
        exit_code: 0,
        ..ActionResult::default()
    }
}

/// Stores the blob `bytes` as a client uploads one.
async fn upload(rig: &Rig, digest: &Digest, bytes: &Bytes) {
    write(rig, pieces(digest, bytes, 700))
        .await
        .expect("uploaded");
}

/// Asserts that a cold cache answers NOT_FOUND, and that a result written for an action is
/// served as it was written, and that a later write for the action replaces it.
#[tokio::test]
async fn a_result_is_served_as_written_and_a_later_one_replaces_it() {
    let rig = Rig::new().await;
    let (action, _) = blob_of(b"an action");
    let (first, first_bytes) = blob_len(100, 1);
    let (second, second_bytes) = blob_len(100, 2);
    upload(&rig, &first, &first_bytes).await;
    upload(&rig, &second, &second_bytes).await;
    let cold = get(&rig, &action).await.expect_err("cold");

    rig.stored
        .put_action_result(&action, &names(&first))
        .await
        .expect("written");
    let served = get(&rig, &action).await.expect("served");
    rig.stored
        .put_action_result(&action, &names(&second))
        .await
        .expect("replaced");
    let replaced = get(&rig, &action).await.expect("served");

    assert_eq!(cold.code(), Code::NotFound);
    assert_eq!(served, names(&first));
    assert_eq!(replaced, names(&second));
}

/// Asserts that a result is refused unless every blob it names is held, naming the blob that is
/// not, and is not served after the refusal.
#[tokio::test]
async fn a_result_naming_a_blob_that_is_not_held_is_refused() {
    let rig = Rig::new().await;
    let (action, _) = blob_of(b"an action");
    let (absent, _) = blob_of(b"never uploaded");

    let refused = rig
        .stored
        .put_action_result(&action, &names(&absent))
        .await
        .expect_err("not held");

    let StorageError::ActionWrite(kbf_meta::ActionWriteError::Absent(named)) = &refused else {
        panic!("expected the blobs that are absent, got {refused:?}");
    };
    assert_eq!(named, &vec![absent]);
    assert_eq!(
        get(&rig, &action).await.expect_err("nothing").code(),
        Code::NotFound
    );
    assert_eq!(Status::from(refused).code(), Code::FailedPrecondition);
}

/// Asserts that a result is a hit only while its blobs are held: once the index drops the
/// location of one, the entry is a miss (the client runs the action again).
#[tokio::test]
async fn a_result_whose_blob_is_lost_is_a_miss() {
    let rig = Rig::new().await;
    let (action, _) = blob_of(b"an action");
    let (output, bytes) = blob_len(100, 3);
    upload(&rig, &output, &bytes).await;
    rig.stored
        .put_action_result(&action, &names(&output))
        .await
        .expect("written");
    let location = rig.index.where_is(&[output]).remove(0).expect("recorded");
    assert!(get(&rig, &action).await.is_ok());

    use kbf_pack_index::PackIndex as _;
    rig.index
        .drop_location(&index_form(&output), &location.pack_key)
        .expect("dropped");

    assert_eq!(
        get(&rig, &action).await.expect_err("lost").code(),
        Code::NotFound
    );
}

/// Asserts that the closure of a result includes the files of its output directory trees: a
/// result whose tree names a file that is not held is refused, and one whose every file is held
/// is stored; and that a tree that is not held is refused too. A large output is held when it
/// is recorded as its object.
#[tokio::test]
async fn the_files_of_an_output_tree_are_part_of_the_result() {
    let rig = Rig::new().await;
    let (action, _) = blob_of(b"an action");
    let (small, small_bytes) = blob_len(100, 4);
    let (large, large_bytes) = blob_len(5000, 5);
    let (lost, _) = blob_len(100, 6);
    upload(&rig, &small, &small_bytes).await;
    upload(&rig, &large, &large_bytes).await;
    let tree_of = |files: &[(&str, &Digest)]| Tree {
        root: Some(Directory {
            files: files
                .iter()
                .map(|(name, digest)| FileNode {
                    name: (*name).to_owned(),
                    digest: Some(proto(digest)),
                    ..FileNode::default()
                })
                .collect(),
            ..Directory::default()
        }),
        children: Vec::new(),
    };
    let result_of = |tree: &Digest| ActionResult {
        output_directories: vec![OutputDirectory {
            path: "dir".to_owned(),
            tree_digest: Some(proto(tree)),
            ..OutputDirectory::default()
        }],
        ..ActionResult::default()
    };
    let (good_tree, good_bytes) =
        blob_of(&tree_of(&[("a", &small), ("b", &large)]).encode_to_vec());
    let (bad_tree, bad_bytes) = blob_of(&tree_of(&[("a", &small), ("c", &lost)]).encode_to_vec());
    let (unheld_tree, _) = blob_of(b"a tree nobody uploaded");
    upload(&rig, &good_tree, &good_bytes).await;
    upload(&rig, &bad_tree, &bad_bytes).await;

    let good = rig
        .stored
        .put_action_result(&action, &result_of(&good_tree))
        .await;
    let bad = rig
        .stored
        .put_action_result(&action, &result_of(&bad_tree))
        .await;
    let unheld = rig
        .stored
        .put_action_result(&action, &result_of(&unheld_tree))
        .await;

    good.expect("every file is held");
    let StorageError::ActionWrite(kbf_meta::ActionWriteError::Absent(named)) =
        bad.expect_err("a file is lost")
    else {
        panic!("expected absent blobs");
    };
    assert_eq!(named, vec![lost]);
    assert!(unheld.is_err(), "a tree that is not held is refused");
}

/// Asserts that a row of the state store that does not decode is INTERNAL for the client, not a
/// miss (a miss would hide the corruption) and not a panic.
#[tokio::test]
async fn a_row_that_does_not_decode_is_an_error() {
    let rig = Rig::new().await;
    let (action, _) = blob_of(b"an action");
    rig.index
        .put_action(&index_form(&action), &[0xff, 0xff, 0xff], &[])
        .expect("put");

    let failed = get(&rig, &action).await.expect_err("corrupt");

    assert_eq!(failed.code(), Code::Internal);
    assert!(failed.message().contains("does not decode"), "{failed}");
}

/// Asserts that a state store that cannot answer fails a lookup UNAVAILABLE (not a miss) and
/// fails a write UNAVAILABLE and stores nothing; and a result the store cannot hold is
/// INVALID_ARGUMENT.
#[tokio::test]
async fn a_state_store_that_cannot_answer_fails_unavailable() {
    let rig = Rig::new().await;
    let (action, _) = blob_of(b"an action");
    let (output, bytes) = blob_len(100, 7);
    upload(&rig, &output, &bytes).await;
    rig.stored
        .put_action_result(&action, &names(&output))
        .await
        .expect("written");
    let huge = ActionResult {
        stdout_raw: vec![0; kbf_pack_index::action::MAX_RESULT_BYTES + 1],
        ..ActionResult::default()
    };

    rig.index.fail_get_action(1);
    let lookup = get(&rig, &action).await.expect_err("store down");
    rig.index.fail_put_action(1);
    let write = rig.stored.put_action_result(&action, &names(&output)).await;
    let oversized = rig.stored.put_action_result(&action, &huge).await;

    assert_eq!(lookup.code(), Code::Unavailable);
    assert_eq!(
        Status::from(write.expect_err("store down")).code(),
        Code::Unavailable
    );
    assert_eq!(
        Status::from(oversized.expect_err("too long")).code(),
        Code::InvalidArgument
    );
    assert_eq!(
        get(&rig, &action).await.expect("still served"),
        names(&output)
    );
}

/// Asserts that a client never writes the action cache, and that a digest function other than
/// SHA-256 is refused.
#[tokio::test]
async fn a_client_cannot_write_the_action_cache() {
    let rig = Rig::new().await;
    let mut cache = ActionCacheClient::new(rig.front.clone());

    let update = cache
        .update_action_result(UpdateActionResultRequest::default())
        .await
        .expect_err("refused");
    let other = cache
        .get_action_result(GetActionResultRequest {
            digest_function: reapi::digest_function::Value::Sha1 as i32,
            ..GetActionResultRequest::default()
        })
        .await
        .expect_err("not SHA-256");

    assert_eq!(update.code(), Code::PermissionDenied);
    assert_eq!(other.code(), Code::InvalidArgument);
}
