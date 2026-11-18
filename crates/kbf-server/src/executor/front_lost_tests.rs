//! An object the store lost while the frontend's state names it: the action's trees and the
//! writer's memos drop what they say of it, the action's next tree is checked against the store,
//! and an object neither names has the base tree published again, one publish at a time.

use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;

use futures::FutureExt as _;
use tokio::sync::{Notify, Semaphore};

use super::put_dir::{DirPrep, WALL, action_with_input, base, listed_base, rig_over};
use super::*;

/// The SHA-256 bytes of the 64 hex digits `hex`.
fn sha(hex: &str) -> [u8; 32] {
    let mut sha = [0_u8; 32];
    for (i, byte) in sha.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex");
    }
    sha
}

/// A rig in put-directory mode with the base published, whose base is published again by what
/// `republish` holds.
fn rig_republishing(republish: Arc<OnceLock<Republish>>) -> (Rig, Arc<DirPrep>) {
    let prepare = DirPrep::new(None, 7_200);
    let slot = Arc::new(OnceLock::new());
    slot.set(base()).expect("set once");
    let dirs = Arc::clone(&prepare);
    let rig = Rig::over(prepare.clone(), move |packs, backend| {
        dirs.set_backend(backend.clone());
        PutDirParts::new(packs, backend, slot).with_republish(republish)
    });
    (rig, prepare)
}

/// A publish again that counts its runs and answers `answer`.
fn counting(runs: &Arc<AtomicUsize>, answer: Result<(), String>) -> Republish {
    let runs = Arc::clone(runs);
    Republish::new(move || {
        runs.fetch_add(1, Ordering::SeqCst);
        let answer = answer.clone();
        async move { answer }.boxed()
    })
}

/// The tree a mint of `action` wrote, in hex, and the mint's answer.
async fn mint_tree(rig: &Rig, action: &Digest) -> Result<String, MintError> {
    rig.front
        .mint(lease(), action, WALL, InputTree::Manifests)
        .await?;
    Ok(rig.front.tree(lease()).await.expect("a tree"))
}

/// Asserts that an object of an action's own tree the store lost makes the action's next mint
/// write its tree again (checked: its closure collected, not the remembered tree), once: the
/// mint after that uses the tree it remembered then. The base is not published again for an
/// object of the action's own tree.
#[tokio::test]
async fn a_lost_object_of_an_actions_tree_has_its_next_tree_written_checked_once() {
    let runs = Arc::new(AtomicUsize::new(0));
    let republish = Arc::new(OnceLock::new());
    let _ = republish.set(counting(&runs, Ok(())));
    let (rig, prepare) = rig_republishing(republish);
    let action = action_with_input(&rig, "input").await;
    let tree = mint_tree(&rig, &action).await.expect("a tree");
    mint_tree(&rig, &action).await.expect("remembered");
    assert_eq!((prepare.prepared(), prepare.minted()), (1, 1));

    rig.front.lost(&action, &sha(&tree)).await;
    let again = mint_tree(&rig, &action).await.expect("written again");
    let after = mint_tree(&rig, &action).await.expect("remembered again");

    assert_eq!(again, tree, "the same inputs make the same tree");
    assert_eq!(after, tree);
    assert_eq!((prepare.prepared(), prepare.minted()), (2, 2));
    assert_eq!(runs.load(Ordering::SeqCst), 0, "not the base's");
}

/// Asserts that a checked tree over a pack the store lost answers the run's inputs missing,
/// naming the blob, and has the pack index forget it (FindMissingBlobs then reports it), and that
/// the action stays checked until a checked tree is written: once the blob is uploaded again, the
/// next mint writes its tree and the one after uses it.
#[tokio::test]
async fn a_checked_tree_over_a_lost_pack_answers_its_blob_missing_until_it_is_uploaded() {
    let rig = super::put_dir::rig();
    let bytes = Bytes::from_static(b"an input held in a pack");
    let blob = kbf_segments::sha256(&bytes);
    let packs = rig.front.parts.packs.clone();
    packs
        .put(vec![(blob, bytes.clone())])
        .await
        .expect("packed");
    let root = rig
        .store(&Directory {
            files: vec![FileNode {
                name: "input".to_owned(),
                digest: Some(wire(&blob)),
                ..FileNode::default()
            }],
            ..Directory::default()
        })
        .await;
    let action = rig
        .action(|a| a.input_root_digest = Some(wire(&root)), |_| {})
        .await;
    let tree = mint_tree(&rig, &action).await.expect("a tree");
    let pack = packs
        .locate_stored(&blob)
        .await
        .expect("asked")
        .expect("packed");
    let key = ObjectKey::new(&pack.pack_key).expect("a key");
    rig.store.inner.delete(&key).await.expect("lost");

    rig.front.lost(&action, &sha(&tree)).await;
    let missing = mint_tree(&rig, &action).await;

    assert_eq!(missing, Err(MintError::Missing(vec![blob])));
    assert_eq!(packs.missing(&[blob]).await.expect("asked"), [blob]);
    packs
        .put(vec![(blob, bytes)])
        .await
        .expect("uploaded again");
    let written = mint_tree(&rig, &action).await.expect("written");
    assert_ne!(written, tree, "the tree names the new pack");
    let remembered = mint_tree(&rig, &action).await.expect("remembered");
    assert_eq!(remembered, written);
}

/// Asserts that an object neither the writer's memos nor the action's tree name is taken for the
/// base tree's and has it published again, one publish at a time: a loss reported while a publish
/// runs waits for it and publishes nothing more; a publish that fails is logged and the next loss
/// publishes again.
#[tokio::test]
async fn a_lost_object_of_the_base_has_it_published_again_one_at_a_time() {
    let runs = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let gate = Arc::new(Semaphore::new(0));
    let republish = Arc::new(OnceLock::new());
    let (counted, inside, held) = (Arc::clone(&runs), Arc::clone(&entered), Arc::clone(&gate));
    let _ = republish.set(Republish::new(move || {
        let (counted, inside, held) =
            (Arc::clone(&counted), Arc::clone(&inside), Arc::clone(&held));
        async move {
            let run = counted.fetch_add(1, Ordering::SeqCst);
            inside.notify_one();
            held.acquire().await.expect("open").forget();
            match run {
                0 => Ok(()),
                _ => Err("the bundle is gone".to_owned()),
            }
        }
        .boxed()
    }));
    let (rig, _) = rig_republishing(republish);
    let action = action_with_input(&rig, "input").await;
    mint_tree(&rig, &action).await.expect("a tree");
    let unknown = [9_u8; 32];

    let first = rig.front.lost(&action, &unknown);
    let second = async {
        entered.notified().await;
        let waiting = rig.front.lost(&action, &unknown);
        let opened = async {
            tokio::task::yield_now().await;
            gate.add_permits(1);
        };
        futures::join!(waiting, opened);
    };
    futures::join!(first, second);
    assert_eq!(runs.load(Ordering::SeqCst), 1, "one publish for both");

    gate.add_permits(1);
    rig.front.lost(&action, &unknown).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        2,
        "a later loss publishes again"
    );
}

/// Asserts that a frontend with nothing to publish the base again with takes a loss of an
/// object no memo names without failing, and still checks the action's next tree.
#[tokio::test]
async fn a_lost_object_with_no_publish_again_still_checks_the_next_tree() {
    let rig = super::put_dir::rig();
    let action = action_with_input(&rig, "input").await;
    mint_tree(&rig, &action).await.expect("a tree");

    rig.front.lost(&action, &[9; 32]).await;
    mint_tree(&rig, &action).await.expect("written again");

    assert!(
        rig.front
            .checked()
            .get(&cas_digest(&action).expect("a digest"))
            .is_none()
    );
}

/// Asserts the same for a tree list (`--executor-input-tree provider`): a lost object of the
/// action's own list (named by its key) has the next list written checked, once, and publishes
/// nothing again.
#[tokio::test]
async fn a_lost_tree_list_has_the_next_list_written_checked_once() {
    let prepare = DirPrep::new(None, 7_200);
    let rig = rig_over(Arc::clone(&prepare), Some(listed_base()));
    let action = action_with_input(&rig, "input").await;
    let mint = || async {
        rig.front
            .mint(lease(), &action, WALL, InputTree::Provider)
            .await
            .expect("a dir");
        rig.front.tree(lease()).await.expect("a list")
    };
    let key = mint().await;
    let hex = key.strip_prefix("cas/l/").expect("a cas/l/ key");

    rig.front.lost(&action, &sha(hex)).await;
    let again = mint().await;
    let after = mint().await;

    assert_eq!((again, after), (key.clone(), key));
    assert_eq!((prepare.prepared(), prepare.minted()), (2, 1));
}

/// Asserts that a loss reported for an action digest that is not one changes nothing: the
/// action's remembered tree is still used.
#[tokio::test]
async fn a_lost_object_of_an_action_that_is_no_digest_changes_nothing() {
    let (rig, prepare) = rig_republishing(Arc::default());
    let action = action_with_input(&rig, "input").await;
    let tree = mint_tree(&rig, &action).await.expect("a tree");
    let bad = Digest {
        hash: action.hash.clone(),
        size_bytes: -1,
    };

    rig.front.lost(&bad, &sha(&tree)).await;
    mint_tree(&rig, &action).await.expect("remembered");

    assert_eq!((prepare.prepared(), prepare.minted()), (1, 1));
}

/// Asserts that a publish again shows as its name alone: it holds a closure, which says nothing.
#[test]
fn a_publish_again_shows_as_its_name() {
    let republish = Republish::new(|| async { Ok(()) }.boxed());
    assert_eq!(format!("{republish:?}"), "Republish");
}
