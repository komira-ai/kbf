//! A blob's `cas/l/` key holding an object of another length than the blob: a write the store
//! kept cut short (a PUT whose body failed before its first byte, kept as an object of 0 bytes).
//! It is never named in a tree: the blob's write deletes it and stores the blob, in the same
//! tree when the write's own attempt left it (a stalled body, tried once more) and in the next
//! tree when an earlier write did. A store that will not delete it, or a key a create-only
//! write finds taken by one, makes no tree, by name.

use kbf_front::{PackSettings, Packs};
use kbf_objstore::{Capabilities, MemoryStore, ObjectStore as _};
use kbf_pack::{PackKeys, WriterKind};
use kbf_pack_index::PackIndex;
use kbf_pack_index::memory::MemoryPackIndex;

use super::*;

/// tierstore's answer to a PUT of whose body no byte came for its idle timeout.
fn stalled() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 400,
        code: "RequestTimeout".to_owned(),
        message: "No byte of the request body arrived within 30s.".to_owned(),
    }
}

/// The input root of one blob only a storage node holds, the blob, its key and its bytes.
async fn unlocated(rig: &Rig) -> (Digest, Digest, String, Bytes) {
    let bytes = Bytes::from_static(b"a large blob the store once kept cut short");
    let blob = rig.farm.cas(bytes.clone()).await;
    let root = rig
        .store(&directory(vec![file("f", &blob, false)], vec![], vec![]))
        .await;
    (root, blob, whole_key(&blob.hash_hex()), bytes)
}

/// The next writes of `key` keep an object of 0 bytes and give `answers`, in order.
fn keeps(rig: &Rig, key: &str, answers: Vec<ObjectStoreError>) {
    let mut state = rig.farm.state();
    state.faults.keeps.insert(key.to_owned(), answers.into());
}

/// The deletes made so far, by key.
fn deletes(rig: &Rig) -> Vec<String> {
    rig.farm.state().deletes.clone()
}

/// Asserts that `tree` names the blob of `bytes` at `f` as the whole object, which holds them.
fn names_the_whole_blob(rig: &Rig, tree: &ActionTree, key: &str, bytes: &Bytes) {
    assert_eq!(rig.farm.object(key).as_ref(), Some(bytes));
    let entries = rig.farm.flatten(tree).expect("every manifest is stored");
    let f = entries.iter().find(|e| &*e.name == "f").expect("f");
    assert!(
        matches!(f.kind, tierstore::EntryKind::File { source: None, .. }),
        "{f:?}"
    );
}

/// Catches: a write whose body stalled, which tierstore answers 400 `RequestTimeout` and its
/// store keeps as an object of 0 bytes, ending the tree (and every later tree of the blob
/// finding that object): the write is tried once more, which deletes the object of 0 bytes
/// and stores the blob, and the tree is made.
#[tokio::test]
async fn a_stalled_write_kept_at_0_bytes_is_deleted_and_written_again() {
    let rig = Rig::new().await;
    let (root, blob, key, bytes) = unlocated(&rig).await;
    keeps(&rig, &key, vec![stalled()]);
    let tree = rig.write(&root).await.expect("a tree");
    names_the_whole_blob(&rig, &tree, &key, &bytes);
    assert_eq!(rig.farm.puts_of(&blob.hash_hex()), 2);
    assert_eq!(deletes(&rig), [key]);
}

/// Catches: an object of another length at a blob's key taken for the blob (a tree would read
/// wrong bytes) or failing every tree of the blob: an object of 0 bytes and one cut short after
/// some bytes are each deleted, the blob is written in their place, and the tree names it.
#[tokio::test]
async fn an_object_of_another_length_at_the_key_is_deleted_and_the_blob_written() {
    for kept in [Bytes::new(), Bytes::from_static(b"a large")] {
        let rig = Rig::new().await;
        let (root, blob, key, bytes) = unlocated(&rig).await;
        rig.farm.state().objects.insert(key.clone(), kept.clone());
        let tree = rig.write(&root).await.expect("a tree");
        names_the_whole_blob(&rig, &tree, &key, &bytes);
        assert_eq!(rig.farm.puts_of(&blob.hash_hex()), 1, "{kept:?}");
        assert_eq!(deletes(&rig), [key], "{kept:?}");
    }
}

/// Catches: a write that stalls twice tried without end, or a tree that names a blob whose
/// write failed: the second `RequestTimeout` ends the tree, `why=store`, naming the key and the
/// store's answer, with the object of 0 bytes left; the next tree deletes it and writes the
/// blob.
#[tokio::test]
async fn a_write_that_stalls_twice_makes_no_tree_and_the_next_tree_repairs_it() {
    let rig = Rig::new().await;
    let (root, blob, key, bytes) = unlocated(&rig).await;
    keeps(&rig, &key, vec![stalled(), stalled()]);
    let refused = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        refused.to_string(),
        format!(
            "the write of {key} failed: store answered 400 RequestTimeout: No byte of the request body arrived within 30s."
        )
    );
    assert_eq!(refused.why(), "store");
    assert_eq!(rig.farm.object(&key), Some(Bytes::new()));
    assert_eq!(rig.farm.puts_of(&blob.hash_hex()), 2);
    let tree = rig.write(&root).await.expect("a tree");
    names_the_whole_blob(&rig, &tree, &key, &bytes);
    assert_eq!(deletes(&rig), [key.clone(), key]);
}

/// Catches: a create-only write that finds its key taken trusting whatever is there: an object
/// of 0 bytes under the key is [`NoTree::WrongLength`], `why=store`, and the tree is not made;
/// the next tree deletes it and writes the blob.
#[tokio::test]
async fn a_key_found_taken_by_an_object_of_another_length_makes_no_tree() {
    let rig = Rig::new().await;
    let (root, blob, key, bytes) = unlocated(&rig).await;
    let taken = ObjectStoreError::AlreadyExists(ObjectKey::new(&key).expect("a key"));
    keeps(&rig, &key, vec![taken]);
    let refused = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        refused,
        NoTree::WrongLength {
            digest: blob,
            length: 0
        }
    );
    assert_eq!(refused.why(), "store");
    assert!(deletes(&rig).is_empty());
    let tree = rig.write(&root).await.expect("a tree");
    names_the_whole_blob(&rig, &tree, &key, &bytes);
    assert_eq!(deletes(&rig), [key]);
}

/// Catches: a write whose answer was lost after the store kept the blob (a dead connection)
/// sent again, or the blob deleted: the second attempt finds an object of the blob's length,
/// which is the blob, and sends and deletes nothing.
#[tokio::test]
async fn a_blob_stored_before_its_answer_was_lost_is_not_written_again() {
    let rig = Rig::new().await;
    let (root, blob, key, bytes) = unlocated(&rig).await;
    let dead = ObjectStoreError::Transport(Arc::new(std::io::Error::other(
        "connection closed before message completed",
    )));
    let lost = VecDeque::from([dead]);
    rig.farm
        .state()
        .faults
        .stored_then
        .insert(key.clone(), lost);
    let tree = rig.write(&root).await.expect("a tree");
    names_the_whole_blob(&rig, &tree, &key, &bytes);
    assert_eq!(rig.farm.puts_of(&blob.hash_hex()), 1);
    assert!(deletes(&rig).is_empty());
}

/// Catches: a refused delete taken for done (the create-only write would then find the key
/// taken) or a blob written over what is there: the tree is not made, `why=store`, naming the
/// key and the store's refusal; the object stays, and no write is sent.
#[tokio::test]
async fn a_refused_delete_makes_no_tree() {
    let rig = Rig::new().await;
    let (root, blob, key, _) = unlocated(&rig).await;
    rig.farm.state().objects.insert(key.clone(), Bytes::new());
    rig.farm.state().faults.delete = Some(key.clone());
    let refused = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        refused,
        NoTree::Put {
            key: key.clone(),
            why: "the store does not support deletes of this key".to_owned(),
        }
    );
    assert_eq!(refused.why(), "store");
    assert_eq!(rig.farm.object(&key), Some(Bytes::new()));
    assert_eq!(rig.farm.puts_of(&blob.hash_hex()), 0);
}

/// Catches: a length check that fails before the write taken for an absent key (a write over
/// what may be there) or retried without end: the tree is not made, `why=store`, naming the
/// key, and no write is sent.
#[tokio::test]
async fn a_failed_length_check_before_the_write_makes_no_tree() {
    let rig = Rig::new().await;
    let (root, blob, key, _) = unlocated(&rig).await;
    rig.farm.state().faults.head_of = Some(key.clone());
    let refused = rig.write(&root).await.expect_err("no tree");
    assert_eq!(
        refused,
        NoTree::Put {
            key,
            why: "the store does not support heads of this key".to_owned(),
        }
    );
    assert_eq!(rig.farm.puts_of(&blob.hash_hex()), 0);
}

/// Catches: the frontend's packs, the store a running frontend writes a tree's blobs to, not
/// deleting through to their object store: an object deleted through them is gone from the
/// store, and deleting an absent key is success.
#[tokio::test]
async fn the_packs_delete_an_object_from_their_store() {
    let memory = Arc::new(MemoryStore::new(Capabilities {
        conditional_put: true,
        object_lock: false,
    }));
    let clock = Arc::new(kbf_state::ManualClock::default());
    let index: Arc<dyn PackIndex> = Arc::new(MemoryPackIndex::new(clock));
    let keys = PackKeys::new(WriterKind::Frontend, "fe1", "boot1").expect("keys");
    let packs =
        Packs::new(Arc::clone(&memory), index, keys, PackSettings::default()).expect("packs");
    let key = ObjectKey::new("cas/l/kept-short").expect("a key");
    memory
        .put_new(&key, Bytes::new(), None)
        .await
        .expect("stored");
    LargeStore::delete(&packs, &key).await.expect("deleted");
    assert_eq!(memory.len(&key).await.expect("a length"), None);
    LargeStore::delete(&packs, &key)
        .await
        .expect("an absent key");
}
