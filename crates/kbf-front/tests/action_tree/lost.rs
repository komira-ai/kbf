//! A manifest or a pack the store lost while the writer's memos, or the pack index, still name it:
//! the memo entries that name a lost manifest and those above them are dropped, and a checked
//! write rebuilds every manifest and asks the store after every pack its tree names, the index
//! forgetting each the store lacks.

use super::listing::listed_base;
use super::memo::{holding, holding_manifest, link_manifest, link_root};
use super::*;

/// The SHA-256 of the manifest whose digest is `hex`.
fn sha(hex: &str) -> [u8; 32] {
    *TreeDigest::from_hex(hex).expect("a digest").as_bytes()
}

/// The manifest digest of a Directory holding only the file `f`, `blob` (5 bytes) at `offset` of
/// the pack `key`, and that manifest's length.
fn file_manifest(blob: &Digest, key: &str, offset: u64) -> (String, usize) {
    let text = format!(
        r#"{{"v":1,"children":[{{"name":"f","kind":"file","sha256":"{}","size":5,"exec":false,"source":{{"key":"{key}","offset":{offset}}}}}]}}"#,
        blob.hash_hex()
    );
    (TreeDigest::of(text.as_bytes()).to_string(), text.len())
}

/// A root holding, as `s`, a Directory of the one file `f`: `blob`.
async fn root_over(rig: &Rig, blob: &Digest) -> Digest {
    let sub = rig
        .store(&directory(vec![file("f", blob, false)], vec![], vec![]))
        .await;
    holding(rig, "top", &sub).await
}

/// The packs the store was asked after, by key.
fn checked_keys(rig: &Rig) -> BTreeSet<String> {
    let state = rig.farm.state();
    state.checks.iter().map(|(key, _)| key.clone()).collect()
}

/// Catches: a lost manifest whose memo entry is still a hit, so the tree that names it is not
/// written again; an entry above it (whose manifest names the lost one) still a hit; the place
/// memo still answering, so the blobs below are not looked up again; or other entries dropped
/// with them. A manifest no entry names drops nothing and says so.
#[tokio::test]
async fn a_lost_manifest_drops_its_memo_entry_and_those_above_it() {
    let rig = Rig::new().await;
    let blob = rig.farm.packed("below", "cas/f/w/p0", 0).await;
    let root = root_over(&rig, &blob).await;
    let other = link_root(&rig, "other").await;
    rig.write(&root).await.expect("a tree");
    rig.write(&other).await.expect("a tree");
    let (sub_hex, sub_len) = file_manifest(&blob, "cas/f/w/p0", 0);
    let root_hex = holding_manifest("top", &sub_hex, sub_len);

    assert!(!rig.writer.lost_manifest(&[7; 32]), "no entry names it");
    assert!(rig.writer.lost_manifest(&sha(&sub_hex)));
    rig.farm.clear_records();
    rig.write(&root).await.expect("a tree");
    rig.write(&other).await.expect("a tree");

    assert_eq!(
        rig.farm.puts_of(&sub_hex),
        1,
        "the lost manifest is written again"
    );
    assert_eq!(rig.farm.puts_of(&root_hex), 1, "the manifest above it too");
    assert!(
        rig.farm.asked().contains(&blob),
        "its blob is looked up again"
    );
    assert_eq!(
        rig.farm.puts_of(&link_manifest("other")),
        0,
        "another root's entry is kept"
    );
}

/// Catches: a pack a job found absent whose memo entries are still hits (the manifest that names a
/// span of it, and the one above), or whose places are still remembered, so the next tree names
/// it again without asking the index; or other entries or places dropped with them. A pack no
/// memo names drops nothing and says so.
#[tokio::test]
async fn an_absent_pack_drops_the_memo_entries_and_places_that_name_it() {
    let rig = Rig::new().await;
    let gone = rig.farm.packed("gone!", "cas/f/w/p0", 0).await;
    let kept = rig.farm.packed("kept!", "cas/f/w/p1", 0).await;
    let sub = rig
        .store(&directory(
            vec![file("a", &gone, false), file("b", &kept, false)],
            vec![],
            vec![],
        ))
        .await;
    let root = holding(&rig, "r", &sub).await;
    let other = link_root(&rig, "other").await;
    rig.write(&root).await.expect("a tree");
    rig.write(&other).await.expect("a tree");

    assert!(!rig.writer.lost_pack("cas/f/w/none"), "no entry names it");
    assert!(rig.writer.lost_pack("cas/f/w/p0"));
    assert!(!rig.writer.lost_pack("cas/f/w/p0"), "dropped already");
    rig.farm.clear_records();
    rig.write(&root).await.expect("a tree");
    rig.write(&other).await.expect("a tree");

    let asked = rig.farm.asked();
    assert!(asked.contains(&gone), "its blob is looked up again");
    assert!(!asked.contains(&kept), "a place in another pack is kept");
    assert_eq!(
        rig.farm.puts_of(&link_manifest("other")),
        0,
        "another root's entry is kept"
    );
}

/// Catches: a checked write that takes a memo hit or a remembered place, or names a pack it did
/// not ask the store after: every manifest is written again, every blob looked up, and every pack
/// the tree names asked after once, with the end of the furthest blob in it.
#[tokio::test]
async fn a_checked_write_rebuilds_every_manifest_and_asks_after_every_pack() {
    let rig = Rig::new().await;
    let blob = rig.farm.packed("below", "cas/f/w/p0", 0).await;
    let root = root_over(&rig, &blob).await;
    rig.write(&root).await.expect("a tree");
    let (sub_hex, _) = file_manifest(&blob, "cas/f/w/p0", 0);
    let kept = rig.kept(&root).await;
    rig.farm.clear_records();

    let written = rig.writer.write_checked(&kept, &rig.base.tree).await;

    let tree = written.expect("a tree");
    assert_eq!(rig.farm.puts_of(&sub_hex), 1, "no memo hit");
    assert!(rig.farm.asked().contains(&blob), "no remembered place");
    let checks = rig.farm.state().checks.clone();
    assert_eq!(checks.len(), 2, "each pack once: {checks:?}");
    assert!(checks.contains(&("cas/f/w/p0".to_owned(), 5)));
    assert_eq!(
        checked_keys(&rig),
        ["cas/f/w/p0", "cas/f/w/p9"].map(str::to_owned).into()
    );
    assert!(rig.farm.flatten(&tree).is_ok());
}

/// Catches: a tree written over a pack the store lost, or the index left naming it: the checked
/// write is refused naming exactly the blobs of the lost pack (`why` `missing`), the index
/// forgets that pack and keeps the others, and the next checked write places the blob anew
/// (here written whole from the CAS that still has it) and succeeds.
#[tokio::test]
async fn a_checked_write_over_a_lost_pack_is_refused_and_the_pack_forgotten() {
    let rig = Rig::new().await;
    let lost = rig.farm.packed("lost!", "cas/f/w/p0", 0).await;
    let kept_blob = rig.farm.packed("kept!", "cas/f/w/p1", 0).await;
    let sub = rig
        .store(&directory(
            vec![file("a", &lost, false), file("b", &kept_blob, false)],
            vec![],
            vec![],
        ))
        .await;
    let root = holding(&rig, "r", &sub).await;
    let kept = rig.kept(&root).await;
    rig.writer
        .write(&kept, &rig.base.tree)
        .await
        .expect("a tree");
    rig.farm.state().lost.insert("cas/f/w/p0".to_owned());

    let refused = rig.writer.write_checked(&kept, &rig.base.tree).await;

    let refused = refused.expect_err("lost");
    assert_eq!(refused, NoTree::Lost { blobs: vec![lost] });
    assert_eq!(refused.why(), "missing");
    assert_eq!(
        refused.to_string(),
        "the store lost the packs of 1 input blobs, which are missing now"
    );
    let spans = rig.farm.state().spans.clone();
    assert!(!spans.contains_key(&lost), "the lost pack is forgotten");
    assert!(spans.contains_key(&kept_blob), "the other pack is kept");
    rig.farm.clear_records();
    rig.writer
        .write(&kept, &rig.base.tree)
        .await
        .expect("a tree");
    assert!(
        rig.farm.asked().contains(&kept_blob),
        "the memos, which placed blobs in the lost pack, were dropped"
    );

    let again = rig.writer.write_checked(&kept, &rig.base.tree).await;
    assert!(again.is_ok(), "{again:?}");
    assert!(rig.farm.puts().contains(&whole_key(&lost.hash_hex())));
}

/// Catches: a checked tree list written over a pack the store lost: it is refused as the
/// manifests are, and the pack forgotten.
#[tokio::test]
async fn a_checked_listing_over_a_lost_pack_is_refused() {
    let rig = Rig::new().await;
    let lost = rig.farm.packed("lost!", "cas/f/w/p0", 0).await;
    let root = root_over(&rig, &lost).await;
    let kept = rig.kept(&root).await;
    let base = listed_base(&rig);
    rig.writer
        .write_listing(&kept, &base)
        .await
        .expect("a list");
    rig.farm.state().lost.insert("cas/f/w/p0".to_owned());

    let refused = rig.writer.write_listing_checked(&kept, &base).await;

    assert_eq!(
        refused.expect_err("lost"),
        NoTree::Lost { blobs: vec![lost] }
    );
    assert!(checked_keys(&rig).contains("cas/f/w/p0"));
    assert!(!rig.farm.state().spans.contains_key(&lost));
}

/// Catches: a check the store could not answer taken for a loss, or for a pack that is held:
/// the write fails as a lookup (`why` `store`, run again) and the index keeps every row.
#[tokio::test]
async fn a_check_the_store_cannot_answer_fails_the_write_and_forgets_nothing() {
    let rig = Rig::new().await;
    let blob = rig.farm.packed("below", "cas/f/w/p0", 0).await;
    let root = root_over(&rig, &blob).await;
    let kept = rig.kept(&root).await;
    rig.farm.state().faults.check = Fault::Fail;
    rig.farm.state().lost.insert("cas/f/w/p0".to_owned());

    let failed = rig.writer.write_checked(&kept, &rig.base.tree).await;

    let failed = failed.expect_err("unanswered");
    assert!(matches!(failed, NoTree::Lookup { .. }), "{failed:?}");
    assert_eq!(failed.why(), "store");
    assert!(rig.farm.state().spans.contains_key(&blob));
}

/// A source of blobs that holds none and has no packs: what a [`BlobSource`] that does not
/// override the check of a pack answers.
struct Bare;

impl BlobSource for Bare {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        Box::pin(async move { Err(StorageError::NotFound(*digest)) })
    }

    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>> {
        Box::pin(async move { Err(StorageError::NotFound(*digest)) })
    }
}

/// Catches a source with no packs taking a pack it cannot see for a lost one: a source that does
/// not override the check, and a CAS with no packs, forget nothing.
#[tokio::test]
async fn a_source_without_packs_forgets_nothing() {
    let rig = Rig::new().await;
    assert!(!Bare.forget_lost("cas/f/w/p0", 5).await.expect("answered"));
    let cas = &rig.farm.backend;
    assert!(!cas.forget_lost("cas/f/w/p0", 5).await.expect("answered"));
}
