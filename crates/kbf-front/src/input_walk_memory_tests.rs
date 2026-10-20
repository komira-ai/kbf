//! The memory of verified subtrees ([`Trees`]) and what it does to the cost of a build's
//! input checks. Time is tokio's paused clock; the fake store is that of the sibling
//! `tests` module.

use std::time::Duration;

use kbf_types::Digest;
use tokio::time::{Instant, sleep};

use super::tests::{
    Fail, READ, Store, Tree, chains, clone_tree, dir, file, level_walk, random_with, serial,
    thousand_dirs,
};
use super::*;

// ---- the verified-subtree memory ----

const EPOCH: u64 = 7;

/// A chain tree under a root with `width` heads of `depth`, all files held, in a store
/// that answers an epoch.
fn remembering(width: usize, depth: usize, delay: Duration) -> (Store, Digest) {
    let (tree, root) = chains(width, depth);
    (Store::fixed(tree, delay).at_epoch(EPOCH), root)
}

/// Catches: a walk that remembers nothing (every Execute of a build reads and asks the
/// whole tree again: the cost this change removes), and one that remembers but still
/// reads or asks. The second walk of a verified root is its root, skipped: no read, no
/// FindMissing, no time.
#[tokio::test(start_paused = true)]
async fn a_verified_tree_is_not_read_or_asked_again() {
    let (store, root) = remembering(20, 6, READ);
    let trees = Trees::default();
    let first = walk(&store, root, &trees).await.expect("walk");
    assert_eq!((first.dirs_read, first.dirs_skipped), (121, 0));
    assert_eq!(first.files_checked, 120);
    assert_eq!(first.file_asks, 1);
    let (reads, asked) = (store.total_reads(), store.total_asked());

    let started = Instant::now();
    let second = walk(&store, root, &trees).await.expect("walk");
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert!(second.absent.is_empty());
    assert_eq!((second.dirs_read, second.dirs_skipped), (0, 1));
    assert_eq!((second.files_checked, second.file_asks), (0, 0));
    assert_eq!(store.total_reads(), reads);
    assert_eq!(store.total_asked(), asked);
}

/// Catches: a memory keyed by root only (two actions that share a toolchain but differ in
/// their sources would read the toolchain twice), and one that skips a shared subtree
/// but still asks about its files. Two roots name one 100-Directory subtree and a Directory
/// of their own; the second walk reads its own Directory and the root alone.
#[tokio::test(start_paused = true)]
async fn a_subtree_shared_by_two_roots_is_read_and_asked_once() {
    let (mut tree, toolchain) = chains(10, 10);
    let one = tree.add(dir(&[file("one")], &[]));
    let two = tree.add(dir(&[file("two")], &[]));
    let (a, b) = (
        tree.add(dir(&[], &[toolchain, one])),
        tree.add(dir(&[], &[toolchain, two])),
    );
    tree.held.extend([file("one"), file("two")]);
    let store = Store::fixed(tree, READ).at_epoch(EPOCH);
    let trees = Trees::default();
    walk(&store, a, &trees).await.expect("a");
    let asked = store.total_asked();
    let second = walk(&store, b, &trees).await.expect("b");
    assert_eq!(
        (second.dirs_read, second.dirs_skipped),
        (2, 1),
        "b and two; toolchain skipped"
    );
    assert_eq!(store.total_asked(), asked + 1, "only `two` is asked about");
    assert_eq!(store.reads_of(&toolchain), 1);
}

/// Catches: a remembered subtree believed for ever. A file lost 10 s after the walk is
/// still reported held until the time to live, and is found at it (the bound on how long
/// a blob that went away is taken for held).
#[tokio::test(start_paused = true)]
async fn a_lost_blob_is_found_when_the_time_to_live_ends() {
    let (store, root) = remembering(3, 3, READ);
    let trees = Trees::default();
    walk(&store, root, &trees).await.expect("walk");
    let gone = file("chain 1 level 2");
    store.lose(gone);

    tokio::time::advance(VERIFIED_TTL - Duration::from_secs(1)).await;
    let early = walk(&store, root, &trees).await.expect("walk");
    assert!(early.absent.is_empty(), "believed within the bound");
    assert_eq!(early.dirs_read, 0);

    tokio::time::advance(Duration::from_secs(1)).await;
    let late = walk(&store, root, &trees).await.expect("walk");
    assert_eq!(
        late.absent,
        vec![gone],
        "at the bound the tree is walked again"
    );
    assert_eq!(late.dirs_read, 10);
}

/// Catches: a memory that outlives a change of what "held" rests on. The same loss is
/// found at once when the epoch moved (a new map generation, or a member that
/// restarted), well inside the time to live.
#[tokio::test(start_paused = true)]
async fn a_new_epoch_forgets_everything() {
    let (store, root) = remembering(3, 3, READ);
    let trees = Trees::default();
    walk(&store, root, &trees).await.expect("walk");
    let gone = file("chain 0 level 0");
    store.lose(gone);
    assert!(
        walk(&store, root, &trees)
            .await
            .expect("walk")
            .absent
            .is_empty()
    );

    store.set_epoch(Some(EPOCH + 1));
    let after = walk(&store, root, &trees).await.expect("walk");
    assert_eq!(after.absent, vec![gone]);
    assert_eq!(after.dirs_read, 10);
}

/// Catches: remembering when the store cannot say what the answer rests on (no epoch):
/// a single storage node, or a map with a member of unknown boot. Nothing is skipped
/// and nothing is kept.
#[tokio::test(start_paused = true)]
async fn nothing_is_remembered_without_an_epoch() {
    let (store, root) = remembering(3, 3, READ);
    store.set_epoch(None);
    let trees = Trees::default();
    walk(&store, root, &trees).await.expect("walk");
    let again = walk(&store, root, &trees).await.expect("walk");
    assert_eq!((again.dirs_read, again.dirs_skipped), (10, 0));
    assert_eq!(trees.len(), 0);
}

/// Catches: a walk that changed epoch while it ran writing its findings under the new
/// one (it began under the old map, so what it found says nothing of the new): the next
/// walk must read.
#[tokio::test(start_paused = true)]
async fn a_walk_that_straddles_an_epoch_change_remembers_nothing() {
    let (tree, root) = chains(3, 3);
    let store = Store::new(tree, |_| READ).at_epoch(EPOCH);
    let trees = Trees::default();
    let moving = async {
        sleep(READ).await;
        store.set_epoch(Some(EPOCH + 1));
    };
    let (walked, ()) = tokio::join!(walk(&store, root, &trees), moving);
    assert_eq!(walked.expect("walk").dirs_read, 10);
    assert_eq!(trees.len(), 0);
}

/// Catches: a memory that trusts verifications made before a loss was seen. One walk
/// finds a file of another tree absent (a blob went away); the tree verified before it
/// shares nothing with it, yet it is walked again at once, and its own lost file is
/// found, inside the time to live.
#[tokio::test(start_paused = true)]
async fn finding_something_absent_distrusts_what_was_verified_before() {
    let mut tree = Tree::default();
    let good = file("good");
    let victim = file("victim");
    let first = tree.add(dir(&[good, victim], &[]));
    let hole = tree.add(dir(&[file("never uploaded")], &[]));
    tree.held.extend([good, victim]);
    let store = Store::fixed(tree, READ).at_epoch(EPOCH);
    let trees = Trees::default();
    walk(&store, first, &trees).await.expect("walk");
    store.lose(victim);
    assert!(
        walk(&store, first, &trees)
            .await
            .expect("walk")
            .absent
            .is_empty()
    );

    let refused = walk(&store, hole, &trees).await.expect("walk");
    assert_eq!(refused.absent, vec![file("never uploaded")]);
    let again = walk(&store, first, &trees).await.expect("walk");
    assert_eq!(again.absent, vec![victim]);
    assert_eq!(again.dirs_read, 1);
}

/// Catches: a verification that began before a loss was seen, but ends after, recorded
/// as if it were later than the loss. The walk of a slow tree starts; while it runs
/// another walk finds a file absent; the slow walk's findings are not kept.
#[tokio::test(start_paused = true)]
async fn a_walk_that_began_before_a_loss_was_seen_is_not_kept() {
    let mut tree = Tree::default();
    let slow_root = tree.add(dir(&[file("slow")], &[]));
    let hole = tree.add(dir(&[file("never uploaded")], &[]));
    tree.held.insert(file("slow"));
    let store = Store::new(tree, move |d| {
        if *d == slow_root {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(1)
        }
    })
    .at_epoch(EPOCH);
    let trees = Trees::default();
    let (slow, quick) = tokio::join!(walk(&store, slow_root, &trees), async {
        sleep(Duration::from_secs(1)).await;
        walk(&store, hole, &trees).await
    });
    assert!(slow.expect("slow").absent.is_empty());
    assert_eq!(quick.expect("quick").absent.len(), 1);
    assert_eq!(
        trees.len(),
        0,
        "the slow walk began before the loss was seen"
    );
}

/// Catches: findings written from a walk that failed or found something absent (a
/// partial result believed). Neither the failing nor the absent-reporting walk
/// leaves an entry, and the next walk of the same tree reads it again.
#[tokio::test(start_paused = true)]
async fn only_a_clean_walk_is_remembered() {
    for failing in [Fail::Internal, Fail::Gone] {
        let mut tree = Tree::default();
        let ok = tree.add(dir(&[file("ok")], &[]));
        let bad = tree.add(dir(&[file("bad")], &[]));
        tree.fail.insert(bad, failing);
        let root = tree.add(dir(&[], &[ok, bad]));
        tree.held.extend([file("ok"), file("bad")]);
        let store = Store::fixed(tree, READ).at_epoch(EPOCH);
        let trees = Trees::default();
        let _ = walk(&store, root, &trees).await;
        assert_eq!(trees.len(), 0, "{failing:?}");
        let _ = walk(&store, root, &trees).await;
        assert_eq!(store.reads_of(&ok), 2, "{failing:?}");
    }
    // A file missing, every Directory fine.
    let mut tree = Tree::default();
    let root = tree.add(dir(&[file("not held")], &[]));
    let store = Store::fixed(tree, READ).at_epoch(EPOCH);
    let trees = Trees::default();
    assert_eq!(
        walk(&store, root, &trees).await.expect("walk").absent.len(),
        1
    );
    assert_eq!(trees.len(), 0);
}

/// Catches: a root recorded as verified as of its own walk when part of it was skipped on
/// the strength of an older verification (the root would outlive the subtree it rests
/// on). A toolchain is verified at t=0; at t=30 a root over it and a new Directory is
/// walked (toolchain skipped); the toolchain loses a file at t=40; at t=50 the root is
/// older than the toolchain's bound (45 s from t=0), so it must be walked again and the
/// loss found. Kept as the root's own 30 s it would be believed until t=75.
#[tokio::test(start_paused = true)]
async fn a_root_is_no_fresher_than_the_oldest_subtree_it_skipped() {
    let (mut tree, toolchain) = chains(2, 2);
    let own = tree.add(dir(&[file("own")], &[]));
    tree.held.insert(file("own"));
    let root = tree.add(dir(&[], &[toolchain, own]));
    let store = Store::fixed(tree, READ).at_epoch(EPOCH);
    let trees = Trees::default();
    walk(&store, toolchain, &trees).await.expect("toolchain");
    tokio::time::advance(Duration::from_secs(30)).await;
    let w = walk(&store, root, &trees).await.expect("root");
    assert_eq!((w.dirs_read, w.dirs_skipped), (2, 1));
    tokio::time::advance(Duration::from_secs(10)).await;
    let gone = file("chain 0 level 1");
    store.lose(gone);
    tokio::time::advance(Duration::from_secs(10)).await;
    let late = walk(&store, root, &trees).await.expect("root");
    assert_eq!(late.absent, vec![gone]);
}

/// Catches: a memory without a bound, and one that evicts the wrong end. With room for 4
/// Directories, a 10-Directory tree leaves at most 4; the most recently used survive
/// (a Directory used again is kept over one that was not).
#[tokio::test(start_paused = true)]
async fn the_memory_is_bounded_and_least_recently_used_goes_first() {
    let trees = Trees::new(4, VERIFIED_TTL);
    let (store, root) = remembering(3, 3, Duration::ZERO);
    walk(&store, root, &trees).await.expect("walk");
    assert_eq!(trees.len(), 4);
    let now = Instant::now();
    // The last four recorded are the last four read; use the oldest of them, then add one.
    let kept: Vec<Digest> = trees.lock().recency.values().copied().collect();
    assert!(trees.fresh(EPOCH, &kept[0], now).is_some());
    let extra = file("extra dir");
    trees.record(EPOCH, &[extra], now);
    assert_eq!(trees.len(), 4);
    assert!(
        trees.fresh(EPOCH, &kept[0], now).is_some(),
        "used again, so kept"
    );
    assert!(
        trees.fresh(EPOCH, &kept[1], now).is_none(),
        "least recently used went"
    );
    assert!(trees.fresh(EPOCH, &extra, now).is_some());
    let none = Trees::new(0, VERIFIED_TTL);
    none.record(EPOCH, &[extra], now);
    assert_eq!(none.len(), 0);
}

/// Catches: an answer that differs because of the memory. 300 random trees with every
/// hazard, walked three times on one memory: each walk answers what a memoryless serial
/// walk does; a walk that was not clean leaves nothing, so it reads as much each time; a
/// clean one is then free. And 200 clean random trees, all of which end verified.
#[tokio::test(start_paused = true)]
async fn the_memory_never_changes_an_answer() {
    let (mut clean, mut dirty) = (0, 0);
    for (seed, hazards) in (1..=300u64)
        .map(|s| (s, true))
        .chain((1..=200u64).map(|s| (s, false)))
    {
        let (tree, root) = random_with(seed, hazards);
        let want = serial(&Store::fixed(clone_tree(&tree), Duration::ZERO), root).await;
        let store = Store::new(clone_tree(&tree), move |d| {
            Duration::from_millis(u64::from(d.hash_hex().as_bytes()[0]) % 13)
        })
        .at_epoch(EPOCH);
        let trees = Trees::default();
        let mut reads = Vec::new();
        for round in 0..3 {
            let before = store.total_reads();
            let got = walk_bounded(&store, root, 5, &trees)
                .await
                .map(|w| (w.absent, w.dirs_read))
                .map_err(|e| e.to_string());
            let skipped_all = got.as_ref().is_ok_and(|(_, read)| *read == 0);
            if !skipped_all {
                assert_eq!(
                    got.as_ref().map(|g| &g.0),
                    want.as_ref().map(|w| &w.0),
                    "seed {seed} round {round}"
                );
                assert_eq!(
                    got.as_ref().err(),
                    want.as_ref().err(),
                    "seed {seed} round {round}"
                );
            }
            reads.push(store.total_reads() - before);
        }
        match &want {
            Ok((absent, _)) if absent.is_empty() => {
                clean += 1;
                assert_eq!(reads[1..], [0, 0], "seed {seed}: verified, so free");
            }
            _ => {
                dirty += 1;
                assert_eq!(
                    reads[0], reads[1],
                    "seed {seed}: nothing kept from an unclean walk"
                );
                assert_eq!(reads[0], reads[2], "seed {seed}");
                assert_eq!(trees.len(), 0, "seed {seed}");
            }
        }
    }
    assert!(clean > 100 && dirty > 30, "{clean} clean, {dirty} dirty");
}

/// A komira-like action set: `actions` roots over one toolchain tree of about 1050
/// Directories (`thousand_dirs`) plus a private source subtree of 13 Directories each.
fn action_set(actions: usize) -> (Tree, Vec<Digest>) {
    let (mut tree, toolchain) = thousand_dirs();
    let roots = (0..actions)
        .map(|n| {
            let mut below = Vec::new();
            for level in (0..3).rev() {
                let width = if level == 2 {
                    9
                } else if level == 1 {
                    3
                } else {
                    1
                };
                let here: Vec<Digest> = (0..width)
                    .map(|m| {
                        let name = file(&format!("action {n} level {level} dir {m}"));
                        tree.held.insert(name);
                        let dirs: Vec<Digest> = below
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| i % width == m)
                            .map(|(_, d)| *d)
                            .collect();
                        tree.add(dir(&[name], &dirs))
                    })
                    .collect();
                below = here;
            }
            tree.add(dir(&[], &[toolchain, below[0]]))
        })
        .collect();
    (tree, roots)
}

/// What presubmit costs an action set: for each action, the time from its arrival to the
/// end of its walk. Returns (mean, max, reads the store did). Run with `--nocapture` for
/// the figures. `fresh` walks without a memory (the tip's walk before this change).
async fn presubmit(
    read: Duration,
    gap: Duration,
    actions: usize,
    old: bool,
) -> (Duration, Duration, usize) {
    let (tree, roots) = action_set(actions);
    let store = Store::fixed(tree, read)
        .with_asks(read)
        .with_flights()
        .at_epoch(EPOCH);
    let trees = Trees::default();
    let waits = futures::future::join_all(roots.iter().enumerate().map(|(n, root)| {
        let (store, trees) = (&store, &trees);
        async move {
            sleep(gap * n as u32).await;
            let began = Instant::now();
            if old {
                level_walk(store, *root, 16).await;
            } else {
                walk(store, *root, trees).await.expect("walk");
            }
            began.elapsed()
        }
    }))
    .await;
    let mean = waits.iter().sum::<Duration>() / waits.len() as u32;
    (mean, waits.into_iter().max().unwrap(), store.total_reads())
}

/// The figures of the change for a komira-like action set at the two read times the
/// test farm showed (16 ms before, 56 ms after); arrival either all at once (a build's
/// first wave) or one every 100 ms. The store models the router's read flights, so a
/// Directory two walks want at once is read once. Prints the figures (`--nocapture`) and
/// asserts their order of magnitude so they cannot silently regress.
async fn presubmit_figures(actions: usize) {
    for read in [Duration::from_millis(16), Duration::from_millis(56)] {
        for (arrival, gap) in [
            ("all at once", Duration::ZERO),
            ("one per 100 ms", Duration::from_millis(100)),
        ] {
            let (old_mean, old_max, old_reads) = presubmit(read, gap, actions, true).await;
            let (new_mean, new_max, new_reads) = presubmit(read, gap, actions, false).await;
            println!(
                "{actions} actions, read {read:?}, {arrival}: before mean {old_mean:?} max {old_max:?} \
                 ({old_reads} store reads) | after mean {new_mean:?} max {new_max:?} ({new_reads} store reads)"
            );
            assert!(new_mean <= old_mean && new_reads <= old_reads);
            if gap > Duration::ZERO {
                assert!(new_mean * 5 < old_mean, "{new_mean:?} vs {old_mean:?}");
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn presubmit_of_a_komira_like_action_set() {
    presubmit_figures(60).await;
}

/// The 482 walks of the build that was measured: `cargo test -- --ignored --nocapture`.
#[tokio::test(start_paused = true)]
#[ignore = "the figures of the pull request, a few seconds of CPU"]
async fn presubmit_of_the_measured_build() {
    presubmit_figures(482).await;
}

/// Catches: findings written under an epoch the memory has moved on from (a walk of the
/// old map recording into the new map's memory would make its verdicts believed there).
/// The memory adopts an epoch at its first lookup; a record for another is refused, one
/// for the adopted epoch is kept.
#[tokio::test(start_paused = true)]
async fn a_finding_for_another_epoch_is_refused() {
    let trees = Trees::default();
    let (now, dir) = (Instant::now(), file("some directory"));
    assert!(trees.fresh(EPOCH, &dir, now).is_none(), "adopts the epoch");
    trees.record(EPOCH + 1, &[dir], now);
    assert_eq!(trees.len(), 0);
    trees.record(EPOCH, &[dir], now);
    assert_eq!(trees.len(), 1);
    assert!(trees.fresh(EPOCH, &dir, now).is_some());
}
