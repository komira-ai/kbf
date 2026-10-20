//! The content cache of decoded Directories behind the walk: that a cached Directory is
//! trusted only while the store says it is held, what a walk over a warm cache costs,
//! that the bound holds, that concurrent walks share reads, and that the figures the
//! walk reports are the ones the metrics and the warning carry. Time is tokio's paused
//! clock; the fake store is that of the sibling `tests` module.

use std::time::Duration;

use futures::future::join_all;
use kbf_types::Digest;
use tokio::time::{Instant, sleep, timeout};

use super::memory_tests::action_set;
use super::tests::{
    Fail, READ, Store, Tree, chains, clone_tree, dir, file, random, serial, thousand_dirs,
};
use super::*;

const EPOCH: u64 = 7;

/// The Directory of `tree` that names the file called `name`.
fn naming(tree: &Tree, name: &str) -> Digest {
    let want = file(name);
    let mut found = tree
        .blobs
        .iter()
        .filter(|(d, bytes)| decode(d, bytes).is_ok_and(|dec| dec.files.contains(&want)))
        .map(|(d, _)| *d);
    let first = found.next().expect("a Directory names it");
    assert!(found.next().is_none(), "only one does");
    first
}

/// What `tree`'s Directories cost in the cache, all together.
fn cost_of(tree: &Tree) -> u64 {
    tree.blobs
        .iter()
        .map(|(d, bytes)| decode(d, bytes).expect("decodes").cost())
        .sum()
}

// ---- trust: presence always comes from the store ----

/// Catches, the core safety of the cache: a cached Directory taken for held because its
/// content is known. A tree is walked (cached), then a Directory in the middle of a chain
/// and a file under it are lost; the next walk reads nothing, yet reports exactly the
/// lost Directory, and not the file under it (as a walk that stopped at the Directory
/// does). Without the presence check it answers "nothing absent"; without the pruning it
/// also names the file.
#[tokio::test(start_paused = true)]
async fn a_cached_directory_the_store_lost_is_absent_and_hides_what_is_under_it() {
    let (tree, root) = chains(3, 3);
    let lost = naming(&tree, "chain 1 level 1");
    let under = file("chain 1 level 2");
    let store = Store::fixed(tree, READ);
    let trees = Trees::default();
    let first = walk(&store, root, &trees).await.expect("walk");
    assert_eq!((first.dirs_read, first.absent.len()), (10, 0));

    store.lose(lost);
    store.lose(under);
    let reads = store.total_reads();
    let second = walk(&store, root, &trees).await.expect("walk");
    assert_eq!(second.absent, vec![lost]);
    assert_eq!(store.total_reads(), reads, "no Directory was read");
    assert_eq!(second.dirs_read, 0);
    assert_eq!(
        (second.dir_cache_hits, second.dirs_present_checked),
        (10, 10)
    );
}

/// Catches: a lost cached Directory that does not clear the verified memory (the loss
/// handling of the walk before the cache: a walk that finds anything absent has seen a
/// loss, and nothing verified before it is believed), a cached Directory trusted inside
/// the time to live, and a loss reported once and then forgotten. Verified at t=0; lost
/// at t=10 s; at t=20 s the verified memory still says held (the bound it always had,
/// no FindMissing); at the time to live the walk finds the loss from the store with no
/// read, and finds it again at once, so the loss was not recorded as verified.
#[tokio::test(start_paused = true)]
async fn a_loss_found_through_the_cache_clears_the_verified_memory() {
    let (tree, root) = chains(3, 3);
    let lost = naming(&tree, "chain 0 level 1");
    let store = Store::fixed(tree, READ).at_epoch(EPOCH);
    let trees = Trees::default();
    walk(&store, root, &trees).await.expect("walk");
    assert_eq!(trees.len(), 10);
    tokio::time::advance(Duration::from_secs(10)).await;
    store.lose(lost);

    tokio::time::advance(Duration::from_secs(10)).await;
    let believed = walk(&store, root, &trees).await.expect("walk");
    assert!(believed.absent.is_empty());
    assert_eq!((believed.dirs_skipped, believed.dir_cache_hits), (1, 0));
    let asks = store.ask_calls();

    tokio::time::advance(VERIFIED_TTL).await;
    let found = walk(&store, root, &trees).await.expect("walk");
    assert_eq!(found.absent, vec![lost]);
    assert_eq!((found.dirs_read, found.dirs_skipped), (0, 0));
    assert_eq!(store.ask_calls(), asks + 1);
    assert_eq!(trees.len(), 0, "a loss clears what was verified");

    let again = walk(&store, root, &trees).await.expect("walk");
    assert_eq!(again.absent, vec![lost], "not recorded as verified");
    assert_eq!(store.ask_calls(), asks + 2);
}

/// Catches: Directories taken from the content cache that are not recorded in the
/// verified memory, so a walk answered through the cache leaves nothing for the next to
/// skip. Walked at t=0 (every Directory read and verified), the tree is walked again
/// past the time to live: the memory has expired, every Directory comes from the cache
/// and the store is asked about them, no read. That walk records the tree; a third walk
/// inside the time to live skips it (the root, which is all it need ask about) with no
/// FindMissing and no cache lookup at all.
#[tokio::test(start_paused = true)]
async fn a_walk_answered_through_the_cache_records_its_tree_as_verified() {
    let (tree, root) = chains(3, 3);
    let store = Store::fixed(tree, READ).at_epoch(EPOCH);
    let trees = Trees::default();
    walk(&store, root, &trees).await.expect("walk");

    tokio::time::advance(VERIFIED_TTL).await;
    let (reads, asks) = (store.total_reads(), store.ask_calls());
    let cached = walk(&store, root, &trees).await.expect("walk");
    assert_eq!(
        (cached.dirs_skipped, cached.dir_cache_hits, cached.dirs_read),
        (0, 10, 0),
        "expired: all taken from the cache"
    );
    assert_eq!(store.ask_calls(), asks + 1, "and asked about");

    tokio::time::advance(Duration::from_secs(1)).await;
    let skipped = walk(&store, root, &trees).await.expect("walk");
    assert!(skipped.dirs_skipped > 0, "{}", skipped.dirs_skipped);
    assert_eq!(
        (skipped.dir_cache_hits, skipped.dirs_present_checked),
        (0, 0),
        "the tree was believed, not taken from the cache again"
    );
    assert_eq!(skipped.dirs_read, 0);
    assert!(skipped.absent.is_empty());
    assert_eq!(store.ask_calls(), asks + 1, "no FindMissing for it");
    assert_eq!(store.total_reads(), reads, "and no read");
}

/// Catches: a failure (a Directory the store cannot read) below a lost cached Directory
/// reported instead of the loss, and one below a kept cached Directory swallowed. `a`
/// names `c`, whose read fails; walking `a` caches `a` and fails. Under a root with
/// another Directory the next walk fails with `c` while `a` is held, and reports `a`
/// lost, with no error, once it is gone.
#[tokio::test(start_paused = true)]
async fn a_failure_below_a_lost_cached_directory_is_not_the_answer() {
    let mut tree = Tree::default();
    let c = tree.add(dir(&[file("c")], &[]));
    tree.fail.insert(c, Fail::Internal);
    let a = tree.add(dir(&[file("a")], &[c]));
    let b = tree.add(dir(&[file("b")], &[]));
    let root = tree.add(dir(&[], &[a, b]));
    tree.held.extend([file("a"), file("b"), file("c")]);
    let store = Store::fixed(tree, READ);
    let trees = Trees::default();
    assert!(walk(&store, a, &trees).await.is_err(), "c cannot be read");

    let kept = walk(&store, root, &trees)
        .await
        .expect_err("c is under a kept `a`");
    assert!(kept.to_string().contains("disk"), "{kept}");

    store.lose(a);
    let lost = walk(&store, root, &trees)
        .await
        .expect("a failure under a lost `a` is not seen");
    assert_eq!(lost.absent, vec![a]);
    assert_eq!(
        lost.dir_cache_hits, 3,
        "root, a and b were read by the walks before"
    );
}

// ---- cost ----

/// Catches: a warm walk that reads a Directory it has cached, one that asks about each
/// cached Directory in a call of its own (a thousand round trips), and one that does
/// not ask at all. The tree of about 1050 Directories is walked, then walked again:
/// no store read, one FindMissing for the 1050 files and 1050 Directories together, and
/// the time of one such call (the first walk takes its levels as well).
#[tokio::test(start_paused = true)]
async fn a_fully_cached_tree_costs_no_reads_and_one_ask() {
    let (tree, root) = thousand_dirs();
    let store = Store::fixed(tree, READ).with_asks(READ);
    let trees = Trees::default();
    let first = walk(&store, root, &trees).await.expect("walk");
    let dirs = first.dirs_read;
    assert!(dirs >= 1000, "{dirs}");
    assert_eq!(first.files_checked, dirs);
    let (reads, asks) = (store.total_reads(), store.ask_calls());
    assert_eq!((first.dir_cache_hits, first.dirs_present_checked), (0, 0));

    let started = Instant::now();
    let second = walk(&store, root, &trees).await.expect("walk");
    assert_eq!(started.elapsed(), READ, "one round trip");
    assert!(second.absent.is_empty());
    assert_eq!(store.total_reads(), reads);
    assert_eq!(store.ask_calls(), asks + 1);
    assert_eq!(second.dirs_read, 0);
    assert_eq!(second.dir_cache_hits, dirs);
    assert_eq!(second.dirs_present_checked, dirs);
    assert_eq!(second.files_checked, dirs);
    assert_eq!(second.file_asks, 1);
    assert_eq!(*store.ask_sizes().last().unwrap(), 2 * dirs as usize);
}

/// Catches: the Directories' digests put to FindMissing in a message of any size (a
/// tree of tens of thousands of Directories in one call), or in calls that are not the
/// ones the files fill: the batches are those of the files, [`FILES_PER_ASK`] a call,
/// the cached Directories in with them. A root over 5000 Directories of one file each is
/// 10001 digests with the root: three calls of 4096, 4096 and 1809.
#[tokio::test(start_paused = true)]
async fn cached_directories_are_asked_in_the_batches_of_the_files() {
    let mut tree = Tree::default();
    let kids: Vec<Digest> = (0..5000)
        .map(|n| {
            let name = file(&format!("kid {n}"));
            tree.held.insert(name);
            tree.add(dir(&[name], &[]))
        })
        .collect();
    let root = tree.add(dir(&[], &kids));
    let store = Store::fixed(tree, Duration::ZERO);
    let trees = Trees::default();
    walk(&store, root, &trees).await.expect("walk");
    let before = store.ask_calls();

    let warm = walk(&store, root, &trees).await.expect("walk");
    assert!(warm.absent.is_empty());
    let asks = store.ask_sizes()[before..].to_vec();
    assert_eq!(asks.len(), 3);
    assert!(asks.iter().all(|n| *n <= FILES_PER_ASK), "{asks:?}");
    assert_eq!(asks.iter().sum::<usize>(), 10_001);
    assert_eq!(warm.file_asks, 3);
    assert_eq!(warm.dirs_present_checked, 5001);
    assert_eq!(warm.files_checked, 5000);
}

// ---- the bound ----

/// Catches: a walk that lets the cache grow past its bytes, and eviction not counted.
/// The tree is walked into a cache of room for about half of it: the bytes held never
/// exceed the bound, what was pushed out plus what is held is what was read, and the
/// walk reports both.
#[tokio::test(start_paused = true)]
async fn the_cache_holds_to_its_byte_bound_while_a_walk_fills_it() {
    let (tree, root) = chains(20, 6);
    let total = cost_of(&tree);
    let bound = total / 2;
    let store = Store::fixed(tree, Duration::ZERO);
    let trees = Trees::default().with_dir_cache(bound);
    let walked = walk(&store, root, &trees).await.expect("walk");
    assert_eq!(walked.dirs_read, 121);
    assert!(
        walked.dir_cache_bytes <= bound,
        "{} > {bound}",
        walked.dir_cache_bytes
    );
    assert!(walked.dir_cache_bytes > bound / 2, "and not left empty");
    assert!(walked.dir_cache_evicted > 0);
    assert_eq!(
        walked.dir_cache_evicted + trees.dirs.len() as u64,
        walked.dirs_read,
        "everything read is held or was pushed out"
    );
    assert_eq!(walked.dir_cache_bytes, trees.dirs.bytes());

    let roomy = Trees::default().with_dir_cache(total);
    let store = Store::fixed(chains(20, 6).0, Duration::ZERO);
    let whole = walk(&store, root, &roomy).await.expect("walk");
    assert_eq!((whole.dir_cache_evicted, whole.dir_cache_bytes), (0, total));
}

// ---- sharing ----

/// Catches: concurrent Executes that each read a Directory the others are reading (the
/// cache without flights: here the store does not merge reads itself). Eight walks of
/// the same tree start together: every Directory is read once in all, each walk
/// answers cleanly, and each Directory reaches the seven walks that did not read it
/// either from the read they waited for or, if they came later, from the cache
/// (asked about, not trusted).
#[tokio::test(start_paused = true)]
async fn concurrent_walks_share_one_read_of_each_directory() {
    let (tree, root) = thousand_dirs();
    let store = Store::fixed(tree, READ);
    let trees = Trees::default();
    let walks = timeout(
        Duration::from_secs(600),
        join_all((0..8).map(|_| walk(&store, root, &trees))),
    )
    .await
    .expect("no walk is left waiting on a read that ended");
    let walks: Vec<Walked> = walks.into_iter().map(|w| w.expect("walk")).collect();
    let dirs = walks.iter().map(|w| w.dirs_read).sum::<u64>();
    assert!(dirs >= 1000);
    assert_eq!(store.most_reads_of_one(), 1);
    assert_eq!(store.total_reads() as u64, dirs);
    assert!(walks.iter().all(|w| w.absent.is_empty()));
    let sum = |f: fn(&Walked) -> u64| walks.iter().map(f).sum::<u64>();
    assert_eq!(sum(|w| w.dir_cache_misses), dirs, "one lead per Directory");
    assert_eq!(
        sum(|w| w.dir_cache_hits),
        7 * dirs,
        "every other walk took each once"
    );
    // A walk that reaches a Directory while it is read waits for the read; one that
    // reaches it after takes it from the cache and asks the store about it.
    assert_eq!(
        sum(|w| w.dir_cache_merged) + sum(|w| w.dirs_present_checked),
        7 * dirs
    );
    assert!(sum(|w| w.dir_cache_merged) > 0, "some waited on a read");
    assert_eq!(sum(|w| w.read_times.len() as u64), dirs);
}

/// Catches: a walk waiting for ever on a read that its owner dropped (a client that
/// cancelled), and one that takes the dropped read's silence for an answer. The first
/// walk is dropped half way through its first read; the second, which was waiting on it,
/// takes over and finishes, having read the root itself.
///
/// A waiter that never settles must fail an assertion, not hang: when the dropped read
/// leaves its flight behind, the waiter finds the flight's gate closed again and again
/// without the clock moving, so a `timeout` on it never fires. The walks are driven by
/// hand instead: polled, the clock advanced a quarter of a read, and polled again, for
/// at most 25 reads of virtual time (the walk needs about 5), then asserted finished.
#[tokio::test(start_paused = true)]
async fn a_walk_waiting_on_a_read_that_is_dropped_reads_it_itself() {
    let (tree, root) = chains(3, 3);
    let store = Store::fixed(tree, READ);
    let trees = Trees::default();
    let leader = timeout(READ / 2, walk(&store, root, &trees));
    let follower = async {
        sleep(Duration::from_millis(1)).await;
        walk(&store, root, &trees).await
    };
    let both = async { tokio::join!(leader, follower) };
    tokio::pin!(both);
    let mut done = None;
    for _ in 0..100 {
        if let std::task::Poll::Ready(ended) = futures::poll!(both.as_mut()) {
            done = Some(ended);
            break;
        }
        tokio::time::advance(READ / 4).await;
    }
    let (leader, follower) = done.expect("the waiting walk reads the dropped Directory itself");
    assert!(leader.is_err(), "dropped half way through the read");
    let follower = follower.expect("walk");
    assert!(follower.absent.is_empty());
    assert_eq!(follower.dirs_read, 10);
    assert_eq!(
        store.reads_of(&root),
        2,
        "the dropped read, and the one that took over"
    );
}

/// Catches: a failed read taken for the answer of everyone waiting on it, or a failed
/// read cached. Three walks reach a Directory the store has lost: each reports it
/// absent, and nothing is cached for it.
#[tokio::test(start_paused = true)]
async fn a_read_that_fails_is_not_cached_and_every_walk_sees_it() {
    let mut tree = Tree::default();
    let gone = tree.add(dir(&[file("gone")], &[]));
    tree.fail.insert(gone, Fail::Gone);
    let root = tree.add(dir(&[], &[gone]));
    let store = Store::fixed(tree, READ);
    let trees = Trees::default();
    let walks = timeout(
        Duration::from_secs(600),
        join_all((0..3).map(|_| walk(&store, root, &trees))),
    )
    .await
    .expect("no walk is left waiting on a read that ended");
    for walked in walks {
        assert_eq!(walked.expect("walk").absent, vec![gone]);
    }
    assert_eq!(trees.dirs.len(), 1, "only the root");
}

// ---- what the walk reports ----

/// Catches the figures of a walk fed from one another: the reads led, the Directories
/// taken without a read, the ones put to FindMissing, the read times, the bytes. A cold
/// walk of a tree with one Directory the store has lost: 3 reads led (one fails and is
/// timed, not counted read), nothing cached to take; the warm walk takes the two cached
/// and asks about them, leads one read (the lost Directory again), and times it.
#[tokio::test(start_paused = true)]
async fn the_figures_of_a_walk_are_distinct_and_exact() {
    let mut tree = Tree::default();
    let ok = tree.add(dir(&[file("ok")], &[]));
    let gone = tree.add(dir(&[file("gone")], &[]));
    tree.fail.insert(gone, Fail::Gone);
    let root = tree.add(dir(&[file("root")], &[ok, gone]));
    tree.held.extend([file("ok"), file("root")]);
    let held = cost_of(&Tree {
        blobs: [root, ok]
            .iter()
            .map(|d| (*d, tree.blobs[d].clone()))
            .collect(),
        ..Tree::default()
    });
    let store = Store::fixed(tree, READ);
    let trees = Trees::default();

    let cold = walk(&store, root, &trees).await.expect("walk");
    assert_eq!(cold.absent, vec![gone]);
    assert_eq!(cold.dirs_read, 2);
    assert_eq!(cold.dir_cache_misses, 3);
    assert_eq!((cold.dir_cache_hits, cold.dir_cache_merged), (0, 0));
    assert_eq!(cold.dirs_present_checked, 0);
    assert_eq!(
        cold.read_times,
        vec![READ; 3],
        "the failed read is timed too"
    );
    assert_eq!((cold.dir_cache_evicted, cold.dir_cache_bytes), (0, held));
    assert_eq!((cold.files_checked, cold.file_asks), (2, 1));

    let warm = walk(&store, root, &trees).await.expect("walk");
    assert_eq!(warm.absent, vec![gone]);
    assert_eq!(warm.dirs_read, 0);
    assert_eq!(
        warm.dir_cache_misses, 1,
        "the lost Directory is read again, and fails"
    );
    assert_eq!((warm.dir_cache_hits, warm.dir_cache_merged), (2, 0));
    assert_eq!(warm.dirs_present_checked, 2);
    assert_eq!(warm.read_times, vec![READ]);
    assert_eq!(warm.dir_cache_bytes, held);
    assert_eq!((warm.files_checked, warm.file_asks), (2, 1));
}

// ---- equivalence ----

/// A small generator, so a failing seed can be rerun.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// Catches: an answer that differs because the content is cached. 300 random trees
/// with every hazard (Directories gone, unreadable, failing, not Directories; files
/// missing) are walked to fill the cache; then about a sixth of the Directories and of
/// the files are lost; the cached walk, at three bounds on reads, answers what a
/// serial walk of the changed store does: the same absent Directories and files, the
/// same error. A cached Directory is trusted for what it names only, never for being
/// held.
#[tokio::test(start_paused = true)]
async fn a_warm_cache_answers_what_a_serial_walk_of_the_changed_store_does() {
    let (mut pruned, mut failed, mut absent_seen) = (0, 0, 0);
    for seed in 1..=300u64 {
        let (tree, root) = random(seed);
        let mut blobs: Vec<Digest> = tree.blobs.keys().copied().collect();
        let mut files: Vec<Digest> = tree.held.iter().copied().collect();
        blobs.sort_unstable();
        files.sort_unstable();
        let store = Store::new(clone_tree(&tree), move |d| {
            Duration::from_millis(u64::from(d.hash_hex().as_bytes()[0]) % 13)
        });
        let trees = Trees::default();
        let _ = walk_bounded(&store, root, 5, &trees).await;

        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        for gone in blobs.iter().chain(&files) {
            if rng.below(6) == 0 {
                store.lose(*gone);
            }
        }
        let want = serial(&store, root).await.map(|(absent, _)| absent);
        match &want {
            Err(_) => failed += 1,
            Ok(absent) if !absent.is_empty() => absent_seen += 1,
            Ok(_) => {}
        }
        for limit in [1, 5, DIRS_IN_FLIGHT] {
            let got = walk_bounded(&store, root, limit, &trees)
                .await
                .map(|w| {
                    pruned += usize::from(w.dirs_present_checked > 0 && !w.absent.is_empty());
                    w.absent
                })
                .map_err(|e| e.to_string());
            assert_eq!(got, want, "seed {seed} limit {limit}");
        }
    }
    assert!(failed > 20, "the trees must fail often enough: {failed}");
    assert!(absent_seen > 50, "and report absent: {absent_seen}");
    assert!(pruned > 100, "through cached Directories: {pruned}");
}

// ---- figures ----

/// How long a read takes in the figures: always `base`, or `base` until more than
/// `threshold` walks are open at once and `spike` while they are (the farm showed
/// 430 ms a read under 255 concurrent leases against 20 ms otherwise).
#[derive(Clone, Copy)]
enum Latency {
    Flat(Duration),
    Loaded {
        base: Duration,
        spike: Duration,
        threshold: usize,
    },
}

#[derive(Debug)]
struct Figures {
    reads: usize,
    asks: usize,
    /// The digests those calls named.
    digests: usize,
    mean: Duration,
    p95: Duration,
}

/// A build of `actions` Executes over one toolchain tree of about 1050 Directories and a
/// private source subtree of 13 each, `per_wave` arriving 25 ms apart every `gap` (a
/// compile burst, then a pause). `cached` walks with the content cache; otherwise
/// without it (the tip). Both have the verified memory (45 s) and a store that merges
/// concurrent reads of one digest (the router's flights).
async fn build(
    actions: usize,
    per_wave: usize,
    gap: Duration,
    latency: Latency,
    cached: bool,
) -> Figures {
    let (tree, roots) = action_set(actions);
    let open = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_open = std::sync::Arc::clone(&open);
    let store = Store::new(tree, move |_| match latency {
        Latency::Flat(base) => base,
        Latency::Loaded {
            base,
            spike,
            threshold,
        } => {
            if seen_open.load(std::sync::atomic::Ordering::SeqCst) > threshold {
                spike
            } else {
                base
            }
        }
    })
    .with_asks(Duration::from_millis(20))
    .with_flights()
    .at_epoch(EPOCH);
    let trees = Trees::default().with_dir_cache(if cached {
        DIR_CACHE_BYTES_FOR_FIGURES
    } else {
        0
    });
    let waits = timeout(
        Duration::from_secs(24 * 3600),
        join_all(roots.iter().enumerate().map(|(n, root)| {
            let (store, trees, open) = (&store, &trees, &open);
            async move {
                let at =
                    gap * (n / per_wave) as u32 + Duration::from_millis(25) * (n % per_wave) as u32;
                sleep(at).await;
                let began = Instant::now();
                open.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                walk(store, *root, trees).await.expect("walk");
                open.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                began.elapsed()
            }
        })),
    )
    .await
    .expect("no walk is left waiting on a read that ended");
    let mut sorted = waits.clone();
    sorted.sort_unstable();
    Figures {
        reads: store.total_reads(),
        asks: store.ask_calls(),
        digests: store.total_asked(),
        mean: waits.iter().sum::<Duration>() / waits.len() as u32,
        p95: sorted[(sorted.len() * 95).div_ceil(100) - 1],
    }
}

/// The size the figures give the cache: the production one.
const DIR_CACHE_BYTES_FOR_FIGURES: u64 = crate::dir_cache::DIR_CACHE_BYTES;

async fn figures(actions: usize, per_wave: usize, gap: Duration) {
    let models = [
        ("20 ms a read", Latency::Flat(Duration::from_millis(20))),
        (
            "20 ms, 430 ms above 16 open walks",
            Latency::Loaded {
                base: Duration::from_millis(20),
                spike: Duration::from_millis(430),
                threshold: 16,
            },
        ),
    ];
    for (name, latency) in models {
        let tip = build(actions, per_wave, gap, latency, false).await;
        let now = build(actions, per_wave, gap, latency, true).await;
        println!(
            "{actions} actions in waves of {per_wave} every {gap:?}, {name}: \
             before: {} store reads, {} FindMissing ({} digests), presubmit mean {:?} p95 {:?} | \
             after: {} store reads, {} FindMissing ({} digests), presubmit mean {:?} p95 {:?}",
            tip.reads,
            tip.asks,
            tip.digests,
            tip.mean,
            tip.p95,
            now.reads,
            now.asks,
            now.digests,
            now.mean,
            now.p95
        );
        assert!(now.reads < tip.reads, "{name}: {now:?} vs {tip:?}");
        assert!(now.mean <= tip.mean, "{name}: {now:?} vs {tip:?}");
    }
}

/// Catches: a cache that does not cut the store's Directory reads of a build whose
/// bursts are further apart than the verified memory lasts (45 s). Prints the figures
/// with `--nocapture`.
#[tokio::test(start_paused = true)]
async fn a_build_in_bursts_reads_its_toolchain_once() {
    figures(120, 20, Duration::from_secs(60)).await;
}

/// The 482 Executes of the build that was measured, in twelve bursts a minute apart:
/// `cargo test -- --ignored --nocapture`.
#[tokio::test(start_paused = true)]
#[ignore = "the figures of the pull request, a few seconds of CPU"]
async fn the_measured_build_with_and_without_the_content_cache() {
    figures(482, 41, Duration::from_secs(60)).await;
}
