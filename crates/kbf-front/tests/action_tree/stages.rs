//! The stages of a tree's writes (`WriteStage`): the input root's manifests and `.kbf` are at the
//! store together, the root is asked for only once both are stored, no manifest is written while
//! a blob that was nowhere is, a refused `.kbf` fails the tree with no root written, and each
//! stage the writes make is timed once.
//!
//! Writes are held by the fake store's gate and the write is polled by hand, so which writes are
//! open at each step is fixed, not raced. The times are read on tokio's paused clock, which moves
//! only by the fake store's fixed delay of each write.

use std::pin::pin;

use futures::poll;
use kbf_front::action_tree::{WriteStage, WriteTimed};

use super::*;

/// The time each write of the store takes when the test sets [`Faults::delay`].
const WRITE: Duration = Duration::from_millis(10);

/// The stages a write timed, in order, with what each took.
type Timed = Arc<Mutex<Vec<(WriteStage, Duration)>>>;

/// What `timed` holds.
fn held<T>(timed: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    timed.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A timer of a write's stages into `timed`; its writes' parts and counts are left out.
fn timer(timed: &Timed) -> impl Fn(WriteTimed) + Send + Sync + 'static {
    let timed = Arc::clone(timed);
    move |event| {
        if let WriteTimed::Stage(stage, took) = event {
            held(&timed).push((stage, took));
        }
    }
}

/// The stages timed so far, which are taken out.
fn taken(timed: &Timed) -> Vec<(WriteStage, Duration)> {
    std::mem::take(&mut *held(timed))
}

/// The input root holding one file, `f`, whose bytes are a span of a pack; and the hex of its
/// manifest.
async fn packed_root(rig: &Rig) -> (Digest, String) {
    let blob = rig.farm.packed("a packed input", "cas/f/w/p0", 0).await;
    let root = rig
        .store(&directory(vec![file("f", &blob, false)], vec![], vec![]))
        .await;
    let manifest = format!(
        r#"{{"v":1,"children":[{{"name":"f","kind":"file","sha256":"{}","size":{},"exec":false,"source":{{"key":"cas/f/w/p0","offset":0}}}}]}}"#,
        blob.hash_hex(),
        blob.size_bytes
    );
    (root, TreeDigest::of(manifest.as_bytes()).to_string())
}

/// The input root holding one file, `f`, whose bytes are only in the CAS: nowhere a tree can
/// name until it is written whole.
async fn nowhere_root(rig: &Rig) -> (Digest, Digest) {
    let blob = rig.farm.cas("only in a storage node's write tier").await;
    let root = rig
        .store(&directory(vec![file("f", &blob, false)], vec![], vec![]))
        .await;
    (root, blob)
}

/// The hex of `.kbf`'s manifest for the rig's Action over `root`, whose Action and Command are
/// spans of a pack.
async fn kbf_manifest(rig: &Rig, root: &Digest) -> String {
    let action = rig.action(root).await;
    let kbf = format!(
        concat!(
            r#"{{"v":1,"children":["#,
            r#"{{"name":"action","kind":"file","sha256":"{action}","size":{action_len},"exec":false,"source":{{"key":"cas/f/w/p9","offset":100}}}},"#,
            r#"{{"name":"command","kind":"file","sha256":"{command}","size":{command_len},"exec":false,"source":{{"key":"cas/f/w/p9","offset":0}}}}"#,
            r#"]}}"#
        ),
        action = action.hash_hex(),
        action_len = action.size_bytes,
        command = rig.command.hash_hex(),
        command_len = rig.command.size_bytes,
    );
    TreeDigest::of(kbf.as_bytes()).to_string()
}

/// The keys written so far, as a set.
fn put_keys(rig: &Rig) -> BTreeSet<String> {
    rig.farm.puts().into_iter().collect()
}

/// Catches: `.kbf` written only once the input root's manifests are stored, one store round
/// trip more before the root; and a root asked for before `.kbf` and every directory manifest
/// are stored, so that a reader could find a root whose children the store lacks. With every
/// write held, the input root's manifest and `.kbf` are open at the store at once and the root
/// is not asked for; with one of them answered it still is not; with both answered the root is
/// written, last, and the tree reads.
#[tokio::test]
async fn kbf_is_written_beside_the_input_manifests_and_the_root_after_both() {
    let rig = Rig::new().await;
    let (root, work_hex) = packed_root(&rig).await;
    let kbf_hex = kbf_manifest(&rig, &root).await;
    let kept = rig.kept(&root).await;
    rig.farm.state().faults.hold_all = true;
    let mut writing = pin!(rig.writer.write(&kept, &rig.base.tree));
    assert!(poll!(writing.as_mut()).is_pending());
    let together = BTreeSet::from([whole_key(&work_hex), whole_key(&kbf_hex)]);
    assert_eq!(put_keys(&rig), together);
    assert_eq!(rig.farm.state().open, 2, "both are at the store at once");
    rig.farm.gate.add_permits(1);
    assert!(poll!(writing.as_mut()).is_pending());
    assert_eq!(rig.farm.state().open, 1);
    assert_eq!(put_keys(&rig), together, "no root while one is open");
    rig.farm.gate.add_permits(1);
    assert!(poll!(writing.as_mut()).is_pending(), "the root is held");
    assert_eq!(rig.farm.state().open, 1);
    assert_eq!(rig.farm.puts().len(), 3);
    rig.farm.gate.add_permits(1);
    let tree = writing.await.expect("a tree");
    let root_key = whole_key(&tree.digest().to_string());
    assert_eq!(rig.farm.puts().last(), Some(&root_key), "the root is last");
    assert_eq!(rig.farm.puts().len(), 3);
    rig.farm.flatten(&tree).expect("every manifest is stored");
}

/// Catches: a manifest written while a blob that was nowhere is still being written, so that a
/// reader could find a manifest (`.kbf` among them) naming an object `cas/l/` lacks: with the
/// blob's write held, it is the only write asked for; once it is stored, the input root's
/// manifest, `.kbf` and the root follow, the root last.
#[tokio::test]
async fn no_manifest_is_written_while_a_blob_that_was_nowhere_is() {
    let rig = Rig::new().await;
    let (root, blob) = nowhere_root(&rig).await;
    let blob_key = whole_key(&blob.hash_hex());
    rig.farm.state().faults.held = Some(blob_key.clone());
    let kept = rig.kept(&root).await;
    let mut writing = pin!(rig.writer.write(&kept, &rig.base.tree));
    assert!(poll!(writing.as_mut()).is_pending());
    assert_eq!(
        rig.farm.puts(),
        std::slice::from_ref(&blob_key),
        "only the blob"
    );
    rig.farm.gate.add_permits(1);
    let tree = writing.await.expect("a tree");
    let puts = rig.farm.puts();
    assert_eq!(puts.len(), 4, "the blob, `kbf-work`, `.kbf` and the root");
    assert_eq!(puts.first(), Some(&blob_key));
    assert_eq!(puts.last(), Some(&whole_key(&tree.digest().to_string())));
    rig.farm.flatten(&tree).expect("every manifest is stored");
}

/// Catches: a refused `.kbf` taken for written, or a root written over it: the write ends in the
/// store's refusal of `.kbf`, by name; the root is never asked for; the manifests stage is timed
/// and no stage after it; and the next write of the tree writes `.kbf` anew, then the root.
#[tokio::test]
async fn a_refused_kbf_fails_the_tree_and_no_root_is_written() {
    let rig = Rig::new().await;
    let (root, work_hex) = packed_root(&rig).await;
    let kbf_hex = kbf_manifest(&rig, &root).await;
    let kbf_key = whole_key(&kbf_hex);
    rig.farm.state().faults.put = Some(kbf_key.clone());
    let kept = rig.kept(&root).await;
    let timed = Timed::default();
    let refused = (rig.writer)
        .write_named(&kept, &rig.base.tree, |_| (), timer(&timed))
        .await;
    let expected = NoTree::Put {
        key: kbf_key.clone(),
        why: ObjectStoreError::Unsupported("writes to this key").to_string(),
    };
    assert_eq!(refused, Err(expected));
    let together = BTreeSet::from([whole_key(&work_hex), kbf_key.clone()]);
    assert_eq!(put_keys(&rig), together, "no root is asked for");
    let stages: Vec<WriteStage> = taken(&timed).into_iter().map(|(at, _)| at).collect();
    assert_eq!(stages, [WriteStage::Manifests]);
    rig.farm.state().faults.put = None;
    let tree = rig
        .writer
        .write(&kept, &rig.base.tree)
        .await
        .expect("a tree");
    assert_eq!(rig.farm.puts_of(&kbf_hex), 2, "`.kbf` is written anew");
    let root_key = whole_key(&tree.digest().to_string());
    assert_eq!(rig.farm.puts().last(), Some(&root_key), "the root is last");
    rig.farm.flatten(&tree).expect("every manifest is stored");
}

/// Catches: a refused root taken for written, or asked for before the manifests it names are
/// stored: the write ends in the store's refusal of the root, by name, with the input root's
/// manifest and `.kbf` stored before it, and both stages timed.
#[tokio::test]
async fn a_refused_root_fails_the_tree_once_both_are_stored() {
    let rig = Rig::new().await;
    let (root, work_hex) = packed_root(&rig).await;
    let kbf_hex = kbf_manifest(&rig, &root).await;
    let kept = rig.kept(&root).await;
    let timed = Timed::default();
    let farm = Arc::clone(&rig.farm);
    let refused = (rig.writer)
        .write_named(
            &kept,
            &rig.base.tree,
            move |tree| farm.state().faults.put = Some(whole_key(&tree.digest().to_string())),
            timer(&timed),
        )
        .await
        .expect_err("no tree");
    let root_key = rig.farm.state().faults.put.clone().expect("the root's key");
    assert!(
        matches!(&refused, NoTree::Put { key, .. } if *key == root_key),
        "{refused:?}"
    );
    assert_eq!(rig.farm.puts().last(), Some(&root_key));
    assert_eq!(rig.farm.puts().len(), 3);
    assert!(!rig.farm.manifest(&work_hex).is_empty());
    assert!(!rig.farm.manifest(&kbf_hex).is_empty());
    let stages: Vec<WriteStage> = taken(&timed).into_iter().map(|(at, _)| at).collect();
    assert_eq!(stages, [WriteStage::Manifests, WriteStage::Root]);
}

/// Catches: a stage timed twice, not at all, out of order, or for a stage the tree does not
/// make; and `.kbf` and the input root's manifest written one after the other. On tokio's
/// paused clock, which moves only by the store's delay of each write, a tree with a blob that
/// was nowhere times its blobs, its manifests and its root in that order, each one write long:
/// the manifests stage holds two writes in the time of one. A tree checked against the store
/// whose blobs are all spans of packs has no blobs stage.
#[tokio::test(start_paused = true)]
async fn each_stage_is_timed_once_and_the_manifests_take_one_write() {
    let rig = Rig::new().await;
    rig.farm.state().faults.delay = Some(WRITE);
    let (nowhere, _) = nowhere_root(&rig).await;
    let kept = rig.kept(&nowhere).await;
    let timed = Timed::default();
    (rig.writer)
        .write_named(&kept, &rig.base.tree, |_| (), timer(&timed))
        .await
        .expect("a tree");
    assert_eq!(
        taken(&timed),
        [
            (WriteStage::Blobs, WRITE),
            (WriteStage::Manifests, WRITE),
            (WriteStage::Root, WRITE)
        ]
    );
    assert_eq!(rig.farm.puts().len(), 4, "the blob and three manifests");
    let (packed, _) = packed_root(&rig).await;
    let kept = rig.kept(&packed).await;
    (rig.writer)
        .write_checked_named(&kept, &rig.base.tree, |_| (), timer(&timed))
        .await
        .expect("a tree");
    assert_eq!(
        taken(&timed),
        [(WriteStage::Manifests, WRITE), (WriteStage::Root, WRITE)]
    );
}
