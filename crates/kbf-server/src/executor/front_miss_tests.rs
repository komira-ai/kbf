//! The frontend's miss path for a tree list a host refused as not mountable: the first refusal of
//! an action has its provider tree written again with its sidecar republished, the second has its
//! jobs read the tree as manifests, and no other action is touched.

use kbf_front::action_tree::ListingWrite;

use super::put_dir::{DirPrep, WALL, action_with_input, listed_base, rig_over};
use super::*;

const PLAIN: ListingWrite = ListingWrite {
    checked: false,
    republish: false,
};

const REPUBLISH: ListingWrite = ListingWrite {
    checked: true,
    republish: true,
};

/// The key or digest the mint of `action` in `form` parked, once the tree is stored.
async fn mint(rig: &Rig, action: &Digest, form: InputTree) -> String {
    rig.front
        .mint(lease(), action, WALL, form, None)
        .await
        .expect("a dir");
    rig.front.stored(lease()).await.expect("stored");
    rig.front.tree(lease()).await.expect("a tree")
}

fn rendered(front: &Front) -> String {
    let mut page = String::new();
    front.metrics.render(&mut page);
    page
}

/// Asserts the two steps in order, for one action. Before any refusal the action's tree list is
/// written plainly and remembered. The first refusal answers `Republish`, leaves the form alone,
/// and has the next provider mint write the list again (the closure collected again) with
/// `ListingWrite { checked: true, republish: true }`, then remember it. The second answers
/// `Manifests`, and the form asked for is manifests from then on: the next mint of that form
/// writes manifests, named by a digest and not a list's key. A third refusal stays at
/// `Manifests`.
#[tokio::test]
async fn the_first_refusal_republishes_and_the_second_gives_manifests() {
    let prepare = DirPrep::new(None, 7_200);
    let rig = rig_over(prepare.clone(), Some(listed_base()));
    let action = action_with_input(&rig, "input").await;
    let digest = cas_digest(&action).expect("a digest");

    let list = mint(&rig, &action, InputTree::Provider).await;
    assert!(list.starts_with("cas/l/"), "{list}");
    assert_eq!(prepare.prepared(), 1);
    assert_eq!(
        rig.front.listing_write(&digest, InputTree::Provider, false),
        PLAIN
    );
    assert_eq!(
        rig.front.input_tree(&action, InputTree::Provider),
        InputTree::Provider
    );

    assert_eq!(rig.front.list_refused(&action).await, MissPath::Republish);
    assert_eq!(
        rig.front.listing_write(&digest, InputTree::Provider, false),
        REPUBLISH
    );
    assert_eq!(
        rig.front
            .listing_write(&digest, InputTree::Manifests, false),
        PLAIN,
        "only a tree list is republished"
    );
    assert_eq!(
        rig.front.input_tree(&action, InputTree::Provider),
        InputTree::Provider
    );
    let again = mint(&rig, &action, InputTree::Provider).await;
    assert_eq!(again, list, "the same inputs make the same list");
    assert_eq!(
        prepare.prepared(),
        2,
        "the list is written again, not remembered"
    );
    mint(&rig, &action, InputTree::Provider).await;
    assert_eq!(
        prepare.prepared(),
        2,
        "the list written again is remembered"
    );

    assert_eq!(rig.front.list_refused(&action).await, MissPath::Manifests);
    assert_eq!(
        rig.front.input_tree(&action, InputTree::Provider),
        InputTree::Manifests
    );
    assert_eq!(
        rig.front.listing_write(&digest, InputTree::Provider, false),
        PLAIN
    );
    let manifests = mint(&rig, &action, InputTree::Manifests).await;
    assert!(
        !manifests.starts_with("cas/l/") && manifests.len() == 64,
        "{manifests}"
    );
    assert_eq!(
        prepare.prepared(),
        3,
        "the refusal dropped the remembered trees"
    );

    assert_eq!(rig.front.list_refused(&action).await, MissPath::Manifests);
    assert_eq!(
        rig.front.input_tree(&action, InputTree::Provider),
        InputTree::Manifests
    );
    let page = rendered(&rig.front);
    assert!(
        page.contains("kbf_executor_list_refused_total{step=\"republish\"} 1\n"),
        "{page}"
    );
    assert!(
        page.contains("kbf_executor_list_refused_total{step=\"manifests\"} 2\n"),
        "{page}"
    );
}

/// Asserts that the miss path of one action leaves another's alone: its form, its writes and its
/// remembered tree are as before, and its own first refusal is still `Republish`.
#[tokio::test]
async fn a_refusal_of_one_action_does_not_touch_another() {
    let prepare = DirPrep::new(None, 7_200);
    let rig = rig_over(prepare.clone(), Some(listed_base()));
    let one = action_with_input(&rig, "input").await;
    let other = action_with_input(&rig, "another").await;
    let other_digest = cas_digest(&other).expect("a digest");
    let list = mint(&rig, &other, InputTree::Provider).await;

    assert_eq!(rig.front.list_refused(&one).await, MissPath::Republish);
    assert_eq!(rig.front.list_refused(&one).await, MissPath::Manifests);

    assert_eq!(
        rig.front.input_tree(&other, InputTree::Provider),
        InputTree::Provider
    );
    assert_eq!(
        rig.front
            .listing_write(&other_digest, InputTree::Provider, false),
        PLAIN
    );
    assert_eq!(mint(&rig, &other, InputTree::Provider).await, list);
    assert_eq!(prepare.prepared(), 1, "its tree is still remembered");
    assert_eq!(rig.front.list_refused(&other).await, MissPath::Republish);
}

/// Asserts that an action a job found an object lost of (so its tree is written checked) and
/// whose list a host refused is written with both marks, and that an action with no refusal is
/// written checked only.
#[tokio::test]
async fn a_checked_tree_of_a_refused_action_is_also_republished() {
    let rig = rig_over(DirPrep::new(None, 7_200), Some(listed_base()));
    let action = action_with_input(&rig, "input").await;
    let digest = cas_digest(&action).expect("a digest");
    let other = cas_digest(&action_with_input(&rig, "another").await).expect("a digest");
    assert_eq!(rig.front.list_refused(&action).await, MissPath::Republish);
    assert_eq!(
        rig.front.listing_write(&digest, InputTree::Provider, true),
        REPUBLISH
    );
    let checked = ListingWrite {
        checked: true,
        republish: false,
    };
    assert_eq!(
        rig.front.listing_write(&other, InputTree::Provider, true),
        checked
    );
}

/// Asserts that a digest that is not one is answered with the manifests step and leaves the form
/// as configured, since there is no tree of ours to write again.
#[tokio::test]
async fn a_digest_that_is_not_one_has_no_miss_path_state() {
    let rig = rig_over(DirPrep::new(None, 7_200), Some(listed_base()));
    let bad = Digest {
        hash: "not hex".to_owned(),
        size_bytes: 1,
    };
    assert_eq!(rig.front.list_refused(&bad).await, MissPath::Manifests);
    assert_eq!(
        rig.front.input_tree(&bad, InputTree::Provider),
        InputTree::Provider
    );
    assert_eq!(
        rendered(&rig.front)
            .matches("list_refused_total{step=\"manifests\"} 0")
            .count(),
        1
    );
}
