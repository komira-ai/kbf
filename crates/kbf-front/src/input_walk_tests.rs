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
pub(super) const READ: Duration = Duration::from_millis(56);

pub(super) fn proto(d: &Digest) -> kbf_proto::reapi::Digest {
    crate::digest_to_proto(d)
}

pub(super) fn sha(bytes: &[u8]) -> Digest {
    kbf_segments::sha256(bytes)
}

pub(super) fn file(name: &str) -> Digest {
    sha(name.as_bytes())
}

/// A Directory naming `files` and `dirs`: its digest and bytes.
pub(super) fn dir(files: &[Digest], dirs: &[Digest]) -> (Digest, Bytes) {
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
pub(super) enum Fail {
    Gone,
    Unreadable,
    Internal,
}

/// The data of a store: Directory blobs, the reads that fail, the files it holds.
#[derive(Default)]
pub(super) struct Tree {
    pub(super) blobs: HashMap<Digest, Bytes>,
    pub(super) fail: HashMap<Digest, Fail>,
    pub(super) held: HashSet<Digest>,
}

impl Tree {
    pub(super) fn add(&mut self, (digest, bytes): (Digest, Bytes)) -> Digest {
        self.blobs.insert(digest, bytes);
        digest
    }
}

/// A store over a [`Tree`] that takes a set time per read and counts what it is asked.
pub(super) struct Store {
    tree: Tree,
    delay: Box<dyn Fn(&Digest) -> Duration + Send + Sync>,
    open: AtomicUsize,
    max_open: AtomicUsize,
    reads: Mutex<BTreeMap<Digest, usize>>,
    asks: Mutex<Vec<usize>>,
    /// FindMissing calls open now, and the most at once.
    asks_open: AtomicUsize,
    max_asks_open: AtomicUsize,
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
    pub(super) fn new(
        tree: Tree,
        delay: impl Fn(&Digest) -> Duration + Send + Sync + 'static,
    ) -> Self {
        Self {
            tree,
            delay: Box::new(delay),
            open: AtomicUsize::new(0),
            max_open: AtomicUsize::new(0),
            reads: Mutex::new(BTreeMap::new()),
            asks: Mutex::new(Vec::new()),
            asks_open: AtomicUsize::new(0),
            max_asks_open: AtomicUsize::new(0),
            epoch: Mutex::new(None),
            removed: Mutex::new(HashSet::new()),
            ask_delay: Duration::ZERO,
            share: None,
        }
    }

    pub(super) fn with_asks(mut self, delay: Duration) -> Self {
        self.ask_delay = delay;
        self
    }

    pub(super) fn with_flights(mut self) -> Self {
        self.share = Some(Mutex::new(HashMap::new()));
        self
    }

    pub(super) fn at_epoch(self, epoch: u64) -> Self {
        self.set_epoch(Some(epoch));
        self
    }

    pub(super) fn set_epoch(&self, epoch: Option<u64>) {
        *self.epoch.lock().unwrap() = epoch;
    }

    /// A file the store held is gone (evicted, lost).
    pub(super) fn lose(&self, file: Digest) {
        self.removed.lock().unwrap().insert(file);
    }

    pub(super) fn total_reads(&self) -> usize {
        self.reads.lock().unwrap().values().sum()
    }

    pub(super) fn total_asked(&self) -> usize {
        self.asks.lock().unwrap().iter().sum()
    }

    pub(super) fn fixed(tree: Tree, delay: Duration) -> Self {
        Self::new(tree, move |_| delay)
    }

    pub(super) fn max_asks_open(&self) -> usize {
        self.max_asks_open.load(Ordering::SeqCst)
    }

    pub(super) fn max_open(&self) -> usize {
        self.max_open.load(Ordering::SeqCst)
    }

    pub(super) fn reads_of(&self, digest: &Digest) -> usize {
        self.reads.lock().unwrap().get(digest).copied().unwrap_or(0)
    }
}

/// Counts a read as open until it ends or is dropped.
pub(super) struct Open<'a>(&'a AtomicUsize);

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
            let now = self.asks_open.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_asks_open.fetch_max(now, Ordering::SeqCst);
            let _open = Open(&self.asks_open);
            sleep(self.ask_delay).await;
            Ok(digests
                .iter()
                .filter(|d| {
                    !self.tree.held.contains(*d) || self.removed.lock().unwrap().contains(*d)
                })
                .copied()
                .collect())
        })
    }
}

/// Chains of `depth` Directories under a root, `width` of them side by side: level k of
/// every chain is one Directory, so a level is `width` reads and the tree has
/// `1 + depth * width` Directories.
pub(super) fn chains(width: usize, depth: usize) -> (Tree, Digest) {
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
pub(super) async fn level_walk(storage: &dyn Blobs, root: Digest, limit: usize) -> u64 {
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
pub(super) async fn serial(storage: &Store, root: Digest) -> Result<(Vec<Digest>, u64), String> {
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
    let walked = walk_bounded(&store, root, 8, &Trees::default())
        .await
        .expect("walk");
    assert_eq!(walked.dirs_read, 201);
    assert_eq!(
        store.max_open(),
        8,
        "exactly the bound, not serial, not more"
    );

    let (tree, root) = chains(200, 1);
    let store = Store::fixed(tree, Duration::from_millis(10));
    walk(&store, root, &Trees::default()).await.expect("walk");
    assert_eq!(store.max_open(), DIRS_IN_FLIGHT);
    const { assert!(DIRS_IN_FLIGHT > 1) };
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
pub(super) struct Rng(u64);

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
pub(super) fn random(seed: u64) -> (Tree, Digest) {
    random_with(seed, true)
}

/// [`random`], or with `hazards` false a tree with nothing wrong in it: every Directory
/// readable and decoding, every file held.
pub(super) fn random_with(seed: u64, hazards: bool) -> (Tree, Digest) {
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

pub(super) fn clone_tree(t: &Tree) -> Tree {
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

/// Catches: FindMissing calls issued one at a time (a tree of many thousand files pays a
/// round trip per 4096 of them in series), and ones issued without a bound (a few
/// hundred thousand files opening dozens of calls on one node). With 13 chunks, each
/// taking a second, exactly [`ASKS_IN_FLIGHT`] (4) are open at once, and the whole takes
/// four rounds (4, 4, 4, 1).
#[tokio::test(start_paused = true)]
async fn file_asks_in_flight_reach_the_bound_and_stay_there() {
    // 13 chunks: three full rounds of the bound and one more. The bound itself is
    // asserted as a number beside the constant: a test written only in terms of the
    // constant passes for any value of it (serial, with 4 chunks, also takes 4 s).
    let chunks = 13;
    assert_eq!(ASKS_IN_FLIGHT, 4, "the bound the sizing above rests on");
    let mut tree = Tree::default();
    let files: Vec<Digest> = (0..chunks * FILES_PER_ASK)
        .map(|n| file(&format!("file {n}")))
        .collect();
    let root = tree.add(dir(&files, &[]));
    tree.held.extend(files.iter().copied());
    let store = Store::fixed(tree, Duration::ZERO).with_asks(Duration::from_secs(1));
    let started = Instant::now();
    let walked = walk(&store, root, &Trees::default()).await.expect("walk");
    assert!(walked.absent.is_empty());
    assert_eq!(walked.file_asks, chunks as u64);
    assert_eq!(store.asks.lock().unwrap().len(), chunks);
    assert_eq!(
        store.max_asks_open(),
        ASKS_IN_FLIGHT,
        "exactly the bound: not serial, not more"
    );
    assert_eq!(
        started.elapsed(),
        Duration::from_secs(4),
        "4 rounds of 4, 4, 4, 1"
    );
}

/// A tree shaped like the test farm's: about 1050 Directories in 11 levels, a file in
/// each, every Directory with one parent.
pub(super) fn thousand_dirs() -> (Tree, Digest) {
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

    let reachable = serial(&timed(clone_tree(&tree)), root)
        .await
        .expect("serial")
        .1;
    let started = Instant::now();
    serial(&timed(clone_tree(&tree)), root)
        .await
        .expect("serial");
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
