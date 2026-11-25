//! What a walk over a store reports of the shape of its reads: how many reads, each waiting on
//! the one before, its longest path from the root took (`Walked::depth`), and, for a read
//! through packs, its index lookup and its ranged read (`Walked::index_times`,
//! `Walked::object_times`). Time is tokio's paused clock; the fake store is that of the sibling
//! `tests` module.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use kbf_pack::ReadTimes;
use kbf_types::Digest;

use super::tests::{READ, Store, Tree, chains, dir, file};
use super::*;

/// Puts the Directory `digest` of `tree` in the content cache of `trees`, as a walk that read
/// it would have.
fn cache(trees: &Trees, tree: &Tree, digest: Digest) {
    let decoded = decode(&digest, &tree.blobs[&digest]).expect("decodes");
    match trees.dirs().lookup(&digest) {
        Lookup::Lead(lead) => {
            lead.finish(Arc::new(decoded));
        }
        _ => panic!("{digest} is cached or being read"),
    }
}

/// Catches a depth counted in Directories rather than in reads on one path (a wide tree would
/// read as deep), or the shortest path taken for the longest. The root names a chain of four
/// Directories and a single one beside it: five reads in a row on the longest path, though the
/// walk reads six Directories.
#[tokio::test(start_paused = true)]
async fn the_depth_is_the_longest_path_of_reads_from_the_root() {
    let mut tree = Tree::default();
    let mut below = None;
    for level in 0..4 {
        let name = file(&format!("deep {level}"));
        tree.held.insert(name);
        let dirs: Vec<Digest> = below.into_iter().collect();
        below = Some(tree.add(dir(&[name], &dirs)));
    }
    let beside = file("beside");
    tree.held.insert(beside);
    let short = tree.add(dir(&[beside], &[]));
    let root = tree.add(dir(&[], &[below.expect("four levels"), short]));
    let store = Store::fixed(tree, READ);

    let walked = walk(&store, root, &Trees::default()).await.expect("walk");

    assert_eq!(walked.dirs_read, 6);
    assert_eq!(walked.depth, 5);
    assert!(walked.absent.is_empty(), "{:?}", walked.absent);
}

/// Catches a Directory taken from the content cache counted as a round: once a walk has read
/// a tree, the next reads nothing and its depth is 0; with only the root cached, a walk of a
/// chain three Directories deep below it waits on three reads, not four.
#[tokio::test(start_paused = true)]
async fn a_cached_directory_adds_no_round() {
    let (tree, root) = chains(2, 3);
    let root_blob = (root, tree.blobs[&root].clone());
    let store = Store::fixed(tree, READ);
    let trees = Trees::default();

    let cold = walk(&store, root, &trees).await.expect("cold walk");
    let warm = walk(&store, root, &trees).await.expect("warm walk");

    assert_eq!((cold.dirs_read, cold.depth), (7, 4));
    assert_eq!((warm.dirs_read, warm.depth), (0, 0));

    let (tree, again) = chains(2, 3);
    assert_eq!(again, root, "the same tree");
    let store = Store::fixed(tree, READ);
    let trees = Trees::default();
    let mut only_root = Tree::default();
    only_root.add(root_blob);
    cache(&trees, &only_root, root);

    let walked = walk(&store, root, &trees).await.expect("walk");

    assert_eq!((walked.dirs_read, walked.depth), (6, 3));
    assert_eq!(store.reads_of(&root), 0, "the cached root was read");
}

/// Catches a wait for another walk's read left out of the depth: two walks of one chain start
/// together; one reads each Directory, the other takes each from that read, and both waited on
/// the same three reads in a row.
#[tokio::test(start_paused = true)]
async fn a_directory_another_walk_read_is_a_round_of_the_walk_that_waited() {
    let (tree, root) = chains(1, 2);
    let store = Store::fixed(tree, READ);
    let trees = Trees::default();

    let (a, b) = tokio::join!(walk(&store, root, &trees), walk(&store, root, &trees));
    let (a, b) = (a.expect("walk a"), b.expect("walk b"));

    assert_eq!(a.dirs_read + b.dirs_read, 3, "each Directory read once");
    assert_eq!(a.dir_cache_merged + b.dir_cache_merged, 3);
    assert_eq!((a.depth, b.depth), (3, 3));
}

/// Catches a read whose root is absent counted as no round (the walk did wait on it), and an
/// absent Directory's children walked.
#[tokio::test(start_paused = true)]
async fn an_absent_root_is_one_round() {
    let store = Store::fixed(Tree::default(), READ);

    let walked = walk(&store, file("no such root"), &Trees::default())
        .await
        .expect("walk");

    assert_eq!(walked.absent, vec![file("no such root")]);
    assert_eq!(walked.depth, 1);
}

/// A store whose every read says it took a pack index lookup of `INDEX`, and a ranged read of
/// `OBJECT` for the Directories in `packed`.
struct Steps {
    store: Store,
    packed: Vec<Digest>,
}

const INDEX: Duration = Duration::from_millis(3);
const OBJECT: Duration = Duration::from_millis(11);

impl Blobs for Steps {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Answer<Bytes>> {
        self.store.read_blob(digest)
    }

    fn find_missing<'a>(&'a self, digests: &'a [Digest]) -> BoxFuture<'a, Answer<Vec<Digest>>> {
        self.store.find_missing(digests)
    }

    fn read_timed<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, (Answer<Bytes>, ReadTimes)> {
        Box::pin(async move {
            let times = ReadTimes {
                index: Some(INDEX),
                objects: self.packed.contains(digest).then_some(OBJECT),
            };
            (self.store.read_blob(digest).await, times)
        })
    }
}

/// Catches the steps of a read dropped, or taken from another read: a walk that reads three
/// Directories, two of them from a pack, reports three index lookups and two ranged reads with
/// the times the store gave; a Directory taken from the content cache reports neither.
#[tokio::test(start_paused = true)]
async fn each_read_reports_the_steps_the_store_gave_it() {
    let (tree, root) = chains(2, 1);
    let below: Vec<Digest> = tree.blobs.keys().copied().filter(|d| *d != root).collect();
    let store = Steps {
        store: Store::fixed(tree, READ),
        packed: vec![root, below[0]],
    };
    let trees = Trees::default();

    let cold = walk(&store, root, &trees).await.expect("cold walk");
    let warm = walk(&store, root, &trees).await.expect("warm walk");

    assert_eq!(cold.dirs_read, 3);
    assert_eq!(cold.index_times, vec![INDEX; 3]);
    assert_eq!(cold.object_times, vec![OBJECT; 2]);
    assert_eq!(cold.read_times, vec![READ; 3]);
    assert!(warm.index_times.is_empty() && warm.object_times.is_empty());
}
