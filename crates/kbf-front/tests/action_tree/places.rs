//! The place memo: a blob found by one tree is not looked up for the next, for the time the
//! manifest memo keeps an entry, by the writer's clock; a blob written whole is remembered only
//! once its write succeeded; the memo's bound; and `forget`.
//!
//! The roots of these tests hold the same files under different symlinks, so that the manifest
//! memo never answers for them: what the farm is asked is what the place memo did not know.

use kbf_front::action_tree::MEMO_TTL;

use super::listing::list;
use super::*;

/// The input root holding `files` and the symlink `name`: roots of one set of files with
/// different names are different Directories.
async fn root_with(rig: &Rig, files: &[Digest], name: &str) -> Digest {
    let nodes = files
        .iter()
        .enumerate()
        .map(|(at, digest)| file(&format!("f{at}"), digest, false))
        .collect();
    rig.store(&directory(nodes, vec![], vec![link(name, "t")]))
        .await
}

/// A blob in a pack and a blob only the CAS holds (which the first tree writes whole).
async fn blobs(rig: &Rig) -> (Digest, Digest) {
    let packed = rig.farm.packed("a packed blob", "cas/f/w/p0", 0).await;
    let loose = rig.farm.cas("a blob only the CAS holds").await;
    (packed, loose)
}

/// Asserts that a second tree naming the files of a first, within [`MEMO_TTL`], makes no ask of
/// the pack index for them and no presence check, writes nothing of the blob written whole, and
/// still names the packed blob by its span and the whole one by its object.
#[tokio::test]
async fn a_second_tree_of_the_same_files_asks_for_none_of_them() {
    let rig = Rig::new().await;
    let (packed, loose) = blobs(&rig).await;
    let first = root_with(&rig, &[packed, loose], "one").await;
    let second = root_with(&rig, &[packed, loose], "two").await;
    rig.write(&first).await.expect("a tree");
    assert_eq!(rig.farm.puts_of(&loose.hash_hex()), 1, "written whole");
    rig.farm.clear_records();
    let action = rig.action(&second).await;
    let tree = rig.write(&second).await.expect("a tree");
    assert_eq!(rig.farm.asked(), BTreeSet::from([action]));
    assert!(rig.farm.state().heads.is_empty());
    assert_eq!(rig.farm.puts_of(&loose.hash_hex()), 0, "not written again");
    let entries = rig.farm.flatten(&tree).expect("the tree reads");
    let kind = |name: &str| {
        let entry = entries.iter().find(|entry| &*entry.name == name);
        &entry.expect("an entry").kind
    };
    assert!(matches!(
        kind("f0"),
        tierstore::EntryKind::File {
            source: Some(_),
            ..
        }
    ));
    assert!(matches!(
        kind("f1"),
        tierstore::EntryKind::File { source: None, .. }
    ));
}

/// Asserts that an entry is good until [`MEMO_TTL`] after it was made, by the writer's clock,
/// and a miss from then: the lookups are made again.
#[tokio::test]
async fn a_place_is_good_for_its_time_and_no_longer() {
    let rig = Rig::new().await;
    let (packed, loose) = blobs(&rig).await;
    let files = [packed, loose];
    rig.write(&root_with(&rig, &files, "one").await)
        .await
        .expect("a tree");
    rig.advance(MEMO_TTL - Duration::from_nanos(1));
    rig.farm.clear_records();
    rig.write(&root_with(&rig, &files, "two").await)
        .await
        .expect("a tree");
    assert!(!rig.farm.asked().contains(&packed), "still good");
    assert!(rig.farm.state().heads.is_empty());
    rig.advance(Duration::from_nanos(1));
    rig.farm.clear_records();
    rig.write(&root_with(&rig, &files, "three").await)
        .await
        .expect("a tree");
    assert!(rig.farm.asked().contains(&packed), "expired, asked again");
    assert_eq!(rig.farm.state().heads, [loose], "and checked again");
}

/// Asserts that a blob whose whole write failed is not remembered: the next tree finds the
/// store without it, writes it, and then it is remembered; the blob found in a pack before the
/// failure is.
#[tokio::test]
async fn a_blob_whose_write_failed_is_asked_about_again() {
    let rig = Rig::new().await;
    let (packed, loose) = blobs(&rig).await;
    let files = [packed, loose];
    let key = whole_key(&loose.hash_hex());
    rig.farm.state().faults.put = Some(key.clone());
    let failed = rig.write(&root_with(&rig, &files, "one").await).await;
    assert!(matches!(failed, Err(NoTree::Put { .. })), "{failed:?}");
    rig.farm.state().faults.put = None;
    rig.farm.clear_records();
    rig.write(&root_with(&rig, &files, "two").await)
        .await
        .expect("a tree");
    assert!(!rig.farm.asked().contains(&packed), "found before");
    assert_eq!(rig.farm.state().heads, [loose], "checked again");
    assert_eq!(rig.farm.puts_of(&loose.hash_hex()), 1, "written now");
    rig.farm.clear_records();
    rig.write(&root_with(&rig, &files, "three").await)
        .await
        .expect("a tree");
    assert!(rig.farm.state().heads.is_empty(), "now remembered");
}

/// Asserts that a lookup that failed remembers nothing: the next tree asks for the blob.
#[tokio::test]
async fn a_failed_lookup_remembers_nothing() {
    let rig = Rig::new().await;
    let (packed, loose) = blobs(&rig).await;
    let files = [packed, loose];
    rig.farm.state().faults.head = Fault::Fail;
    let failed = rig.write(&root_with(&rig, &files, "one").await).await;
    assert!(matches!(failed, Err(NoTree::Lookup { .. })), "{failed:?}");
    rig.farm.state().faults.head = Fault::None;
    rig.farm.clear_records();
    rig.write(&root_with(&rig, &files, "two").await)
        .await
        .expect("a tree");
    assert_eq!(rig.farm.state().heads, [loose]);
}

/// Asserts that the memo drops its oldest entries at its bound: with room for two places, the
/// tree of the Action and the Command (the only blobs of a root with no file) remembers both,
/// a second Action's blob pushes out the one remembered first, and only that one is asked for
/// again.
#[tokio::test]
async fn the_place_memo_drops_its_oldest_entry_at_its_bound() {
    let mut rig = Rig::new().await;
    rig.writer = rig.writer.with_place_entries(2);
    let first = root_with(&rig, &[], "one").await;
    let second = root_with(&rig, &[], "two").await;
    let action = rig.action(&first).await;
    rig.write(&first).await.expect("a tree");
    rig.write(&second).await.expect("a tree");
    // The two blobs of the first tree went in together, in digest order.
    let oldest = action.min(rig.command);
    rig.farm.clear_records();
    rig.write(&first).await.expect("a tree");
    assert_eq!(rig.farm.asked(), BTreeSet::from([oldest]));
}

/// Asserts that `forget` drops the places with the manifests: the next tree of the same files
/// asks for them again.
#[tokio::test]
async fn forget_drops_the_places() {
    let rig = Rig::new().await;
    let (packed, loose) = blobs(&rig).await;
    let files = [packed, loose];
    let first = root_with(&rig, &files, "one").await;
    rig.write(&first).await.expect("a tree");
    rig.writer.forget(&first);
    rig.farm.clear_records();
    rig.write(&root_with(&rig, &files, "two").await)
        .await
        .expect("a tree");
    assert!(rig.farm.asked().contains(&packed));
    assert_eq!(rig.farm.state().heads, [loose]);
}

/// Asserts that a tree list is answered from the place memo as a tree is: a second list of the
/// same files asks for none of them and checks none, and a blob the first list wrote whole is
/// not written again.
#[tokio::test]
async fn a_second_list_of_the_same_files_asks_for_none_of_them() {
    let rig = Rig::new().await;
    let (packed, loose) = blobs(&rig).await;
    let files = [packed, loose];
    let first = root_with(&rig, &files, "one").await;
    let second = root_with(&rig, &files, "two").await;
    list(&rig, &first).await.expect("a list");
    assert_eq!(rig.farm.puts_of(&loose.hash_hex()), 1, "written whole");
    rig.farm.clear_records();
    let action = rig.action(&second).await;
    list(&rig, &second).await.expect("a list");
    assert_eq!(rig.farm.asked(), BTreeSet::from([action]));
    assert!(rig.farm.state().heads.is_empty());
    assert_eq!(rig.farm.puts_of(&loose.hash_hex()), 0);
}
