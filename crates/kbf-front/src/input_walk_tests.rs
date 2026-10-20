//! The input walk over a store: how many reads it keeps open, how long a tree takes,
//! that scheduling cannot change what it answers, and that a digest is read once.
//! Time is tokio's paused clock, so a "56 ms read" costs no real time and every elapsed
//! figure is exact.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures::future::BoxFuture;
use kbf_proto::reapi::{Directory, DirectoryNode, FileNode};
use kbf_types::Digest;
use prost::Message as _;
use tokio::time::{Instant, sleep};

use super::*;

/// What one read costs a tree like the test farm's (milliseconds).
const READ: Duration = Duration::from_millis(56);

fn proto(d: &Digest) -> kbf_proto::reapi::Digest {
    crate::digest_to_proto(d)
}

fn sha(bytes: &[u8]) -> Digest {
    kbf_segments::sha256(bytes)
}

fn file(name: &str) -> Digest {
    sha(name.as_bytes())
}

/// A Directory naming `files` and `dirs`: its digest and bytes.
fn dir(files: &[Digest], dirs: &[Digest]) -> (Digest, Bytes) {
    let message = Directory {
        files: files
            .iter()
            .map(|d| FileNode {
                name: d.hash_hex(),
                digest: Some(proto(d)),
                ..FileNode::default()
            })
            .collect(),
        directories: dirs
            .iter()
            .map(|d| DirectoryNode {
                name: d.hash_hex(),
                digest: Some(proto(d)),
            })
            .collect(),
        ..Directory::default()
    };
    let bytes = Bytes::from(message.encode_to_vec());
    (sha(&bytes), bytes)
}

/// How a read of a Directory the tree names goes wrong.
#[derive(Clone, Copy, Debug)]
enum Fail {
    Gone,
    Unreadable,
    Internal,
}

/// The data of a store: Directory blobs, the reads that fail, the files it holds.
#[derive(Default)]
struct Tree {
    blobs: HashMap<Digest, Bytes>,
    fail: HashMap<Digest, Fail>,
    held: HashSet<Digest>,
}

impl Tree {
    fn add(&mut self, (digest, bytes): (Digest, Bytes)) -> Digest {
        self.blobs.insert(digest, bytes);
        digest
    }
}

/// A store over a [`Tree`] that takes a set time per read and counts what it is asked.
struct Store {
    tree: Tree,
    delay: Box<dyn Fn(&Digest) -> Duration + Send + Sync>,
    open: AtomicUsize,
    max_open: AtomicUsize,
    reads: Mutex<BTreeMap<Digest, usize>>,
    asks: Mutex<Vec<usize>>,
    /// What [`Blobs::epoch`] answers: `None` (the default) remembers nothing.
    epoch: Mutex<Option<u64>>,
    /// Files that have gone since the store was made: held by `tree`, missing now.
    removed: Mutex<HashSet<Digest>>,
    /// How long a FindMissing takes.
    ask_delay: Duration,
    /// Model the router's read flights: a read of a digest that is being read waits for
    /// that read to end instead of starting its own (the first read of a digest is
    /// counted, a joined one is not).
    share: Option<Mutex<HashMap<Digest, Instant>>>,
}

impl Store {
    fn new(tree: Tree, delay: impl Fn(&Digest) -> Duration + Send + Sync + 'static) -> Self {
        Self {
            tree,
            delay: Box::new(delay),
            open: AtomicUsize::new(0),
            max_open: AtomicUsize::new(0),
            reads: Mutex::new(BTreeMap::new()),
            asks: Mutex::new(Vec::new()),
            epoch: Mutex::new(None),
            removed: Mutex::new(HashSet::new()),
            ask_delay: Duration::ZERO,
            share: None,
        }
    }

    fn with_asks(mut self, delay: Duration) -> Self {
        self.ask_delay = delay;
        self
    }

    fn with_flights(mut self) -> Self {
        self.share = Some(Mutex::new(HashMap::new()));
        self
    }

    fn at_epoch(self, epoch: u64) -> Self {
        self.set_epoch(Some(epoch));
        self
    }

    fn set_epoch(&self, epoch: Option<u64>) {
        *self.epoch.lock().unwrap() = epoch;
    }

    /// A file the store held is gone (evicted, lost).
    fn lose(&self, file: Digest) {
        self.removed.lock().unwrap().insert(file);
    }

    fn total_reads(&self) -> usize {
        self.reads.lock().unwrap().values().sum()
    }

    fn total_asked(&self) -> usize {
        self.asks.lock().unwrap().iter().sum()
    }

    fn fixed(tree: Tree, delay: Duration) -> Self {
        Self::new(tree, move |_| delay)
    }

    fn max_open(&self) -> usize {
        self.max_open.load(Ordering::SeqCst)
    }

    fn reads_of(&self, digest: &Digest) -> usize {
        self.reads.lock().unwrap().get(digest).copied().unwrap_or(0)
    }
}

/// Counts a read as open until it ends or is dropped.
struct Open<'a>(&'a AtomicUsize);

impl Drop for Open<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Blobs for Store {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<Bytes>> {
        Box::pin(async move {
            let began = Instant::now();
            let ends = match &self.share {
                Some(flights) => {
                    let mut flights = flights.lock().unwrap();
                    match flights.get(digest) {
                        Some(end) if *end > began => *end,
                        _ => {
                            let end = began + (self.delay)(digest);
                            flights.insert(*digest, end);
                            *self.reads.lock().unwrap().entry(*digest).or_default() += 1;
                            end
                        }
                    }
                }
                None => {
                    *self.reads.lock().unwrap().entry(*digest).or_default() += 1;
                    began + (self.delay)(digest)
                }
            };
            let now = self.open.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_open.fetch_max(now, Ordering::SeqCst);
            let _open = Open(&self.open);
            tokio::time::sleep_until(ends).await;
            match self.tree.fail.get(digest) {
                Some(Fail::Gone) => Err(StorageError::NotFound(*digest)),
                Some(Fail::Unreadable) => Err(StorageError::Unreachable(*digest)),
                Some(Fail::Internal) => Err(StorageError::Internal(format!("disk {digest}"))),
                None => self
                    .tree
                    .blobs
                    .get(digest)
                    .cloned()
                    .ok_or(StorageError::NotFound(*digest)),
            }
        })
    }

    fn epoch(&self) -> Option<u64> {
        *self.epoch.lock().unwrap()
    }

    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        Box::pin(async move {
            self.asks.lock().unwrap().push(digests.len());
            sleep(self.ask_delay).await;
            Ok(digests
                .iter()
                .filter(|d| !self.tree.held.contains(*d) || self.removed.lock().unwrap().contains(*d))
                .copied()
                .collect())
        })
    }
}

/// Chains of `depth` Directories under a root, `width` of them side by side: level k of
/// every chain is one Directory, so a level is `width` reads and the tree has
/// `1 + depth * width` Directories.
fn chains(width: usize, depth: usize) -> (Tree, Digest) {
    let mut tree = Tree::default();
    let heads: Vec<Digest> = (0..width)
        .map(|w| {
            let mut below: Option<Digest> = None;
            for level in (0..depth).rev() {
                let name = file(&format!("chain {w} level {level}"));
                tree.held.insert(name);
                let dirs: Vec<Digest> = below.into_iter().collect();
                below = Some(tree.add(dir(&[name], &dirs)));
            }
            below.expect("depth is at least one")
        })
        .collect();
    let root = tree.add(dir(&[], &heads));
    (tree, root)
}

/// The tip's walk before this change, kept to time against: one level at a time, 16
/// reads of a level open, nothing started until the level before it is read.
async fn level_walk(storage: &dyn Blobs, root: Digest, limit: usize) -> u64 {
    use futures::StreamExt as _;
    let (mut seen, mut dirs_read) = (BTreeSet::from([root]), 0);
    let mut files = BTreeSet::new();
    let mut level = vec![root];
    while !level.is_empty() {
        let read: Vec<_> = futures::stream::iter(level)
            .map(|dir| async move { (dir, storage.read_blob(&dir).await) })
            .buffer_unordered(limit)
            .collect()
            .await;
        level = Vec::new();
        for (dir, blob) in read {
            dirs_read += 1;
            let decoded = decode(&dir, &blob.expect("a held directory")).expect("decodes");
            files.extend(decoded.files.iter().copied());
            level.extend(decoded.dirs.iter().copied().filter(|d| seen.insert(*d)));
        }
    }
    let files: Vec<Digest> = files.into_iter().collect();
    for chunk in files.chunks(FILES_PER_ASK) {
        storage.find_missing(chunk).await.expect("asked");
    }
    dirs_read
}

/// One read after another, the way the check reads when nothing is concurrent. Written
/// apart from [`walk_bounded`] as the definition of its answer: the Directories
/// reachable without passing through one that is absent or failed, the failure of the
/// smallest digest, absent directories before absent files.
async fn serial(storage: &Store, root: Digest) -> Result<(Vec<Digest>, u64), String> {
    let mut seen = BTreeSet::from([root]);
    let mut stack = vec![root];
    let (mut absent, mut files, mut read) = (BTreeSet::new(), BTreeSet::new(), 0);
    let mut failure: Option<(Digest, String)> = None;
    while let Some(next) = stack.pop() {
        match storage.read_blob(&next).await {
            Err(StorageError::NotFound(_) | StorageError::Unreachable(_)) => {
                absent.insert(next);
            }
            Err(e) => {
                if failure.as_ref().is_none_or(|(was, _)| next < *was) {
                    failure = Some((next, e.to_string()));
                }
            }
            Ok(bytes) => {
                read += 1;
                match decode(&next, &bytes) {
                    Ok(d) => {
                        files.extend(d.files.iter().copied());
                        for child in d.dirs.iter() {
                            if seen.insert(*child) {
                                stack.push(*child);
                            }
                        }
                    }
                    Err(e) => {
                        if failure.as_ref().is_none_or(|(was, _)| next < *was) {
                            failure = Some((next, StorageError::from(e).to_string()));
                        }
                    }
                }
            }
        }
    }
    if let Some((_, e)) = failure {
        return Err(e);
    }
    let files: Vec<Digest> = files.into_iter().collect();
    let missing: BTreeSet<Digest> = storage
        .find_missing(&files)
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .collect();
    Ok((absent.into_iter().chain(missing).collect(), read))
}

/// Catches: a walk that reads one Directory at a time (it would never have more than one
/// open), and one with no bound (a 200-wide tree would open 200). The count is taken
/// inside the fake's read, so it is what the store sees. Also pins the default.
#[tokio::test(start_paused = true)]
async fn reads_in_flight_reach_the_bound_and_stay_there() {
    let (tree, root) = chains(200, 1);
    let store = Store::fixed(tree, Duration::from_millis(10));
    let walked = walk_bounded(&store, root, 8, &Trees::default()).await.expect("walk");
    assert_eq!(walked.dirs_read, 201);
    assert_eq!(store.max_open(), 8, "exactly the bound, not serial, not more");

    let (tree, root) = chains(200, 1);
    let store = Store::fixed(tree, Duration::from_millis(10));
    walk(&store, root, &Trees::default()).await.expect("walk");
    assert_eq!(store.max_open(), DIRS_IN_FLIGHT);
    assert!(DIRS_IN_FLIGHT > 1);
}

/// Catches: reads of a level that wait for each other (the cost is the tree's size, not
/// its depth). 20 chains of 6 are 121 reads of 56 ms: a serial walk takes 6.8 s, this
/// one the root and six levels, 7 round trips.
#[tokio::test(start_paused = true)]
async fn a_tree_takes_its_depth_in_round_trips_not_its_size() {
    let (width, depth) = (20, 6);
    let (tree, root) = chains(width, depth);
    let store = Store::fixed(tree, READ);
    let started = Instant::now();
    let walked = walk(&store, root, &Trees::default()).await.expect("walk");
    let took = started.elapsed();
    assert_eq!(walked.dirs_read, (1 + width * depth) as u64);
    assert_eq!(took, READ * (depth as u32 + 1));
    assert!(took * 10 < READ * walked.dirs_read as u32, "{took:?}");
}

/// Catches: a walk that starts the next level only when the one before is read. Under
/// the root are one slow Directory (500 ms) and a chain of four fast ones (10 ms): the
/// chain is read while the slow one is, so the walk ends with the slow read (510 ms);
/// level by level it would end at 550.
#[tokio::test(start_paused = true)]
async fn a_slow_read_does_not_hold_up_the_levels_below_its_siblings() {
    let mut tree = Tree::default();
    let slow = tree.add(dir(&[file("slow")], &[]));
    let mut head = tree.add(dir(&[file("c3")], &[]));
    for n in (0..3).rev() {
        head = tree.add(dir(&[file(&format!("c{n}"))], &[head]));
    }
    let root = tree.add(dir(&[], &[slow, head]));
    let store = Store::new(tree, move |d| {
        if *d == slow {
            Duration::from_millis(500)
        } else {
            Duration::from_millis(10)
        }
    });
    let started = Instant::now();
    walk(&store, root, &Trees::default()).await.expect("walk");
    assert_eq!(started.elapsed(), Duration::from_millis(510));
}

/// Catches: a digest read again for every parent that names it, and a repeated subtree
/// walked once per parent. 50 parents name one shared Directory, which names another.
#[tokio::test(start_paused = true)]
async fn a_repeated_subtree_is_read_once() {
    let mut tree = Tree::default();
    let leaf = tree.add(dir(&[file("leaf")], &[]));
    let shared = tree.add(dir(&[file("shared")], &[leaf]));
    let parents: Vec<Digest> = (0..50)
        .map(|n| tree.add(dir(&[file(&format!("parent {n}"))], &[shared, leaf])))
        .collect();
    let root = tree.add(dir(&[], &parents));
    let store = Store::fixed(tree, Duration::from_millis(5));
    let walked = walk(&store, root, &Trees::default()).await.expect("walk");
    assert_eq!(walked.dirs_read, 53, "root, 50 parents, shared, leaf");
    assert_eq!(store.reads_of(&shared), 1);
    assert_eq!(store.reads_of(&leaf), 1);
    assert_eq!(store.reads_of(&root), 1);
    assert!(store.reads.lock().unwrap().values().all(|n| *n == 1));
}

/// A small generator, so a failing seed can be rerun.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A random shared-subtree tree of Directories (each names earlier ones, so a Directory
/// can have many parents), with some Directories gone, unreadable, failing or not a
/// Directory at all, and some files held.
fn random(seed: u64) -> (Tree, Digest) {
    random_with(seed, true)
}

/// [`random`], or with `hazards` false a tree with nothing wrong in it: every Directory
/// readable and decoding, every file held.
fn random_with(seed: u64, hazards: bool) -> (Tree, Digest) {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut tree = Tree::default();
    let mut nodes: Vec<Digest> = Vec::new();
    for n in 0..rng.below(60) + 5 {
        let files: Vec<Digest> = (0..rng.below(5))
            .map(|_| file(&format!("f{}", rng.below(40))))
            .collect();
        let dirs: Vec<Digest> = if nodes.is_empty() {
            Vec::new()
        } else {
            (0..rng.below(5))
                .map(|_| nodes[rng.below(nodes.len())])
                .collect()
        };
        // Salt the name so equal children still give distinct Directories.
        let mut files = files;
        files.push(file(&format!("node {seed} {n}")));
        let digest = if hazards && rng.below(25) == 0 {
            // Bytes that are not a Directory message.
            let junk = Bytes::from(format!("junk \u{ff}\u{ff} {seed} {n}"));
            tree.add((sha(&junk), junk))
        } else {
            tree.add(dir(&files, &dirs))
        };
        let fail = match rng.below(14) {
            _ if !hazards => None,
            0 => Some(Fail::Gone),
            1 => Some(Fail::Unreadable),
            2 if rng.below(3) == 0 => Some(Fail::Internal),
            _ => None,
        };
        if let Some(fail) = fail {
            tree.fail.insert(digest, fail);
        }
        nodes.push(digest);
    }
    for n in 0..40 {
        if !hazards || rng.below(3) != 0 {
            tree.held.insert(file(&format!("f{n}")));
        }
    }
    // Most nodes' own files are held, some are not.
    for n in 0..nodes.len() {
        if !hazards || rng.below(4) != 0 {
            tree.held.insert(file(&format!("node {seed} {n}")));
        }
    }
    let root = *nodes.last().expect("at least five nodes");
    (tree, root)
}

fn clone_tree(t: &Tree) -> Tree {
    Tree {
        blobs: t.blobs.clone(),
        fail: t.fail.clone(),
        held: t.held.clone(),
    }
}

/// Catches: an answer that depends on which read finishes first (the error of whichever
/// failing Directory answered first; a set of absent directories that varies), a
/// Directory read under an absent one, a Directory dropped from the produced set, a
/// corrupt Directory taken for an empty one. 300 random trees with shared subtrees and
/// every kind of failure, 4 bounds, and a read time that differs per Directory so the
/// order of completion is not the order of starting. The reference is a one-at-a-time
/// walk; the answer, the error text and the count of Directories read must be equal.
#[tokio::test(start_paused = true)]
async fn the_answer_is_the_serial_walks_whatever_the_order_of_completion() {
    let (mut failed, mut absent_seen) = (0, 0);
    for seed in 1..=300u64 {
        let (tree, root) = random(seed);
        let want = serial(&Store::fixed(clone_tree(&tree), Duration::ZERO), root).await;
        match &want {
            Err(_) => failed += 1,
            Ok((absent, _)) if !absent.is_empty() => absent_seen += 1,
            Ok(_) => {}
        }
        for limit in [1, 2, 5, DIRS_IN_FLIGHT] {
            let spread = seed % 7 + 1;
            let store = Store::new(clone_tree(&tree), move |d| {
                Duration::from_millis(u64::from(d.hash_hex().as_bytes()[0]) % 13 * spread)
            });
            let got = walk_bounded(&store, root, limit, &Trees::default())
                .await
                .map(|w| (w.absent, w.dirs_read))
                .map_err(|e| e.to_string());
            assert_eq!(got, want, "seed {seed} limit {limit}");
            assert!(store.max_open() <= limit, "seed {seed} limit {limit}");
            assert!(store.reads.lock().unwrap().values().all(|n| *n == 1));
        }
    }
    assert!(failed > 20, "the trees must fail often enough: {failed}");
    assert!(absent_seen > 20, "and report absent: {absent_seen}");
}

/// Catches: files named in one oversized FindMissing, and asks that are not answered in
/// order (the error reported would depend on timing).
#[tokio::test(start_paused = true)]
async fn files_are_asked_in_batches_and_answered_in_order() {
    let mut tree = Tree::default();
    let files: Vec<Digest> = (0..2 * FILES_PER_ASK + 1)
        .map(|n| file(&format!("file {n}")))
        .collect();
    let root = tree.add(dir(&files, &[]));
    let store = Store::fixed(tree, Duration::ZERO);
    let walked = walk(&store, root, &Trees::default()).await.expect("walk");
    assert_eq!(walked.absent.len(), files.len());
    let mut asks = store.asks.lock().unwrap().clone();
    asks.sort_unstable();
    assert_eq!(asks, vec![1, FILES_PER_ASK, FILES_PER_ASK]);
}

/// A tree shaped like the test farm's: about 1050 Directories in 11 levels, a file in
/// each, every Directory with one parent.
fn thousand_dirs() -> (Tree, Digest) {
    let widths = [1usize, 6, 30, 90, 200, 300, 250, 120, 40, 12, 1];
    let mut tree = Tree::default();
    // Built from the deepest level up: Directory m of a level is named by Directory
    // m % width of the level above it, so every Directory has one parent.
    let mut below: Vec<Digest> = Vec::new();
    for (level, width) in widths.iter().enumerate().rev() {
        let mut here = Vec::new();
        for n in 0..*width {
            let name = file(&format!("level {level} dir {n}"));
            tree.held.insert(name);
            let dirs: Vec<Digest> = below
                .iter()
                .enumerate()
                .filter(|(m, _)| m % width == n)
                .map(|(_, d)| *d)
                .collect();
            here.push(tree.add(dir(&[name], &dirs)));
        }
        below = here;
    }
    (tree, below[0])
}

/// The figures of the change on a tree shaped like the test farm's (about 1050
/// Directories in 11 levels, 56 ms a read). Run with `--nocapture` to see them. What it
/// asserts is the order of magnitude, so the figures cannot silently regress.
#[tokio::test(start_paused = true)]
async fn the_tree_of_a_thousand_directories_takes_a_second_not_a_minute() {
    let (tree, root) = thousand_dirs();
    let timed = |tree: Tree| Store::fixed(tree, READ);

    let reachable = serial(&timed(clone_tree(&tree)), root).await.expect("serial").1;
    let started = Instant::now();
    serial(&timed(clone_tree(&tree)), root).await.expect("serial");
    let serial_took = started.elapsed();

    let old = timed(clone_tree(&tree));
    let started = Instant::now();
    assert_eq!(level_walk(&old, root, 16).await, reachable);
    let level_took = started.elapsed();

    let new = timed(clone_tree(&tree));
    let started = Instant::now();
    let walked = walk(&new, root, &Trees::default()).await.expect("walk");
    let new_took = started.elapsed();
    assert_eq!(walked.dirs_read, reachable);

    println!(
        "dirs {reachable}, read {READ:?}: serial {serial_took:?} | level-by-level x16 \
         {level_took:?} (max open {}) | pipelined x{DIRS_IN_FLIGHT} {new_took:?} (max open {})",
        old.max_open(),
        new.max_open()
    );
    assert!(reachable >= 1000, "{reachable}");
    assert!(new_took < level_took && level_took < serial_took);
    assert!(new_took * 20 < serial_took);
}

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
    let (a, b) = (tree.add(dir(&[], &[toolchain, one])), tree.add(dir(&[], &[toolchain, two])));
    tree.held.extend([file("one"), file("two")]);
    let store = Store::fixed(tree, READ).at_epoch(EPOCH);
    let trees = Trees::default();
    walk(&store, a, &trees).await.expect("a");
    let asked = store.total_asked();
    let second = walk(&store, b, &trees).await.expect("b");
    assert_eq!((second.dirs_read, second.dirs_skipped), (2, 1), "b and two; toolchain skipped");
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
    assert_eq!(late.absent, vec![gone], "at the bound the tree is walked again");
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
    assert!(walk(&store, root, &trees).await.expect("walk").absent.is_empty());

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
    assert!(walk(&store, first, &trees).await.expect("walk").absent.is_empty());

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
        if *d == slow_root { Duration::from_secs(5) } else { Duration::from_millis(1) }
    })
    .at_epoch(EPOCH);
    let trees = Trees::default();
    let (slow, quick) = tokio::join!(walk(&store, slow_root, &trees), async {
        sleep(Duration::from_secs(1)).await;
        walk(&store, hole, &trees).await
    });
    assert!(slow.expect("slow").absent.is_empty());
    assert_eq!(quick.expect("quick").absent.len(), 1);
    assert_eq!(trees.len(), 0, "the slow walk began before the loss was seen");
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
    assert_eq!(walk(&store, root, &trees).await.expect("walk").absent.len(), 1);
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
    assert!(trees.fresh(EPOCH, &kept[0], now).is_some(), "used again, so kept");
    assert!(trees.fresh(EPOCH, &kept[1], now).is_none(), "least recently used went");
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
    for (seed, hazards) in (1..=300u64).map(|s| (s, true)).chain((1..=200u64).map(|s| (s, false))) {
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
                assert_eq!(got.as_ref().map(|g| &g.0), want.as_ref().map(|w| &w.0), "seed {seed} round {round}");
                assert_eq!(got.as_ref().err(), want.as_ref().err(), "seed {seed} round {round}");
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
                assert_eq!(reads[0], reads[1], "seed {seed}: nothing kept from an unclean walk");
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
                let width = if level == 2 { 9 } else if level == 1 { 3 } else { 1 };
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
async fn presubmit(read: Duration, gap: Duration, actions: usize, old: bool) -> (Duration, Duration, usize) {
    let (tree, roots) = action_set(actions);
    let store = Store::fixed(tree, read).with_asks(read).with_flights().at_epoch(EPOCH);
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
        for (arrival, gap) in [("all at once", Duration::ZERO), ("one per 100 ms", Duration::from_millis(100))] {
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
