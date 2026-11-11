//! The memo: a shared subtree built and written once, its bound, its expiry on the writer's
//! clock (a parent no later than its earliest child), and an input root forgotten.

use kbf_front::action_tree::MEMO_TTL;

use super::*;

/// The manifest digest of the Directory listing only the symlink `name` (to `t`).
fn link_manifest(name: &str) -> String {
    let text =
        format!(r#"{{"v":1,"children":[{{"name":"{name}","kind":"symlink","target":"t"}}]}}"#);
    TreeDigest::of(text.as_bytes()).to_string()
}

/// A Directory listing only the symlink `name` (to `t`).
async fn link_root(rig: &Rig, name: &str) -> Digest {
    rig.store(&directory(vec![], vec![], vec![link(name, "t")]))
        .await
}

/// A Directory holding the subdirectory `sub` as `name`, and the symlink `name` to `t`.
async fn holding(rig: &Rig, name: &str, sub: &Digest) -> Digest {
    rig.store(&directory(
        vec![],
        vec![subdir("s", sub)],
        vec![link(name, "t")],
    ))
    .await
}

/// The manifest digest of [`holding`]'s Directory, its subdirectory's manifest `sub` (whose
/// text is `sub_text`).
fn holding_manifest(name: &str, sub: &str, sub_len: usize) -> String {
    let text = format!(
        r#"{{"v":1,"children":[{{"name":"s","kind":"dir","sha256":"{sub}","size":{sub_len}}},{{"name":"{name}","kind":"symlink","target":"t"}}]}}"#
    );
    TreeDigest::of(text.as_bytes()).to_string()
}

/// The text of [`link_root`]'s manifest.
fn link_text(name: &str) -> String {
    format!(r#"{{"v":1,"children":[{{"name":"{name}","kind":"symlink","target":"t"}}]}}"#)
}

/// Catches: a subtree that two parents of one tree name built, looked up or written twice.
#[tokio::test]
async fn a_shared_subtree_is_built_and_written_once() {
    let rig = Rig::new().await;
    let blob = rig.farm.packed("deep file", "cas/f/w/p0", 0).await;
    let deep = rig
        .store(&directory(vec![file("f", &blob, false)], vec![], vec![]))
        .await;
    let one = rig
        .store(&directory(vec![], vec![subdir("deep", &deep)], vec![]))
        .await;
    let two = rig
        .store(&directory(
            vec![],
            vec![subdir("deep", &deep), subdir("x", &one)],
            vec![],
        ))
        .await;
    let root = rig
        .store(&directory(
            vec![],
            vec![subdir("one", &one), subdir("two", &two)],
            vec![],
        ))
        .await;
    rig.write(&root).await.expect("a tree");
    let deep_text = format!(
        r#"{{"v":1,"children":[{{"name":"f","kind":"file","sha256":"{}","size":9,"exec":false,"source":{{"key":"cas/f/w/p0","offset":0}}}}]}}"#,
        blob.hash_hex()
    );
    assert_eq!(
        rig.farm
            .puts_of(&TreeDigest::of(deep_text.as_bytes()).to_string()),
        1
    );
    let asked = rig
        .farm
        .state()
        .asks
        .iter()
        .flatten()
        .filter(|d| **d == blob)
        .count();
    assert_eq!(asked, 1);
}

/// Catches: a subtree two input roots share (a toolchain) built again for the second: its
/// manifest is not written again and its files are not looked up again, and the second tree
/// names it.
#[tokio::test]
async fn a_subtree_two_roots_share_is_built_once() {
    let rig = Rig::new().await;
    let tool = rig.farm.packed("a toolchain file", "cas/f/w/p0", 0).await;
    let shared = rig
        .store(&directory(vec![file("cc", &tool, true)], vec![], vec![]))
        .await;
    let first = holding(&rig, "one", &shared).await;
    let second = holding(&rig, "two", &shared).await;
    rig.write(&first).await.expect("a tree");
    rig.farm.clear_records();
    let written = rig.write(&second).await.expect("a tree");
    assert!(
        !rig.farm.asked().contains(&tool),
        "the shared files are not looked up"
    );
    assert_eq!(
        rig.farm.puts().len(),
        3,
        "the second root, `.kbf` and the root"
    );
    let entries = rig.farm.flatten(&written).expect("the tree reads");
    assert!(entries.iter().any(|entry| &*entry.name == "cc"));
}

/// Catches: a memo that grows past its bound, or drops other than its oldest entry first.
#[tokio::test]
async fn the_memo_drops_its_oldest_entry_at_its_bound() {
    let rig = Rig::with_memo(Some(2)).await;
    let roots = [
        link_root(&rig, "a").await,
        link_root(&rig, "b").await,
        link_root(&rig, "c").await,
    ];
    for root in &roots {
        rig.write(root).await.expect("a tree");
    }
    rig.farm.clear_records();
    rig.write(&roots[1]).await.expect("a tree");
    assert_eq!(rig.farm.puts_of(&link_manifest("b")), 0, "b is remembered");
    rig.write(&roots[0]).await.expect("a tree");
    assert_eq!(rig.farm.puts_of(&link_manifest("a")), 1, "a went first");
}

/// Catches: an entry used past its time, or dropped before it: it is a hit until
/// [`MEMO_TTL`] after it was made, by the writer's clock, and a miss from then.
#[tokio::test]
async fn an_entry_is_good_for_its_time_and_no_longer() {
    let rig = Rig::new().await;
    let root = link_root(&rig, "a").await;
    rig.write(&root).await.expect("a tree");
    rig.advance(MEMO_TTL - Duration::from_nanos(1));
    rig.farm.clear_records();
    rig.write(&root).await.expect("a tree");
    assert_eq!(rig.farm.puts_of(&link_manifest("a")), 0, "still good");
    rig.advance(Duration::from_nanos(1));
    rig.write(&root).await.expect("a tree");
    assert_eq!(
        rig.farm.puts_of(&link_manifest("a")),
        1,
        "expired, built again"
    );
}

/// Catches: a parent remembered for its own full time over a child that expires sooner, which
/// would name the child's places past their time: a parent built over a remembered child
/// expires with that child.
#[tokio::test]
async fn a_parent_expires_no_later_than_its_earliest_child() {
    let rig = Rig::new().await;
    let child = link_root(&rig, "child").await;
    let first = holding(&rig, "first", &child).await;
    let second = holding(&rig, "second", &child).await;
    rig.write(&first).await.expect("a tree");
    let half = MEMO_TTL / 2;
    rig.advance(half);
    rig.write(&second).await.expect("a tree");
    let text = link_text("child");
    let child_hex = TreeDigest::of(text.as_bytes()).to_string();
    let second_hex = holding_manifest("second", &child_hex, text.len());
    assert_eq!(rig.farm.puts_of(&second_hex), 1);
    assert_eq!(rig.farm.puts_of(&child_hex), 1, "the child was a hit");
    rig.advance(half - Duration::from_nanos(1));
    rig.farm.clear_records();
    rig.write(&second).await.expect("a tree");
    assert_eq!(rig.farm.puts_of(&second_hex), 0, "both still good");
    rig.advance(Duration::from_nanos(1));
    rig.write(&second).await.expect("a tree");
    assert_eq!(rig.farm.puts_of(&child_hex), 1, "the child expired");
    assert_eq!(
        rig.farm.puts_of(&second_hex),
        1,
        "the parent expired with it, half its own time early"
    );
}

/// Catches: a forgotten input root whose entries, or its subdirectories', are still used: its
/// next tree is built from the index as it is then. Forgetting a root never written changes
/// nothing.
#[tokio::test]
async fn forget_drops_every_entry_of_the_root() {
    let rig = Rig::new().await;
    let blob = rig.farm.packed("below", "cas/f/w/p0", 0).await;
    let sub = rig
        .store(&directory(vec![file("f", &blob, false)], vec![], vec![]))
        .await;
    let root = holding(&rig, "r", &sub).await;
    rig.write(&root).await.expect("a tree");
    rig.writer.forget(&link_root(&rig, "never written").await);
    rig.farm.clear_records();
    rig.write(&root).await.expect("a tree");
    assert!(!rig.farm.asked().contains(&blob), "still remembered");
    rig.writer.forget(&root);
    rig.farm.clear_records();
    rig.write(&root).await.expect("a tree");
    assert!(rig.farm.asked().contains(&blob), "looked up again");
    assert_eq!(
        rig.farm.puts().len(),
        4,
        "both manifests, `.kbf` and the root"
    );
}
