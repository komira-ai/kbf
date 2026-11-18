//! Manifest writes shared between preparations, and kept when a preparation is cut off: a
//! manifest two trees need while its write is in flight is written once; a preparation dropped
//! part way (its time ran out) keeps the subtrees it stored, and its write in flight is dropped
//! with it and made anew by the next preparation that needs it; a refused shared write reaches every waiter, by
//! name, and the next preparation makes it anew.
//!
//! A write is held by the fake store's gate ([`Faults::held`]) and each preparation is polled by
//! hand, so which writes are in flight at each step is fixed, not raced.

use std::pin::pin;

use futures::poll;

use super::memo::{holding, holding_manifest, link_manifest, link_root, link_text};
use super::*;

/// Catches: a preparation cut off part way (its prepare limit ran out) remembering nothing, so
/// that every attempt after it writes the whole tree again and runs out of time again: the
/// subtree it stored is remembered and not written again; and its write in flight is not left
/// holding a request for a preparation that is gone: it is dropped, and made anew.
#[tokio::test]
async fn a_preparation_cut_off_keeps_the_subtrees_it_stored() {
    let rig = Rig::new().await;
    let leaf = link_root(&rig, "a").await;
    let root = holding(&rig, "z", &leaf).await;
    let leaf_hex = link_manifest("a");
    let root_hex = holding_manifest("z", &leaf_hex, link_text("a").len());
    rig.farm.state().faults.held = Some(whole_key(&root_hex));
    let kept = rig.kept(&root).await;
    {
        let mut cut = pin!(rig.writer.write(&kept, &rig.base.tree));
        assert!(
            poll!(cut.as_mut()).is_pending(),
            "the input root's write is held"
        );
    }
    assert_eq!(rig.farm.puts_of(&leaf_hex), 1);
    assert_eq!(rig.farm.manifest(&leaf_hex), link_text("a"));
    assert_eq!(rig.farm.puts_of(&root_hex), 1);
    rig.farm.gate.add_permits(1);
    let tree = rig
        .writer
        .write(&kept, &rig.base.tree)
        .await
        .expect("a tree");
    assert_eq!(
        rig.farm.puts_of(&leaf_hex),
        1,
        "the subtree stored before the cut is remembered"
    );
    assert_eq!(
        rig.farm.puts_of(&root_hex),
        2,
        "the cut write was dropped with its preparation and is made anew"
    );
    rig.farm.flatten(&tree).expect("every manifest is stored");
}

/// Catches: concurrent preparations of trees that share a subtree each writing its manifest
/// (one create-only write of every shared manifest per preparation): the second waits for the
/// first's write, and both trees are made.
#[tokio::test]
async fn a_manifest_two_preparations_need_is_written_once() {
    let rig = Rig::new().await;
    let shared = link_root(&rig, "a").await;
    let one = holding(&rig, "x", &shared).await;
    let two = holding(&rig, "y", &shared).await;
    let shared_hex = link_manifest("a");
    rig.farm.state().faults.held = Some(whole_key(&shared_hex));
    let (kept_one, kept_two) = (rig.kept(&one).await, rig.kept(&two).await);
    let mut first = pin!(rig.writer.write(&kept_one, &rig.base.tree));
    let mut second = pin!(rig.writer.write(&kept_two, &rig.base.tree));
    assert!(poll!(first.as_mut()).is_pending());
    assert!(poll!(second.as_mut()).is_pending());
    assert_eq!(
        rig.farm.puts_of(&shared_hex),
        1,
        "the second waits for the first's write"
    );
    rig.farm.gate.add_permits(1);
    let (first, second) = futures::join!(first, second);
    let (first, second) = (first.expect("a tree"), second.expect("a tree"));
    assert_eq!(rig.farm.puts_of(&shared_hex), 1);
    rig.farm.flatten(&first).expect("every manifest is stored");
    rig.farm.flatten(&second).expect("every manifest is stored");
}

/// Catches: a shared write lost when the preparation that started it is dropped while another
/// waits for it: the waiter carries it on and gets its tree.
#[tokio::test]
async fn a_waiter_carries_on_the_write_of_a_preparation_dropped() {
    let rig = Rig::new().await;
    let shared = link_root(&rig, "a").await;
    let one = holding(&rig, "x", &shared).await;
    let two = holding(&rig, "y", &shared).await;
    let shared_hex = link_manifest("a");
    rig.farm.state().faults.held = Some(whole_key(&shared_hex));
    let (kept_one, kept_two) = (rig.kept(&one).await, rig.kept(&two).await);
    let mut second = pin!(rig.writer.write(&kept_two, &rig.base.tree));
    {
        let mut first = pin!(rig.writer.write(&kept_one, &rig.base.tree));
        assert!(poll!(first.as_mut()).is_pending());
        assert!(poll!(second.as_mut()).is_pending());
    }
    rig.farm.gate.add_permits(1);
    let tree = second.await.expect("a tree");
    assert_eq!(rig.farm.puts_of(&shared_hex), 1);
    rig.farm.flatten(&tree).expect("every manifest is stored");
}

/// Catches: a refused shared write taken for written by a waiter, reported to one waiter only,
/// or kept as the answer for every later preparation (or a stored one kept, so that a write
/// after [`TreeWriter::forget`] is never made): each waiter gets the store's refusal by name,
/// the next preparation writes the manifest anew, and a write after the memo is forgotten is
/// made again.
#[tokio::test]
async fn a_refused_shared_write_reaches_every_waiter_and_is_made_anew() {
    let rig = Rig::new().await;
    let shared = link_root(&rig, "a").await;
    let one = holding(&rig, "x", &shared).await;
    let two = holding(&rig, "y", &shared).await;
    let shared_hex = link_manifest("a");
    let key = whole_key(&shared_hex);
    {
        let mut state = rig.farm.state();
        state.faults.held = Some(key.clone());
        state.faults.put = Some(key.clone());
    }
    let (kept_one, kept_two) = (rig.kept(&one).await, rig.kept(&two).await);
    let mut first = pin!(rig.writer.write(&kept_one, &rig.base.tree));
    let mut second = pin!(rig.writer.write(&kept_two, &rig.base.tree));
    assert!(poll!(first.as_mut()).is_pending());
    assert!(poll!(second.as_mut()).is_pending());
    assert_eq!(
        rig.farm.puts_of(&shared_hex),
        1,
        "the second waits for the first's write"
    );
    rig.farm.gate.add_permits(1);
    let (first, second) = futures::join!(first, second);
    let refused = NoTree::Put {
        key: key.clone(),
        why: ObjectStoreError::Unsupported("writes to this key").to_string(),
    };
    assert_eq!(first, Err(refused.clone_refusal()));
    assert_eq!(second, Err(refused));
    assert_eq!(rig.farm.puts_of(&shared_hex), 1);
    {
        let mut state = rig.farm.state();
        state.faults.held = None;
        state.faults.put = None;
    }
    rig.write(&one).await.expect("a tree");
    assert_eq!(
        rig.farm.puts_of(&shared_hex),
        2,
        "the refused write is made anew"
    );
    rig.writer.forget(&one);
    rig.write(&one).await.expect("a tree");
    assert_eq!(
        rig.farm.puts_of(&shared_hex),
        3,
        "a stored write is not kept in flight: forgotten, it is made again"
    );
}

/// A copy of a [`NoTree::Put`] (`NoTree` is not `Clone`).
trait CloneRefusal {
    fn clone_refusal(&self) -> Self;
}

impl CloneRefusal for NoTree {
    fn clone_refusal(&self) -> Self {
        match self {
            Self::Put { key, why } => Self::Put {
                key: key.clone(),
                why: why.clone(),
            },
            other => panic!("not a refused write: {other:?}"),
        }
    }
}
