//! The name of an action tree, handed out before its writes (`TreeWriter::write_named`,
//! `TreeWriter::write_listing_named`): it comes before any blob or manifest is written, it is
//! the tree the writes end with, a tree the writer refuses or cannot look up is never named,
//! a write that fails after the name was handed out still fails the write, and a write checked
//! against the store is named the same way.

use kbf_front::action_tree::Listing;

use super::listing::listed_base;
use super::*;

/// The input root holding one file, `blob`.
async fn one_file(rig: &Rig, blob: &Digest) -> Digest {
    let nodes = vec![file("f0", blob, false)];
    rig.store(&directory(nodes, vec![], vec![])).await
}

/// What `named` saw: the name, and how many writes the store had been asked for by then.
type Seen<T> = Arc<Mutex<Option<(T, usize)>>>;

fn seen<T>() -> Seen<T> {
    Arc::new(Mutex::new(None))
}

fn take<T>(seen: &Seen<T>) -> Option<(T, usize)> {
    seen.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// Catches: a name handed out after the writes began, or one that is not the tree written: the
/// manifests' tree is named while the store has been asked for no write, and the tree the write
/// returns, with its blob, its manifests and its root stored, is that name.
#[tokio::test]
async fn a_tree_is_named_before_its_first_write_and_is_the_tree_written() {
    let rig = Rig::new().await;
    let blob = rig.farm.cas("only in a storage node's write tier").await;
    let root = one_file(&rig, &blob).await;
    let kept = rig.kept(&root).await;
    let at_name = seen::<ActionTree>();
    let into = Arc::clone(&at_name);
    let farm = Arc::clone(&rig.farm);
    let written = (rig.writer)
        .write_named(&kept, &rig.base.tree, move |tree| {
            let puts = farm.puts().len();
            *into.lock().unwrap_or_else(PoisonError::into_inner) = Some((*tree, puts));
        })
        .await
        .expect("a tree");
    let (named, puts) = take(&at_name).expect("the tree was named");
    assert_eq!(puts, 0, "no write before the name");
    assert_eq!(named, written);
    let root_key = whole_key(&written.digest().to_string());
    assert_eq!(
        rig.farm.puts().last(),
        Some(&root_key),
        "the root is written last"
    );
    assert!(rig.farm.object(&whole_key(&blob.hash_hex())).is_some());
}

/// Catches: a name handed out for a tree the writer could not finish: a lookup that fails
/// leaves `named` uncalled and nothing written.
#[tokio::test]
async fn a_tree_whose_lookups_fail_is_never_named() {
    let rig = Rig::new().await;
    let blob = rig.farm.cas("unpacked").await;
    rig.farm.state().faults.index = Fault::Fail;
    let root = one_file(&rig, &blob).await;
    let kept = rig.kept(&root).await;
    let at_name = seen::<ActionTree>();
    let into = Arc::clone(&at_name);
    let refused = (rig.writer)
        .write_named(&kept, &rig.base.tree, move |tree| {
            *into.lock().unwrap_or_else(PoisonError::into_inner) = Some((*tree, 0));
        })
        .await
        .expect_err("no tree");
    assert!(matches!(refused, NoTree::Lookup { .. }), "{refused:?}");
    assert_eq!(take(&at_name), None, "never named");
    assert!(rig.farm.puts().is_empty());
}

/// Catches: a write that fails after the name was handed out taken for a stored tree: the tree
/// is named, the write of its blob is refused, and the write ends in that refusal with no
/// manifest written.
#[tokio::test]
async fn a_named_tree_whose_write_fails_is_not_written() {
    let rig = Rig::new().await;
    let blob = rig.farm.cas("refused by the store").await;
    let blob_key = whole_key(&blob.hash_hex());
    rig.farm.state().faults.put = Some(blob_key.clone());
    let root = one_file(&rig, &blob).await;
    let kept = rig.kept(&root).await;
    let at_name = seen::<ActionTree>();
    let into = Arc::clone(&at_name);
    let refused = (rig.writer)
        .write_named(&kept, &rig.base.tree, move |tree| {
            *into.lock().unwrap_or_else(PoisonError::into_inner) = Some((*tree, 0));
        })
        .await
        .expect_err("no tree");
    assert!(
        matches!(&refused, NoTree::Put { key, .. } if *key == blob_key),
        "{refused:?}"
    );
    assert!(take(&at_name).is_some(), "named before the write failed");
    assert_eq!(rig.farm.puts(), [blob_key], "no manifest is written");
}

/// Catches: a list named after its writes began, or not the list written: the list's key is
/// handed out while the store has been asked for no write, the list is the last write, and the
/// listing returned is the one named.
#[tokio::test]
async fn a_list_is_named_before_its_first_write_and_is_the_list_written() {
    let rig = Rig::new().await;
    let blob = rig.farm.cas("a file of the input root").await;
    let root = one_file(&rig, &blob).await;
    let kept = rig.kept(&root).await;
    let base = listed_base(&rig);
    rig.farm.clear_records();
    let at_name = seen::<Listing>();
    let into = Arc::clone(&at_name);
    let farm = Arc::clone(&rig.farm);
    let written = (rig.writer)
        .write_listing_named(&kept, &base, move |listing| {
            let puts = farm.puts().len();
            *into.lock().unwrap_or_else(PoisonError::into_inner) = Some((listing.clone(), puts));
        })
        .await
        .expect("a list");
    let (named, puts) = take(&at_name).expect("the list was named");
    assert_eq!(puts, 0, "no write before the name");
    assert_eq!(named, written);
    assert_eq!(
        rig.farm.puts().last().map(String::as_str),
        Some(written.key())
    );
    assert_eq!(rig.farm.puts().len(), 2, "the blob, then the list");
}

/// Catches: a list that is refused, or whose write fails, handled as one stored: a list whose
/// lookups fail is never named, and a named list whose write the store refuses ends the write
/// in that refusal.
#[tokio::test]
async fn a_list_is_named_only_when_listed_and_a_failed_write_still_fails() {
    let rig = Rig::new().await;
    let blob = rig.farm.cas("a file of the input root").await;
    let root = one_file(&rig, &blob).await;
    let kept = rig.kept(&root).await;
    let base = listed_base(&rig);
    rig.farm.state().faults.index = Fault::Fail;
    let at_name = seen::<Listing>();
    let into = Arc::clone(&at_name);
    let refused = (rig.writer)
        .write_listing_named(&kept, &base, move |listing| {
            *into.lock().unwrap_or_else(PoisonError::into_inner) = Some((listing.clone(), 0));
        })
        .await
        .expect_err("no list");
    assert!(matches!(refused, NoTree::Lookup { .. }), "{refused:?}");
    assert_eq!(take(&at_name), None, "never named");
    rig.farm.state().faults.index = Fault::None;
    let into = Arc::clone(&at_name);
    let farm = Arc::clone(&rig.farm);
    let refused = (rig.writer)
        .write_listing_named(&kept, &base, move |listing| {
            farm.state().faults.put = Some(listing.key().to_owned());
            *into.lock().unwrap_or_else(PoisonError::into_inner) = Some((listing.clone(), 0));
        })
        .await
        .expect_err("no list");
    let (named, _) = take(&at_name).expect("named");
    assert!(
        matches!(&refused, NoTree::Put { key, .. } if key == named.key()),
        "{refused:?}"
    );
}

/// Catches a checked write that is named after its writes began, or not named the tree it
/// writes: a tree and a list written checked against the store are named while the store has
/// been asked for no write, and each is the one written.
#[tokio::test]
async fn a_checked_tree_and_list_are_named_before_their_first_write() {
    let rig = Rig::new().await;
    let blob = rig.farm.cas("a file of the input root").await;
    let root = one_file(&rig, &blob).await;
    let kept = rig.kept(&root).await;
    let at_name = seen::<ActionTree>();
    let into = Arc::clone(&at_name);
    let farm = Arc::clone(&rig.farm);
    let written = (rig.writer)
        .write_checked_named(&kept, &rig.base.tree, move |tree| {
            let puts = farm.puts().len();
            *into.lock().unwrap_or_else(PoisonError::into_inner) = Some((*tree, puts));
        })
        .await
        .expect("a tree");
    assert_eq!(take(&at_name), Some((written, 0)));

    let base = listed_base(&rig);
    rig.farm.clear_records();
    let at_list = seen::<Listing>();
    let into = Arc::clone(&at_list);
    let farm = Arc::clone(&rig.farm);
    let listed = (rig.writer)
        .write_listing_checked_named(&kept, &base, move |listing| {
            let puts = farm.puts().len();
            *into.lock().unwrap_or_else(PoisonError::into_inner) = Some((listing.clone(), puts));
        })
        .await
        .expect("a list");
    assert_eq!(take(&at_list), Some((listed, 0)));
}
