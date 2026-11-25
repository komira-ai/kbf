//! What an Execute over a frontend with no storage node records of its input walk's reads
//! through the packs: the pack index lookup and the ranged pack read of each Directory it
//! reads, and the rounds of reads its longest path took; and that a tree a client uploaded
//! through this frontend costs it none of them.
//!
//! Every test says the behaviour it asserts.

use std::sync::Arc;

use bytes::Bytes;
use kbf_auth::Authorizers;
use kbf_front::{ExecutionService, PackSettings, Packs};
use kbf_pack::{PackKeys, WriterKind};
use kbf_proto::reapi::execution_server::Execution as _;
use kbf_proto::reapi::{Action, Command, Directory, DirectoryNode, ExecuteRequest};
use kbf_types::Digest;
use prost::Message as _;

use crate::cas::{item, update};
use crate::empty_blob::{Queue, file, proto};
use crate::rig::{Rig, SMALL, blob_of};

/// An Action over a root that names the file `top` and a Directory `sub` naming the file
/// `inner`: every blob of it, and the Action's digest. `tag` makes the tree distinct.
fn action_tree(tag: &str) -> (Vec<(Digest, Bytes)>, Digest) {
    let top = blob_of(format!("{tag} top").as_bytes());
    let inner = blob_of(format!("{tag} inner").as_bytes());
    let sub = blob_of(
        &Directory {
            files: vec![file("inner", &inner.0)],
            ..Directory::default()
        }
        .encode_to_vec(),
    );
    let root = blob_of(
        &Directory {
            files: vec![file("top", &top.0)],
            directories: vec![DirectoryNode {
                name: "sub".to_owned(),
                digest: Some(proto(&sub.0)),
            }],
            ..Directory::default()
        }
        .encode_to_vec(),
    );
    let command = blob_of(
        &Command {
            arguments: vec!["true".to_owned()],
            ..Command::default()
        }
        .encode_to_vec(),
    );
    let action = blob_of(
        &Action {
            command_digest: Some(proto(&command.0)),
            input_root_digest: Some(proto(&root.0)),
            ..Action::default()
        }
        .encode_to_vec(),
    );
    let digest = action.0;
    (vec![action, command, root, sub, top, inner], digest)
}

/// Catches the walk's pack steps measured and never recorded by an Execute (or recorded from
/// another figure): an Execute of a tree another frontend uploaded into the same packs reads
/// its two Directories through the packs, and records two index lookups, two ranged pack reads
/// and a depth of two reads; an Execute of a tree uploaded through this frontend reads neither
/// Directory, records no lookup or pack read, and a depth of none.
#[tokio::test]
async fn an_execute_records_the_pack_steps_and_depth_of_its_directory_reads() {
    let rig = Rig::new().await;
    let settings = PackSettings {
        max_blob_bytes: SMALL,
        ..PackSettings::default()
    };
    let keys = PackKeys::new(WriterKind::Frontend, "fe2", "boot1").expect("keys");
    let other = Packs::new(Arc::clone(&rig.store), rig.index.clone(), keys, settings)
        .expect("the other frontend's packs");
    let (cold, cold_action) = action_tree("cold");
    other
        .put(cold)
        .await
        .expect("uploaded through the other frontend");
    let (warm, warm_action) = action_tree("warm");
    let warm = warm.iter().map(|(d, b)| item(d, b)).collect();
    update(&rig, warm)
        .await
        .expect("uploaded through this frontend");
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
    let m = rig.backend.metrics();

    assert!(
        execute(&cold_action).await.is_ok(),
        "the cold Execute was refused"
    );

    assert_eq!(m.execute_walk_dirs_read.get(), 2);
    assert_eq!(
        m.execute_walk_dir_index.count(),
        2,
        "an index lookup per read"
    );
    assert_eq!(
        m.execute_walk_dir_object.count(),
        2,
        "a ranged pack read per read"
    );
    assert_eq!(
        (m.execute_walk_depth.count(), m.execute_walk_depth.sum()),
        (1, 2)
    );

    assert!(
        execute(&warm_action).await.is_ok(),
        "the warm Execute was refused"
    );

    assert_eq!(
        m.execute_walk_dirs_read.get(),
        2,
        "the uploaded tree was read"
    );
    assert_eq!(m.execute_walk_dir_index.count(), 2);
    assert_eq!(m.execute_walk_dir_object.count(), 2);
    assert_eq!(
        (m.execute_walk_depth.count(), m.execute_walk_depth.sum()),
        (2, 2)
    );
    assert_eq!(queue.submitted.lock().expect("lock").len(), 2);
    let text = rig.backend.render_metrics();
    for line in [
        "kbf_execute_walk_dir_index_seconds_count 2",
        "kbf_execute_walk_dir_object_seconds_count 2",
        "kbf_execute_walk_depth_count 2",
        "kbf_execute_walk_depth_sum 2",
    ] {
        assert!(text.lines().any(|l| l == line), "{line} missing");
    }
}
