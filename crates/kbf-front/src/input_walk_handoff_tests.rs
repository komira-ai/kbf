//! What a walk hands to the run its Execute starts (the module's "What an Execute hands to the
//! run it starts"): every Directory it reads, with its names, in the preparation's cache, the
//! empty one excepted; when its answer was found held; and the Execute's finding, kept by the
//! Action's digest, taken once, believed for the time to live and no longer, refused when it
//! began before a loss, replaced by a newer one, and pushed out beyond its bound. A walk runs on
//! tokio's paused clock; a finding is given its instants outright.

use std::sync::Arc;
use std::time::Duration;

use kbf_types::Digest;
use tokio::time::Instant;

use super::tests::{READ, Store, Tree, dir, file};
use super::*;
use crate::prepare_run::DIRECTORY_MEMORY_BYTES;

const EPOCH: u64 = 7;

/// A root that names the file `a`, a Directory `sub` that names `b`, and the empty Directory.
struct Small {
    store: Store,
    root: Digest,
    sub: Digest,
    empty: Digest,
}

fn small() -> Small {
    let mut tree = Tree::default();
    tree.held.extend([file("a"), file("b")]);
    let sub = tree.add(dir(&[file("b")], &[]));
    let empty = tree.add(dir(&[], &[]));
    let root = tree.add(dir(&[file("a")], &[sub, empty]));
    Small {
        store: Store::fixed(tree, READ),
        root,
        sub,
        empty,
    }
}

fn named_cache() -> Arc<DirectoryCache> {
    Arc::new(DirectoryCache::in_memory(DIRECTORY_MEMORY_BYTES))
}

/// The file node the test tree gives `digest`: named by its digest in hex.
fn named(digest: &Digest) -> (String, Digest, bool) {
    (digest.hash_hex(), *digest, false)
}

/// Catches a walk that keeps nothing for the preparation, keeps a Directory without its names,
/// or keeps the empty Directory the preparation's walk never caches: after one walk the cache
/// holds the root and `sub` exactly as their blobs name their nodes, and not the empty one.
#[tokio::test(start_paused = true)]
async fn a_walk_keeps_each_directory_it_reads_with_its_names_but_not_the_empty_one() {
    let Small {
        store,
        root,
        sub,
        empty,
    } = small();
    let trees = Trees::default();
    let cache = named_cache();
    trees.keep_into(Arc::clone(&cache));
    let walked = walk(&store, root, &trees).await.expect("walk");
    assert!(walked.absent.is_empty(), "{:?}", walked.absent);
    assert_eq!(store.reads_of(&empty), 1, "the empty Directory was read");

    let kept_root = cache.recent(&root).expect("the root is kept");
    assert_eq!(kept_root.files, vec![named(&file("a"))]);
    assert_eq!(
        kept_root.directories,
        vec![(sub.hash_hex(), sub), (empty.hash_hex(), empty)]
    );
    let kept_sub = cache.recent(&sub).expect("sub is kept");
    assert_eq!(kept_sub.files, vec![named(&file("b"))]);
    assert!(kept_sub.directories.is_empty());
    assert_eq!(
        cache.recent(&empty),
        None,
        "the empty Directory is not kept"
    );
}

/// Catches a walk with no cache to keep into that keeps anyway, or one whose keeping changes
/// its answer: a walk before the cache is given keeps nothing, and answers as the walk after.
#[tokio::test(start_paused = true)]
async fn a_walk_keeps_nothing_until_it_is_given_a_cache() {
    let Small { store, root, .. } = small();
    let trees = Trees::new(0, VERIFIED_TTL).with_dir_cache(0);
    let cache = named_cache();
    let before = walk(&store, root, &trees).await.expect("walk");
    assert_eq!(cache.recent(&root), None);
    trees.keep_into(Arc::clone(&cache));
    let after = walk(&store, root, &trees).await.expect("walk");
    assert!(cache.recent(&root).is_some());
    assert_eq!(
        (before.absent, before.dirs_read, before.files_checked),
        (after.absent, after.dirs_read, after.files_checked)
    );
}

/// Catches a time that is later than what the answer rests on, or a time given for an answer
/// that found something absent: a walk that reads everything says it began when it did; a walk
/// that skips a subtree verified earlier says when that verification began; a walk that finds a
/// file gone says nothing.
#[tokio::test(start_paused = true)]
async fn a_walk_says_since_when_its_inputs_were_held_and_nothing_when_one_is_absent() {
    let Small { store, root, .. } = small();
    let store = store.at_epoch(EPOCH);
    let trees = Trees::default();
    let first = Instant::now();
    let read = walk(&store, root, &trees).await.expect("walk");
    assert_eq!(read.held_since, Some(first));

    tokio::time::advance(Duration::from_secs(10)).await;
    let skipped = walk(&store, root, &trees).await.expect("walk");
    assert!(skipped.dirs_skipped > 0, "the root was remembered");
    assert_eq!(
        skipped.held_since,
        Some(first),
        "the verification it rests on"
    );

    store.lose(file("b"));
    tokio::time::advance(VERIFIED_TTL).await;
    let lost = walk(&store, root, &trees).await.expect("walk");
    assert_eq!(lost.absent, vec![file("b")]);
    assert_eq!(lost.held_since, None);
}

/// Catches a finding believed past the time to live, one believed twice, or one found for an
/// Action never vouched for: a finding is believed up to a nanosecond before the time to live
/// ends and not at its end, and is gone once taken.
#[test]
fn a_finding_is_taken_once_and_believed_until_the_time_to_live_ends() {
    let trees = Trees::default();
    let at = Instant::now();
    let (a, b, never) = (file("action a"), file("action b"), file("action c"));
    trees.vouch(a, at);
    trees.vouch(b, at);
    assert!(trees.vouched(&a, at + VERIFIED_TTL - Duration::from_nanos(1)));
    assert!(!trees.vouched(&a, at), "taken once");
    assert!(
        !trees.vouched(&b, at + VERIFIED_TTL),
        "past its time to live"
    );
    assert!(!trees.vouched(&never, at));
}

/// Catches a later finding for an Action that does not replace the earlier one: the newer
/// finding is believed at a time the older one has run out by.
#[test]
fn a_finding_made_again_replaces_the_one_before() {
    let trees = Trees::default();
    let older = Instant::now();
    let newer = older + VERIFIED_TTL;
    let action = file("action");
    trees.vouch(action, older);
    trees.vouch(action, newer);
    assert!(trees.vouched(&action, newer + Duration::from_secs(1)));
}

/// Catches a finding that rests on reads begun before a loss was seen (`distrust`), and one
/// made before the loss that survives it: a finding begun before the loss is refused, one begun
/// at it is kept, and the loss drops the findings not yet taken.
#[test]
fn a_loss_drops_the_findings_and_refuses_one_that_began_before_it() {
    let trees = Trees::default();
    let before = Instant::now();
    let loss = before + Duration::from_secs(1);
    let (kept, dropped, late) = (file("kept"), file("dropped"), file("late"));
    trees.vouch(dropped, before);
    trees.distrust(loss);
    assert!(!trees.vouched(&dropped, loss), "dropped by the loss");
    trees.vouch(late, before);
    assert!(!trees.vouched(&late, loss), "began before the loss");
    trees.vouch(kept, loss);
    assert!(trees.vouched(&kept, loss));
}

/// Catches findings that grow without bound, or a bound that pushes out a finding still waiting
/// in favour of a place already taken or replaced: at a bound of one, the place of a taken
/// finding is passed over without touching the finding made again for that Action, and the next
/// finding pushes that one out.
#[test]
fn findings_beyond_the_bound_push_out_the_oldest_still_waiting() {
    let at = Instant::now();
    let (a, b) = (file("action a"), file("action b"));
    let mut found = Vouched::new(1);
    found.put(a, at);
    assert_eq!(found.take(&a), Some(at));
    found.put(a, at);
    assert_eq!(found.found.len(), 1, "the old place is passed over");
    found.put(b, at);
    assert_eq!(found.take(&a), None, "pushed out by the newer finding");
    assert_eq!(found.take(&b), Some(at));
}
