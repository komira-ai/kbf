//! The walk of an input tree over the plane of a frontend with no storage node, done again: a
//! tree found held is remembered (`crate::input_walk`'s verified memory) under the packs' epoch
//! ([`Packs::losses`](crate::Packs)), so the next Execute over it asks the pack index nothing,
//! while what changed is still asked about, what is absent is still reported, and a loss the
//! packs find makes every tree be asked about again.

use bytes::Bytes;
use kbf_types::Digest;

use super::tests::{blob, dir, stored};
use super::*;
use crate::backend::Backend;

/// The digests the pack index has been asked whether it holds, and the blobs read from packs.
fn asked_and_read(stored: &Stored) -> (u64, u64) {
    let metrics = stored.packs().metrics();
    (metrics.asked.get(), metrics.reads.get())
}

/// A tree of two Directories, each with its files: `root` names the file `a` and the
/// Directory `sub`, which names `files`.
fn two_level(a: &str, files: &[&(Digest, Bytes)]) -> Vec<(Digest, Bytes)> {
    let a = blob(a);
    let sub = dir(&files.iter().map(|f| f.0).collect::<Vec<_>>(), &[]);
    let root = dir(&[a.0], &[sub.0]);
    let mut blobs = vec![root, sub, a];
    blobs.extend(files.iter().map(|f| (*f).clone()));
    blobs
}

/// Catches an Execute that asks the store again about a tree it found held moments before (on
/// the farm every Execute put each of its cached Directories and files to the pack index): the
/// first walk asks about each of its four blobs once, and the second walk of the same tree
/// skips it from the root, asking the index nothing and reading nothing.
#[tokio::test]
async fn a_second_walk_of_a_tree_in_the_packs_asks_the_index_nothing() {
    let stored = stored();
    let b = blob("file b");
    let tree = two_level("file a", &[&b]);
    let root = tree[0].0;
    stored.packs().put(tree).await.expect("packed");
    let backend = Backend::stored(Arc::clone(&stored));
    let (before, _) = asked_and_read(&stored);

    let first = backend.walk_inputs(root).await.expect("walked");
    let (asked, read) = asked_and_read(&stored);
    let second = backend.walk_inputs(root).await.expect("walked");

    assert!(first.absent.is_empty(), "{:?}", first.absent);
    assert_eq!(first.dirs_skipped, 0);
    assert_eq!(
        asked - before,
        4,
        "two files and two Directories, each asked about once"
    );
    assert!(second.absent.is_empty(), "{:?}", second.absent);
    assert_eq!(second.dirs_skipped, 1, "the root, with everything under it");
    assert_eq!(
        (second.dirs_read, second.dirs_present_checked),
        (0, 0),
        "no Directory read or asked about"
    );
    assert_eq!((second.files_checked, second.file_asks), (0, 0));
    assert_eq!(
        asked_and_read(&stored),
        (asked, read),
        "nothing asked or read"
    );
}

/// Catches a remembered tree hiding a change from the store: after a tree is found held, a
/// second tree that keeps its Directory `shared` and names a new Directory with one new file
/// (as a changed file does: every Directory above it is new) is walked, and exactly the new
/// file and the two new Directories are asked about; the shared Directory, with its file, is
/// skipped.
#[tokio::test]
async fn a_changed_file_is_asked_about_and_an_unchanged_subtree_is_not() {
    let stored = stored();
    let (b, c) = (blob("file b"), blob("file c"));
    let shared = dir(&[b.0], &[]);
    let old_root = dir(&[], &[shared.0]);
    let changed = dir(&[c.0], &[]);
    let new_root = dir(&[], &[shared.0, changed.0]);
    let first_tree = vec![old_root.clone(), shared.clone(), b.clone()];
    stored.packs().put(first_tree).await.expect("packed");
    let backend = Backend::stored(Arc::clone(&stored));
    let first = backend.walk_inputs(old_root.0).await.expect("walked");
    assert!(first.absent.is_empty(), "{:?}", first.absent);
    let second_tree = vec![new_root.clone(), changed.clone(), c.clone()];
    stored.packs().put(second_tree).await.expect("packed");
    let (asked, _) = asked_and_read(&stored);

    let second = backend.walk_inputs(new_root.0).await.expect("walked");

    assert!(second.absent.is_empty(), "{:?}", second.absent);
    assert_eq!(second.dirs_skipped, 1, "the shared Directory");
    assert_eq!(second.files_checked, 1, "the changed file");
    assert_eq!(second.dirs_present_checked, 2, "the two new Directories");
    assert_eq!(asked_and_read(&stored).0, asked + 3);
}

/// Catches a remembered subtree making an Execute skip a file the store does not hold: the
/// second tree shares a Directory with one found held, and names a file nobody uploaded, which
/// is reported absent as the only absent blob; the absence is a loss as far as the memory can
/// tell, so a third walk of the first tree, held throughout, asks about it again.
#[tokio::test]
async fn a_file_not_uploaded_is_absent_beside_a_remembered_subtree_and_forgets_it() {
    let stored = stored();
    let (b, gone) = (blob("file b"), blob("never uploaded"));
    let shared = dir(&[b.0], &[]);
    let old_root = dir(&[], &[shared.0]);
    let new_root = dir(&[gone.0], &[shared.0]);
    let packed = vec![
        old_root.clone(),
        shared.clone(),
        b.clone(),
        new_root.clone(),
    ];
    stored.packs().put(packed).await.expect("packed");
    let backend = Backend::stored(Arc::clone(&stored));
    backend.walk_inputs(old_root.0).await.expect("walked");

    let second = backend.walk_inputs(new_root.0).await.expect("walked");
    let third = backend.walk_inputs(old_root.0).await.expect("walked");

    assert_eq!(second.absent, vec![gone.0]);
    assert_eq!(second.dirs_skipped, 1, "the shared Directory");
    assert!(third.absent.is_empty(), "{:?}", third.absent);
    assert_eq!(third.dirs_skipped, 0, "nothing believed after an absence");
    assert_eq!(third.files_checked, 1);
}

/// Catches a memory that outlives a loss the packs found: once a tree is found held, a pack the
/// store no longer has is reported to the packs (as a read of it would), and the next walk of
/// the tree asks the index about all of it again.
#[tokio::test]
async fn a_loss_the_packs_find_makes_the_next_walk_ask_again() {
    let stored = stored();
    let b = blob("file b");
    let tree = two_level("file a", &[&b]);
    let root = tree[0].0;
    stored.packs().put(tree).await.expect("packed");
    let backend = Backend::stored(Arc::clone(&stored));
    backend.walk_inputs(root).await.expect("walked");
    let before = stored.packs().losses();
    let (asked, _) = asked_and_read(&stored);

    let _noted = stored
        .packs()
        .absent("cas/b/a-pack-the-store-lost", 0)
        .await;
    let again = backend.walk_inputs(root).await.expect("walked");

    assert_eq!(stored.packs().losses(), before + 1);
    assert!(again.absent.is_empty(), "{:?}", again.absent);
    assert_eq!(again.dirs_skipped, 0);
    assert_eq!(
        (again.files_checked, again.dirs_present_checked),
        (2, 2),
        "every blob asked about again"
    );
    assert_eq!(asked_and_read(&stored).0, asked + 4);
}
