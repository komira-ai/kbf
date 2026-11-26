//! Action trees started ahead of their runs ([`Ingest::start_tree`]), through the frontend's
//! real pieces ([`walking`]): the Execute's start of the write, which the rig's dispatch keeps,
//! handed to this [`Front`] as the executor hands it, and the mint of the run that takes it.
//! The store's writes are held where a test says, through the rig's object store, and let
//! through when it says; no test reads a clock or waits a set time.

use kbf_front::{DecodedAction, inputs_held};

use super::super::{OTHER_IMAGE, platform, props, put_dir};
use super::*;
use crate::executor::traits::LostObject;

/// An Execute of a fresh input set on `walking`, submitted, and its start of the tree's write as
/// the dispatch kept it.
async fn executed(walking: &Walking) -> (InputSet, Started) {
    let set = input_set(&walking.rig).await;
    execute(&walking.service, &set.action).await;
    let mut started = (walking.taken.1.lock()).unwrap_or_else(PoisonError::into_inner);
    let start = started.pop().expect("the Execute started its tree's write");
    drop(started);
    (set, start)
}

/// Whether a write of `action`'s tree in `form` is under way, started ahead of its run.
fn under_way(rig: &Rig, action: &kbf_types::Digest, form: InputTree) -> bool {
    let earlies = rig.front.earlies.lock();
    let earlies = earlies.unwrap_or_else(PoisonError::into_inner);
    earlies.contains_key(&(*action, form))
}

/// Lets every write of the rig's object store through, from now on.
fn let_writes_through(rig: &Rig) {
    rig.store.hold_puts.store(false, Ordering::SeqCst);
    rig.store.released.add_permits(1_000);
}

/// Asserts that the run's mint takes the tree its Execute started beside the held check instead
/// of collecting a closure and writing the tree itself, and that the run is still not stored
/// until the tree's writes are: with every write to the store held, the mint names the tree, its
/// `stored` is unanswered while the writes are held, and answers once they are let through (the
/// job is submitted only after `stored`, and the writer stores the tree's root last). The run
/// prepared no closure of its own, the one closure was collected ahead of it, each Directory was
/// read once and each file asked about once (by the Execute), and the tree is remembered for the
/// action. Catches a mint that collects and writes the tree a second time, a run stored before
/// the tree's writes are, and a second presence check.
#[tokio::test]
async fn the_run_takes_the_tree_its_execute_started_and_waits_for_its_writes() {
    let walking = walking();
    let (set, (action, decoded, finding)) = executed(&walking).await;
    let Walking { rig, log, real, .. } = &walking;
    rig.store.hold_puts.store(true, Ordering::SeqCst);

    rig.front.start_tree(
        &set.action,
        Arc::clone(&decoded),
        InputTree::Manifests,
        finding,
    );
    let minted = (rig.front)
        .mint(
            lease(),
            &set.action,
            WALL,
            InputTree::Manifests,
            Some(&decoded),
        )
        .await;
    minted.expect("a dir");
    let tree = rig.front.tree(lease()).await.expect("the tree is named");
    let stored = rig.front.stored(lease());
    tokio::pin!(stored);
    assert!(
        futures::poll!(stored.as_mut()).is_pending(),
        "stored while every write of the tree is held"
    );
    let_writes_through(rig);
    stored.await.expect("stored");

    assert_eq!((real.prepared(), real.collected()), (0, 1));
    assert_eq!((log.reads_of(&set.root), log.reads_of(&set.sub)), (1, 1));
    let mut once = set.files.to_vec();
    once.sort();
    assert_eq!(log.asked(), once);
    let remembered = rig
        .front
        .remembered_tree(&action, InputTree::Manifests, clock());
    assert_eq!(remembered, Some(tree));
    assert!(!under_way(rig, &action, InputTree::Manifests), "forgotten");
}

/// Asserts that a tree started for an Execute that did not find every input held is written but
/// not remembered, and the action's next mint collects a closure and writes the tree itself;
/// while the same write for an Execute that found every input held is remembered, and the next
/// mint collects nothing. Catches a tree trusted for an Execute that ended MISSING.
#[tokio::test]
async fn a_tree_is_remembered_only_once_its_execute_found_every_input_held() {
    for held in [false, true] {
        let walking = walking();
        let (set, (action, decoded, _)) = executed(&walking).await;
        let Walking { rig, real, .. } = &walking;
        let (said, finding) = inputs_held();
        let form = InputTree::Manifests;
        rig.front
            .start_tree(&set.action, Arc::clone(&decoded), form, finding);
        match held {
            true => said.held(),
            false => drop(said),
        }

        let taken = rig.front.take_early(&action, form).await;
        let (tree, writes) = taken.expect("the tree is named");
        writes.await.expect("the task ended").expect("written");

        let remembered = rig.front.remembered_tree(&action, form, clock());
        assert_eq!(remembered, held.then_some(tree), "held: {held}");
        let minted = (rig.front)
            .mint(lease(), &set.action, WALL, form, Some(&decoded))
            .await;
        minted.expect("a dir");
        assert_eq!(real.prepared(), usize::from(!held), "held: {held}");
    }
}

/// The digest of an action stored on `walking`'s node whose input root is a Directory the node
/// does not hold, and its decode.
async fn rootless(walking: &Walking) -> (Digest, Arc<DecodedAction>) {
    let rig = &walking.rig;
    let absent = kbf_front::VerifiedBlob::hashed(Bytes::from_static(b"no such Directory"));
    let command = reapi::Command::default();
    let action = Action {
        command_digest: Some(wire(&rig.store(&command).await)),
        input_root_digest: Some(wire(&absent.digest())),
        ..Action::default()
    };
    let digest = wire(&rig.store(&action).await);
    (digest, Arc::new(DecodedAction { action, command }))
}

/// Asserts that a write that ended before it named the tree (its closure could not be
/// collected: the input root is not held) is not taken: the run's mint collects the closure
/// itself, and answers exactly what a mint with no tree started answers. Catches a mint that
/// takes a write with no tree, and a refusal of the run lost to the early write's.
#[tokio::test]
async fn a_write_that_named_no_tree_leaves_the_run_its_own_answer() {
    let walking = walking();
    let (action, decoded) = rootless(&walking).await;
    let Walking { rig, real, .. } = &walking;
    let (said, finding) = inputs_held();
    said.held();

    rig.front
        .start_tree(&action, Arc::clone(&decoded), InputTree::Manifests, finding);
    let minted = (rig.front)
        .mint(lease(), &action, WALL, InputTree::Manifests, Some(&decoded))
        .await;

    let error = minted.expect_err("the input root is not held");
    assert_eq!((real.collected(), real.prepared()), (1, 1));
    let alone = super::walking();
    let (same, decoded) = rootless(&alone).await;
    let unstarted = (alone.rig.front)
        .mint(lease(), &same, WALL, InputTree::Manifests, Some(&decoded))
        .await;
    assert_eq!(Err(error), unstarted.map(drop));
}

/// Asserts that an object of the action's tree the store lost, while a write started ahead of
/// the run is still under way, drops that write: once it ends it is not remembered, and the
/// action's next start writes nothing while its next tree is to be written checked. Catches a
/// tree remembered after the action's trees were dropped for an object it may name.
#[tokio::test]
async fn a_lost_object_drops_a_write_still_under_way() {
    let walking = walking();
    let (set, (action, decoded, finding)) = executed(&walking).await;
    let rig = &walking.rig;
    rig.store.hold_puts.store(true, Ordering::SeqCst);
    let form = InputTree::Manifests;
    rig.front
        .start_tree(&set.action, Arc::clone(&decoded), form, finding);
    let taken = rig.front.take_early(&action, form).await;
    let (_, writes) = taken.expect("the tree is named");

    rig.front
        .lost(&set.action, &LostObject::Manifest([7; 32]))
        .await;
    assert!(!under_way(rig, &action, form), "dropped");
    let_writes_through(rig);
    writes.await.expect("the task ended").expect("written");

    assert_eq!(rig.front.remembered_tree(&action, form, clock()), None);
    let (said, again) = inputs_held();
    said.held();
    rig.front.start_tree(&set.action, decoded, form, again);
    assert!(!under_way(rig, &action, form), "checked: nothing started");
}

/// Asserts which Executes start a write and in which form: a second start of an action whose
/// write is under way starts none (one closure collected); a start of an action whose tree is
/// remembered starts none; a Mac action's write is its tree list whatever form is configured;
/// and none is started for an action whose facts are refused (another image than the served
/// base) or whose digest is not one. Catches a second write of one tree, a tree written in a
/// form its run is not minted in, and a write for an action no run can take.
#[tokio::test]
async fn a_tree_is_started_once_and_in_the_form_of_its_run() {
    let walking = walking();
    let (set, (action, decoded, finding)) = executed(&walking).await;
    let Walking { rig, real, .. } = &walking;
    let form = InputTree::Manifests;
    rig.front
        .start_tree(&set.action, Arc::clone(&decoded), form, finding);
    let (said, twin) = inputs_held();
    said.held();
    rig.front
        .start_tree(&set.action, Arc::clone(&decoded), form, twin);
    let (_, writes) = (rig.front.take_early(&action, form).await).expect("named");
    writes.await.expect("the task ended").expect("written");
    assert_eq!(real.collected(), 1, "one write for both starts");
    let (_, remembered) = inputs_held();
    rig.front
        .start_tree(&set.action, Arc::clone(&decoded), form, remembered);
    assert!(
        !under_way(rig, &action, form),
        "remembered: nothing started"
    );

    let mac = Arc::new(DecodedAction {
        action: Action {
            platform: Some(props(&[("os", "macos")])),
            ..decoded.action.clone()
        },
        command: decoded.command.clone(),
    });
    let mac_digest = wire(&rig.store(&mac.action).await);
    let mac_action = cas_digest(&mac_digest).expect("a digest");
    rig.front
        .start_tree(&mac_digest, mac, form, inputs_held().1);
    assert!(under_way(rig, &mac_action, InputTree::Provider), "a Mac's");
    assert!(!under_way(rig, &mac_action, form));

    let other = Arc::new(DecodedAction {
        action: Action {
            platform: Some(platform(OTHER_IMAGE)),
            ..decoded.action.clone()
        },
        command: decoded.command.clone(),
    });
    let other_digest = wire(&rig.store(&other.action).await);
    let other_action = cas_digest(&other_digest).expect("a digest");
    rig.front
        .start_tree(&other_digest, other, form, inputs_held().1);
    assert!(!under_way(rig, &other_action, form), "refused facts");
    let negative = Digest {
        size_bytes: -1,
        ..set.action
    };
    let before = rig.front.earlies.lock().map(|e| e.len()).unwrap_or(0);
    rig.front
        .start_tree(&negative, decoded, form, inputs_held().1);
    let after = rig.front.earlies.lock().map(|e| e.len()).unwrap_or(0);
    assert_eq!(after, before, "no digest, nothing started");
}

/// Asserts that no write is started before the base tree is published (no tree can be written
/// over it), and that a preparation that collects no closure ahead of its run (the default)
/// starts a write that names no tree, so its run's mint collects its own. Catches a write
/// started with no base, and a mint that waits on a write that never names its tree.
#[tokio::test]
async fn no_tree_is_named_ahead_of_its_run_without_a_base_or_a_closure() {
    let unpublished = put_dir::rig_with(put_dir::DirPrep::new(None, 7_200), false);
    let digest = put_dir::action_with_input(&unpublished, "f").await;
    let action = cas_digest(&digest).expect("a digest");
    let decoded = Arc::new(DecodedAction {
        action: Action::default(),
        command: reapi::Command::default(),
    });
    let form = InputTree::Manifests;
    (unpublished.front).start_tree(&digest, Arc::clone(&decoded), form, inputs_held().1);
    assert!(!under_way(&unpublished, &action, form), "no base");

    let prep = put_dir::DirPrep::new(None, 7_200);
    let rig = put_dir::rig_with(Arc::clone(&prep), true);
    let digest = put_dir::action_with_input(&rig, "f").await;
    let action = cas_digest(&digest).expect("a digest");
    rig.front
        .start_tree(&digest, decoded, form, inputs_held().1);
    assert!(under_way(&rig, &action, form), "started");
    assert!(rig.front.take_early(&action, form).await.is_none());
    rig.front
        .mint(lease(), &digest, WALL, form, None)
        .await
        .expect("a dir");
    assert_eq!(prep.prepared(), 1, "the run's own closure");
}

/// Asserts that a host's refusal of the action's tree list drops a write still under way, and
/// only that one: the write the next Execute starts after the refusal is under way when the
/// first ends (the first's end does not forget it), the first is not remembered though its
/// Execute found every input held, and the next is remembered once its own Execute says so.
/// Catches a tree remembered after a refused tree list dropped the action's trees, and a write
/// whose end forgets a later write of the same action and form.
#[tokio::test]
async fn a_refused_tree_list_drops_the_write_under_way_and_not_the_next() {
    let walking = walking();
    let (set, (action, decoded, finding)) = executed(&walking).await;
    let rig = &walking.rig;
    let form = InputTree::Manifests;
    rig.store.hold_puts.store(true, Ordering::SeqCst);
    rig.front
        .start_tree(&set.action, Arc::clone(&decoded), form, finding);
    let (_, first) = (rig.front.take_early(&action, form).await).expect("named");

    let step = rig.front.list_refused(&set.action).await;
    assert_eq!(step, MissPath::Republish);
    assert!(!under_way(rig, &action, form), "dropped");
    let (said, next) = inputs_held();
    rig.front
        .start_tree(&set.action, Arc::clone(&decoded), form, next);
    assert!(under_way(rig, &action, form), "the next write");
    let_writes_through(rig);
    first.await.expect("the task ended").expect("written");

    assert!(
        under_way(rig, &action, form),
        "the first's end kept the next"
    );
    assert_eq!(rig.front.remembered_tree(&action, form, clock()), None);
    let (tree, writes) = (rig.front.take_early(&action, form).await).expect("named");
    said.held();
    writes.await.expect("the task ended").expect("written");
    assert_eq!(
        rig.front.remembered_tree(&action, form, clock()),
        Some(tree)
    );
}

/// Asserts that a write that fails after it named the tree fails the run that took it, as the
/// run's own write would: the mint names the tree, the run's `stored` answers UNAVAILABLE (so its
/// job is never submitted on a tree that is not stored), and the tree is not remembered though
/// its Execute found every input held. Catches a run stored over a failed write, and a tree
/// remembered from one.
#[tokio::test]
async fn a_write_that_fails_after_naming_its_tree_fails_its_run_and_is_not_remembered() {
    let walking = walking();
    let (set, (action, decoded, finding)) = executed(&walking).await;
    let rig = &walking.rig;
    let form = InputTree::Manifests;
    rig.store.fail_puts.store(true, Ordering::SeqCst);
    rig.front
        .start_tree(&set.action, Arc::clone(&decoded), form, finding);

    let minted = (rig.front)
        .mint(lease(), &set.action, WALL, form, Some(&decoded))
        .await;
    minted.expect("a dir");
    let failed = rig.front.stored(lease()).await;

    assert!(
        matches!(&failed, Err(MintError::Unavailable(why)) if why.starts_with("no action tree")),
        "{failed:?}"
    );
    assert_eq!(walking.real.prepared(), 0, "the run took the started write");
    assert_eq!(rig.front.remembered_tree(&action, form, clock()), None);
    assert!(!under_way(rig, &action, form), "forgotten");
}
