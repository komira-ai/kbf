//! `GetActionResult` through a proxy given packs: an entry whose outputs a daemon wrote into
//! packs is held by the pack index and not by the storage node, so the hit is checked against
//! the index and the node together, and the same entry through a proxy without packs is the
//! node's alone.
//!
//! Every test says the behaviour it asserts.

use std::sync::Arc;

use bytes::Bytes;
use kbf_auth::Authorizers;
use kbf_front::{Owners, PackSettings, Proxy};
use kbf_meta::{ActionRecord, Role};
use kbf_proto::reapi::action_cache_client::ActionCacheClient;
use kbf_proto::reapi::{
    ActionResult, Directory, FileNode, GetActionResultRequest, OutputDirectory, OutputFile, Tree,
};
use kbf_storage::Storage;
use kbf_types::Digest;
use prost::Message as _;
use tonic::Code;
use tonic::transport::Channel;

use crate::harness;
use crate::support::{Rig, blob, node};

/// One storage node, a proxy given packs in front of it, and a proxy without packs.
struct Farm {
    node: Arc<dyn Storage>,
    packed: Rig,
    plain: Channel,
}

async fn farm() -> Farm {
    let (node, addr) = node();
    let owners = Owners::single(harness::owner("node", addr));
    let proxy = Proxy::new(owners.clone(), Arc::new(Authorizers::allow_all()));
    let plain = harness::serve(proxy.routes()).await;
    let packed = Rig::over(owners, PackSettings::default()).await;
    Farm {
        node,
        packed,
        plain,
    }
}

impl Farm {
    /// Writes the action-cache entry for `action` on the node as a frontend with packs leaves
    /// it: the result stored as a blob, and the closure naming `on_node` and nothing else.
    async fn enter(&self, action: &Digest, result: &ActionResult, on_node: &[Digest]) {
        let node = &self.node;
        let encoded = Bytes::from(result.encode_to_vec());
        let stored = node.put(encoded).await.expect("the result is stored");
        let record = ActionRecord {
            result: stored,
            closure: on_node.iter().copied().collect(),
        };
        let committed = node.commit_action_record(Role::Daemon, *action, record);
        committed.await.expect("the node takes the entry");
    }

    /// Puts `blobs` in a pack of the frontend.
    async fn pack(&self, blobs: &[&(Digest, Bytes)]) {
        let blobs = blobs.iter().map(|b| (*b).clone()).collect();
        self.packed.packs.put(blobs).await.expect("packed");
    }
}

fn wire(digest: &Digest) -> kbf_proto::reapi::Digest {
    kbf_front::digest_to_proto(digest)
}

/// An output file of the blob `digest`.
fn file(digest: &Digest) -> OutputFile {
    OutputFile {
        path: digest.hash_hex(),
        digest: Some(wire(digest)),
        ..OutputFile::default()
    }
}

/// The `Tree` that names these files, and its digest.
fn tree_of(files: &[Digest]) -> (Digest, Bytes) {
    let named = |digest: &Digest| FileNode {
        name: digest.hash_hex(),
        digest: Some(wire(digest)),
        ..FileNode::default()
    };
    let tree = Tree {
        root: Some(Directory {
            files: files.iter().map(named).collect(),
            ..Directory::default()
        }),
        ..Tree::default()
    };
    let bytes = Bytes::from(tree.encode_to_vec());
    (kbf_segments::sha256(&bytes), bytes)
}

/// A result with a directory output whose tree is `tree`.
fn directory_of(tree: &Digest) -> ActionResult {
    ActionResult {
        output_directories: vec![OutputDirectory {
            path: "out".to_owned(),
            tree_digest: Some(wire(tree)),
            ..OutputDirectory::default()
        }],
        ..ActionResult::default()
    }
}

/// What `GetActionResult` of `action` through `front` answers.
async fn ask(front: &Channel, action: &Digest) -> Result<ActionResult, tonic::Status> {
    let request = GetActionResultRequest {
        action_digest: Some(wire(action)),
        ..GetActionResultRequest::default()
    };
    let mut client = ActionCacheClient::new(front.clone());
    let answered = client.get_action_result(request).await?;
    Ok(answered.into_inner())
}

/// The code `GetActionResult` of `action` through `front` is refused with.
async fn refusal(front: &Channel, action: &Digest) -> Code {
    let answered = ask(front, action).await;
    answered.expect_err("a miss").code()
}

/// Asserts that a hit whose output is only in a pack is served by a proxy given packs, because
/// the index holds the output and the node holds the rest; and that when the output is held by
/// neither it is a miss, while a proxy without packs still serves what the node says (the node
/// checked what it holds and the entry names no more), so that without packs nothing changes.
#[tokio::test]
async fn a_hit_is_served_when_the_index_holds_what_the_node_does_not() {
    let farm = farm().await;
    let (output, stdout) = (blob("packed output"), blob("node stdout"));
    farm.node.put(stdout.1.clone()).await.expect("stored");
    let result = ActionResult {
        output_files: vec![file(&output.0)],
        stdout_digest: Some(wire(&stdout.0)),
        ..ActionResult::default()
    };
    let action = blob("an action with a packed output").0;
    farm.enter(&action, &result, &[stdout.0]).await;
    let front = &farm.packed.front;

    assert_eq!(refusal(front, &action).await, Code::NotFound);
    let plain = ask(&farm.plain, &action).await;
    assert_eq!(plain.expect("the node's"), result);

    farm.pack(&[&output]).await;

    let hit = ask(front, &action).await;
    assert_eq!(hit.expect("a hit"), result);
    let plain = ask(&farm.plain, &action).await;
    assert_eq!(plain.expect("the node's"), result);
}

/// Asserts that the output trees of a hit are read from the packs or the node and followed: a
/// tree in a pack that names a file in a pack is a hit; the same tree naming a file held
/// nowhere is a miss; a tree held nowhere is a miss; and a tree that is not a `Tree` is a miss,
/// not an error.
#[tokio::test]
async fn the_trees_of_a_hit_are_followed() {
    let farm = farm().await;
    let (inside, outside) = (blob("a file in a tree"), blob("a file held nowhere"));
    farm.pack(&[&inside]).await;
    let held = tree_of(&[inside.0]);
    let gone = tree_of(&[inside.0, outside.0]);
    let junk = blob("not a Tree \u{ff}\u{ff}");
    farm.pack(&[&held, &gone]).await;
    farm.node.put(junk.1.clone()).await.expect("stored");
    let nowhere = tree_of(&[outside.0]).0;
    let cases = [
        (held.0, true),
        (gone.0, false),
        (nowhere, false),
        (junk.0, false),
    ];

    for (n, (tree, hit)) in cases.into_iter().enumerate() {
        let action = blob(&format!("an action with a tree {n}")).0;
        farm.enter(&action, &directory_of(&tree), &[]).await;
        let answered = ask(&farm.packed.front, &action).await;
        if hit {
            assert_eq!(answered.expect("a hit"), directory_of(&tree), "case {n}");
        } else {
            let code = answered.expect_err("a miss").code();
            assert_eq!(code, Code::NotFound, "case {n}");
        }
    }
}

/// Asserts that an index that cannot answer is not taken for "the blob is held": the lookup
/// fails UNAVAILABLE and does not serve the hit, and the next lookup, once the index answers,
/// is the hit.
#[tokio::test]
async fn an_index_that_cannot_answer_fails_the_lookup() {
    let farm = farm().await;
    let output = blob("packed output");
    farm.pack(&[&output]).await;
    let result = ActionResult {
        output_files: vec![file(&output.0)],
        ..ActionResult::default()
    };
    let action = blob("an action while the index is down").0;
    farm.enter(&action, &result, &[]).await;
    farm.packed.index.fail_find(1);
    let front = &farm.packed.front;

    assert_eq!(refusal(front, &action).await, Code::Unavailable);

    let again = ask(front, &action).await;
    assert_eq!(again.expect("a hit"), result);
}
