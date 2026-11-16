//! The plane of a frontend with no storage node: the calls that only the crate makes (the
//! lookup of the action cache and the walk of an input tree). The rest of the plane is tested
//! through [`Backend`](super::super::Backend) in `tests/stored/backend.rs`.

use bytes::Bytes;
use kbf_objstore::{Capabilities, MemoryStore};
use kbf_pack::{PackKeys, WriterKind};
use kbf_pack_index::action::ActionStore;
use kbf_pack_index::memory::MemoryPackIndex;
use kbf_proto::reapi::{ActionResult, Directory, DirectoryNode, FileNode, OutputFile};
use kbf_state::ManualClock;
use prost::Message as _;

use super::super::Backend;
use super::*;
use crate::PackSettings;

fn stored() -> Arc<Stored> {
    let create_only = Capabilities {
        conditional_put: true,
        object_lock: false,
    };
    let store = Arc::new(MemoryStore::new(create_only));
    let actions: Arc<dyn ActionStore> =
        Arc::new(MemoryPackIndex::new(Arc::new(ManualClock::default())));
    let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
    let stored = Stored::new(store, actions, keys, PackSettings::default());
    Arc::new(stored.expect("stored"))
}

fn blob(text: &str) -> (Digest, Bytes) {
    let bytes = Bytes::from(text.to_owned());
    (kbf_segments::sha256(&bytes), bytes)
}

fn dir(files: &[Digest], dirs: &[Digest]) -> (Digest, Bytes) {
    let message = Directory {
        files: files
            .iter()
            .map(|d| FileNode {
                name: d.hash_hex(),
                digest: Some(crate::digest_to_proto(d)),
                ..FileNode::default()
            })
            .collect(),
        directories: dirs
            .iter()
            .map(|d| DirectoryNode {
                name: d.hash_hex(),
                digest: Some(crate::digest_to_proto(d)),
            })
            .collect(),
        ..Directory::default()
    };
    let bytes = Bytes::from(message.encode_to_vec());
    (kbf_segments::sha256(&bytes), bytes)
}

/// Asserts that the walk of an input tree reads its Directories from the packs and reports the
/// files none holds as absent, naming no other place to look: a tree whose Directories and one
/// file are packed and whose other file is nowhere is walked, and exactly that file is absent.
#[tokio::test]
async fn the_walk_of_an_input_tree_reads_the_packs_and_reports_what_is_absent() {
    let stored = stored();
    let (held, absent) = (blob("held"), blob("absent"));
    let leaf = dir(&[held.0, absent.0], &[]);
    let root = dir(&[], &[leaf.0]);
    let packed = vec![(held.0, held.1), (leaf.0, leaf.1), (root.0, root.1)];
    stored.packs().put(packed).await.expect("packed");
    let backend = Backend::stored(Arc::clone(&stored));

    let walked = backend.walk_inputs(root.0).await.expect("walked");

    assert_eq!(walked.absent, vec![absent.0]);
}

/// Asserts that a tree whose root Directory is not held is reported absent as a whole: the root
/// is the one absent blob, as over any other store. (The root names a file: a Directory that
/// names nothing encodes to no bytes, which is the empty blob, and every store holds that.)
#[tokio::test]
async fn the_walk_of_a_tree_whose_root_is_not_held_reports_the_root_absent() {
    let backend = Backend::stored(stored());
    let root = dir(&[blob("a file of the root").0], &[]);

    let walked = backend.walk_inputs(root.0).await;

    assert_eq!(walked.expect("walked").absent, vec![root.0]);
}

/// Asserts that the lookup of the action cache answers the digest of the encoded result with it,
/// and nothing for an action it has no row for.
#[tokio::test]
async fn the_lookup_answers_the_digest_of_the_encoded_result() {
    let stored = stored();
    let output = blob("an output");
    stored
        .packs()
        .put(vec![output.clone()])
        .await
        .expect("packed");
    let backend = Backend::stored(Arc::clone(&stored));
    let action = blob("an action").0;
    let result = ActionResult {
        output_files: vec![OutputFile {
            path: "out".to_owned(),
            digest: Some(crate::digest_to_proto(&output.0)),
            ..OutputFile::default()
        }],
        ..ActionResult::default()
    };
    let cold = backend.action_result(&action).await;
    backend
        .put_action_result(action, &result)
        .await
        .expect("written");

    let warm = backend.action_result(&action).await;

    assert!(cold.expect("asked").is_none());
    let (digest, served) = warm.expect("asked").expect("a row");
    assert_eq!(served, result);
    assert_eq!(digest, kbf_segments::sha256(&result.encode_to_vec()));
}
