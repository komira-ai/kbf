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

    rig.front
        .lost(&action, &LostObject::Manifest(sha(&tree)))
        .await;
    let again = mint_tree(&rig, &action).await.expect("written again");
    let after = mint_tree(&rig, &action).await.expect("remembered again");

    assert_eq!(again, tree, "the same inputs make the same tree");
    assert_eq!(after, tree);
    assert_eq!((prepare.prepared(), prepare.minted()), (2, 2));
    assert_eq!(runs.load(Ordering::SeqCst), 0, "not the base's");
}

/// The action, the blob, the key of the pack it is in, and the input root.
type OverPacked = (Digest, kbf_types::Digest, String, kbf_types::Digest);

/// An action whose input root holds one file, `bytes`, packed by the frontend's packs.
async fn action_over_packed(rig: &Rig, bytes: &'static [u8]) -> OverPacked {
    let bytes = Bytes::from_static(bytes);
    let blob = kbf_segments::sha256(&bytes);
    let packs = rig.front.packs.clone();
    packs.put(vec![(blob, bytes)]).await.expect("packed");
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
    let pack = packs
        .locate_stored(&blob)
        .await
        .expect("asked")
        .expect("packed");
    (action, blob, pack.pack_key, root)
}

/// Moves the test's clock on by `by`.
fn wait(by: Duration) {
    CLOCK.with(|c| c.set(c.get() + by.as_secs()));
}

/// Asserts that a checked tree over a pack the store lost forgets nothing on the first look at
/// it (the store may answer "not found" for a while for an object it holds): the tree is written
/// naming the pack. A checked tree [`kbf_pack::CONFIRM_AFTER`] later, the pack still absent,
/// answers the run's inputs missing, naming the blob, and has the pack index forget the pack
/// (FindMissingBlobs then reports the blob); the action stays checked until a checked tree is
/// written: once the blob is uploaded again, the next mint writes its tree and the one after uses
/// it.
#[tokio::test]
async fn a_checked_tree_over_a_lost_pack_answers_its_blob_missing_once_the_loss_is_confirmed() {
    let rig = super::put_dir::rig();
    let (action, blob, pack, _) = action_over_packed(&rig, b"an input held in a pack").await;
    let packs = rig.front.packs.clone();
    let tree = mint_tree(&rig, &action).await.expect("a tree");
    let key = ObjectKey::new(&pack).expect("a key");
    rig.store.inner.delete(&key).await.expect("lost");
    rig.index_clock.advance(kbf_pack::MIN_AGE);

    rig.front
        .lost(&action, &LostObject::Manifest(sha(&tree)))
        .await;
    let first = mint_tree(&rig, &action).await;
    let kept = packs.missing(&[blob]).await.expect("asked");
    wait(kbf_pack::CONFIRM_AFTER);
    rig.front
        .lost(&action, &LostObject::Manifest(sha(&tree)))
        .await;
    let missing = mint_tree(&rig, &action).await;

    assert_eq!(first, Ok(tree.clone()), "one absence forgets nothing");
    assert!(kept.is_empty(), "{kept:?}");
    assert_eq!(missing, Err(MintError::Missing(vec![blob])));
    assert_eq!(packs.missing(&[blob]).await.expect("asked"), [blob]);
    packs
        .put(vec![(blob, Bytes::from_static(b"an input held in a pack"))])
        .await
        .expect("uploaded again");
    let written = mint_tree(&rig, &action).await.expect("written");
    assert_ne!(written, tree, "the tree names the new pack");
    let remembered = mint_tree(&rig, &action).await.expect("remembered");
    assert_eq!(remembered, written);
}

/// Asserts the loop a job's missing source takes (tierstore's `404 source missing: <key>`): the
/// first report drops the action's remembered tree and the memos' entries that name the pack,
/// and notes the absence, so the next mint writes the tree checked and, the pack not yet
/// confirmed lost, names it again (and the memos remember it again); a report
/// [`kbf_pack::CONFIRM_AFTER`] later confirms the absence, forgets the pack and drops the memos'
/// entries of it before the retry, so the retry's mint answers the input missing. Once the
/// client uploads the blob again (into a new pack), a second action over the same input, written
/// from the memos, builds its input's manifest again over the new pack: no memo hands out the old
/// one.
#[tokio::test]
async fn a_missing_source_drops_the_memos_and_the_retry_does_not_reuse_its_pack() {
    let bytes: &'static [u8] = b"an input whose pack goes";
    let rig = super::put_dir::rig();
    let (action, blob, pack, root) = action_over_packed(&rig, bytes).await;
    let tree = mint_tree(&rig, &action).await.expect("a tree");
    rig.store
        .inner
        .delete(&ObjectKey::new(&pack).expect("a key"))
        .await
        .expect("lost");
    rig.index_clock.advance(kbf_pack::MIN_AGE);
    let source = LostObject::Source(pack.clone());

    rig.front.lost(&action, &source).await;
    let not_yet = mint_tree(&rig, &action).await;
    wait(kbf_pack::CONFIRM_AFTER);
    rig.front.lost(&action, &source).await;
    let retry = mint_tree(&rig, &action).await;
    let packs = rig.front.packs.clone();
    let reported = packs.missing(&[blob]).await.expect("asked");
    packs
        .put(vec![(blob, Bytes::from_static(bytes))])
        .await
        .expect("uploaded again");
    let other = rig
        .action(
            |a| a.input_root_digest = Some(wire(&root)),
            |c| c.arguments = vec!["another".to_owned()],
        )
        .await;
    let again = packs
        .locate_stored(&blob)
        .await
        .expect("asked")
        .expect("packed");
    let named_before = manifests_naming(&rig, &again.pack_key).await;
    let shared = mint_tree(&rig, &other).await.expect("a tree");
    let named_after = manifests_naming(&rig, &again.pack_key).await;

    assert_eq!(not_yet, Ok(tree.clone()), "a first absence forgets nothing");
    assert_eq!(retry, Err(MintError::Missing(vec![blob])));
    assert_eq!(reported, [blob]);
    assert_ne!(again.pack_key, pack);
    assert_ne!(shared, tree);
    assert_eq!(named_before, 0);
    assert_eq!(
        named_after, 1,
        "the input's manifest was built again over the new pack, not taken from a memo"
    );
}

/// How many manifests under `cas/l/` in the rig's store name a span of the pack `key`.
async fn manifests_naming(rig: &Rig, key: &str) -> usize {
    let needle = format!("\"key\":\"{key}\"");
    let prefix = KeyPrefix::new("cas/l/").expect("a prefix");
    let page = rig.store.inner.list(&prefix, None, PageSize::MAX).await;
    let mut naming = 0;
    for object in page.expect("listed").objects {
        let Some(range) = ByteRange::new(0, object.size) else {
            continue;
        };
        let bytes = rig.store.inner.get_range(&object.key, range).await;
        let text = String::from_utf8_lossy(&bytes.expect("read")).into_owned();
        naming += usize::from(text.contains(&needle));
    }
    naming
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
    let unknown = LostObject::Manifest([9_u8; 32]);

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

    rig.front
        .lost(&action, &LostObject::Manifest([9; 32]))
        .await;
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

    rig.front
        .lost(&action, &LostObject::Manifest(sha(hex)))
        .await;
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

    rig.front
        .lost(&bad, &LostObject::Manifest(sha(&tree)))
        .await;
    mint_tree(&rig, &action).await.expect("remembered");

    assert_eq!((prepare.prepared(), prepare.minted()), (1, 1));
}

/// Asserts that a publish again shows as its name alone: it holds a closure, which says nothing.
#[test]
fn a_publish_again_shows_as_its_name() {
    let republish = Republish::new(|| async { Ok(()) }.boxed());
    assert_eq!(format!("{republish:?}"), "Republish");
}
