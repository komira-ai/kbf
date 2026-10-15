//! Unit tests of the classifier's parts: the rule table, the counts, the choice, and the
//! walk's cap.

use std::collections::HashMap;
use std::sync::Arc;

use kbf_proto::reapi::{Digest, Directory, DirectoryNode, FileNode};
use prost::Message as _;

use super::classify::{
    Candidate, Limits, Reserved, Summaries, Summary, load, qualifies, select, summarize, tree_name,
};
use crate::cas::{Cas, CasError, digest_of, label};
use crate::tree::{Loaded, TreeError};

/// A CAS over a map, for the walks.
struct Blobs(HashMap<String, Vec<u8>>);
impl Cas for Blobs {
    type Lease = Arc<Self>;
    fn lease(self: &Arc<Self>, _: &kbf_types::LeaseId) -> Arc<Self> {
        Arc::clone(self)
    }
    async fn get(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        self.0
            .get(&label(digest))
            .cloned()
            .ok_or_else(|| CasError::Missing(label(digest)))
    }
    async fn put(&self, bytes: Vec<u8>) -> Result<Digest, CasError> {
        Ok(digest_of(&bytes))
    }
}

const LIMITS: Limits = Limits {
    min_files: 256,
    max_bytes: 1 << 30,
    max_mounts: 8,
};

fn reserved<'a>(outputs: &'a [String], working_directory: &'a str) -> Reserved<'a> {
    Reserved {
        outputs,
        working_directory,
    }
}

/// Catches each direction of the path rule: a node holding an output, the working
/// directory or its own path as one is out; a node under an output directory is out; a
/// node above the working directory's *parent* is only out if it holds it; names that
/// merely share a prefix with a reserved path are not related.
#[test]
fn the_reserved_path_table() {
    let outputs = vec!["a/b/o".to_owned()];
    let r = reserved(&outputs, "c/d");
    for (path, conflicts) in [
        ("a", true),
        ("a/b", true),
        ("a/b/o", true),
        ("a/b/o/deeper", true),
        ("a/c", false),
        ("ab", false),
        ("a/bb", false),
        ("c", true),
        ("c/d", true),
        // Below the working directory: the action runs above it, in a directory that
        // is not the tree's.
        ("c/d/e", false),
        ("c/e", false),
        ("cd", false),
    ] {
        assert_eq!(r.conflicts(path), conflicts, "{path}");
    }
    // The working directory with a trailing slash is the same directory.
    assert!(reserved(&[], "c/d/").conflicts("c/d"));
    // An output that is the input root itself reserves everything.
    assert!(reserved(&[String::new()], "").conflicts("any"));
}

fn summary(files: u64, bytes: u64) -> Summary {
    Summary { files, bytes }
}

/// Catches the boundaries of the size rule (files `>=`, bytes `<=`) and the characters
/// a volume destination cannot hold.
#[test]
fn the_qualification_boundaries() {
    let none = reserved(&[], "");
    assert!(qualifies("t", summary(256, 0), &none, LIMITS));
    assert!(!qualifies("t", summary(255, 0), &none, LIMITS));
    assert!(qualifies("t", summary(256, 1 << 30), &none, LIMITS));
    assert!(!qualifies("t", summary(256, (1 << 30) + 1), &none, LIMITS));
    for bad in ["a:b", "a,b", "x/a:b/y"] {
        assert!(!qualifies(bad, summary(999, 0), &none, LIMITS), "{bad}");
    }
    let outputs = vec!["t/o".to_owned()];
    assert!(!qualifies(
        "t",
        summary(999, 0),
        &reserved(&outputs, ""),
        LIMITS
    ));
}

/// A directory of `files` files (each `size` bytes) and the given children; the digest
/// is its real one, so a tree of these is a valid input root.
fn directory(files: usize, size: i64, children: &[(&str, &Digest)]) -> (Digest, Directory) {
    let mut directory = Directory::default();
    for i in 0..files {
        let mut hash = digest_of(format!("{files}-{i}-{size}").as_bytes());
        hash.size_bytes = size;
        directory.files.push(FileNode {
            name: format!("f{i}"),
            digest: Some(hash),
            ..FileNode::default()
        });
    }
    for (name, digest) in children {
        directory.directories.push(DirectoryNode {
            name: (*name).to_owned(),
            digest: Some((*digest).clone()),
        });
    }
    (digest_of(&directory.encode_to_vec()), directory)
}

fn loaded(directories: &[&(Digest, Directory)]) -> Loaded {
    directories
        .iter()
        .map(|(digest, directory)| (label(digest), directory.clone()))
        .collect()
}

/// Catches counts that miss a level, a shared child counted once instead of at each
/// place (a count of *places* is what a mount saves), bytes that drop the negative-size
/// guard, and a child with no summary treated as empty.
#[test]
fn counts_are_per_place_and_use_the_known_summaries_for_skipped_children() {
    let leaf = directory(3, 10, &[]);
    let mid = directory(2, 5, &[("x", &leaf.0), ("y", &leaf.0)]);
    let top = directory(1, 1, &[("m", &mid.0)]);
    let all = loaded(&[&leaf, &mid, &top]);
    let sums = summarize(&all, &|_| None).unwrap();
    assert_eq!(sums[&label(&leaf.0)], summary(3, 30));
    assert_eq!(sums[&label(&mid.0)], summary(2 + 6, 10 + 60));
    assert_eq!(sums[&label(&top.0)], summary(1 + 8, 1 + 70));

    // `leaf` is not loaded: its summary is the known one.
    let partial = loaded(&[&mid, &top]);
    let known = |name: &str| (name == label(&leaf.0)).then(|| summary(300, 3000));
    let sums = summarize(&partial, &known).unwrap();
    assert_eq!(sums[&label(&top.0)], summary(1 + 2 + 600, 1 + 10 + 6000));
    // The skipped child is in the map too: `select` looks it up there.
    assert_eq!(sums[&label(&leaf.0)], summary(300, 3000));

    // A skipped child with no summary is an error, not an empty directory.
    let err = summarize(&partial, &|_| None).unwrap_err();
    assert!(matches!(err, TreeError::Invalid(_)), "{err}");
}

/// Catches a descent past the highest node (both a node and its child chosen), the
/// order (largest first, equal sizes by path), the cap, and a node that is not loaded
/// being skipped silently.
#[test]
fn the_highest_nodes_are_chosen_largest_first_up_to_the_cap() {
    let big = directory(300, 1, &[]);
    let inner = directory(0, 0, &[("deep", &big.0)]);
    let bigger = directory(400, 1, &[]);
    let same = directory(300, 2, &[]);
    let root = directory(
        1,
        1,
        &[
            ("z", &inner.0),
            ("m", &bigger.0),
            ("b", &same.0),
            ("a", &big.0),
        ],
    );
    let all = loaded(&[&root, &inner, &big, &bigger, &same]);
    let sums = summarize(&all, &|_| None).unwrap();
    let none = reserved(&[], "");
    let paths =
        |chosen: &[Candidate]| -> Vec<String> { chosen.iter().map(|c| c.path.clone()).collect() };
    let chosen = select(&all, &sums, &root.0, &none, LIMITS, 100).unwrap();
    // `z` holds 300 files (through `deep`) and is the highest node on its path: `z`,
    // not `z/deep`. 300-file nodes by path: a, b, z; the 400-file one first.
    assert_eq!(paths(&chosen), ["m", "a", "b", "z"]);
    let capped = Limits {
        max_mounts: 2,
        ..LIMITS
    };
    let chosen = select(&all, &sums, &root.0, &none, capped, 100).unwrap();
    assert_eq!(paths(&chosen), ["m", "a"]);
    // An output under `z` takes `z` out and `z/deep` too.
    let outputs = vec!["z/deep/o".to_owned()];
    let chosen = select(&all, &sums, &root.0, &reserved(&outputs, ""), LIMITS, 100).unwrap();
    assert_eq!(paths(&chosen), ["m", "a", "b"]);

    // A node to walk into that was not loaded is an error.
    let hole = loaded(&[&root]);
    let err = select(&hole, &sums, &root.0, &reserved(&outputs, ""), LIMITS, 100).unwrap_err();
    assert!(matches!(err, TreeError::Invalid(_)), "{err}");
}

/// Catches the walk's cap not applying: a diamond (each level names the next twice)
/// has 2^n places at depth n.
#[tokio::test]
async fn a_tree_with_too_many_places_is_refused() {
    let mut blobs = HashMap::new();
    let mut below = directory(1, 1, &[]);
    blobs.insert(label(&below.0), below.1.encode_to_vec());
    for _ in 0..12 {
        below = directory(0, 0, &[("l", &below.0), ("r", &below.0)]);
        blobs.insert(label(&below.0), below.1.encode_to_vec());
    }
    let cas = Blobs(blobs);
    let ok = load(&cas, &below.0, &Loaded::new(), &|_, _| false, 1 << 14).await;
    assert_eq!(ok.unwrap().len(), 13, "13 distinct directories");
    let err = load(&cas, &below.0, &Loaded::new(), &|_, _| false, 1000)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, TreeError::Invalid(m) if m.contains("more than 1000")),
        "{err}"
    );
    // A skipped child is not read.
    let skipped = load(
        &cas,
        &below.0,
        &Loaded::new(),
        &|path, _| path == "l" || path == "r",
        1000,
    )
    .await
    .unwrap();
    assert_eq!(skipped.len(), 1);
}

/// Catches a level that reads a digest twice when it appears at two depths (`x` is the
/// root's child and `y`'s), a child with no digest taken for an empty one, and a skipped
/// node that is walked anyway.
#[tokio::test]
async fn a_digest_at_two_depths_is_read_once_and_a_child_needs_its_digest() {
    let x = directory(2, 1, &[]);
    let y = directory(0, 0, &[("x", &x.0)]);
    let root = directory(0, 0, &[("x", &x.0), ("y", &y.0)]);
    let mut blobs = HashMap::new();
    for d in [&x, &y, &root] {
        blobs.insert(label(&d.0), d.1.encode_to_vec());
    }
    let cas = Blobs(blobs);
    let loaded = load(&cas, &root.0, &Loaded::new(), &|_, _| false, 100)
        .await
        .unwrap();
    assert_eq!(loaded.len(), 3);

    let mut headless = directory(0, 0, &[("x", &x.0)]);
    headless.1.directories[0].digest = None;
    headless.0 = digest_of(&headless.1.encode_to_vec());
    let mut blobs = HashMap::new();
    blobs.insert(label(&headless.0), headless.1.encode_to_vec());
    let err = load(
        &Blobs(blobs),
        &headless.0,
        &Loaded::new(),
        &|_, _| false,
        100,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, TreeError::Invalid(m) if m.contains("no digest")),
        "{err}"
    );
}

/// Catches the walk of `select` ignoring its cap, as `load` has one.
#[test]
fn select_refuses_a_tree_with_too_many_places() {
    let leaf = directory(1, 1, &[]);
    let mut below = leaf.clone();
    let mut all = loaded(&[&leaf]);
    for _ in 0..6 {
        below = directory(0, 0, &[("l", &below.0), ("r", &below.0)]);
        all.insert(label(&below.0), below.1.clone());
    }
    let sums = summarize(&all, &|_| None).unwrap();
    let none = reserved(&[], "");
    let wide = Limits {
        min_files: 1 << 40,
        ..LIMITS
    };
    assert!(
        select(&all, &sums, &below.0, &none, wide, 1000)
            .unwrap()
            .is_empty()
    );
    let err = select(&all, &sums, &below.0, &none, wide, 20).unwrap_err();
    assert!(
        matches!(&err, TreeError::Invalid(m) if m.contains("more than 20")),
        "{err}"
    );
}

/// Catches a store name that could leave the store or collide: only lower-case SHA-256
/// hex with a size is a name; a node whose digest has none is walked into, not mounted.
#[test]
fn only_a_sha256_digest_has_a_tree_name() {
    let good = digest_of(b"x");
    assert_eq!(tree_name(&good), Some(format!("{}-1", good.hash)));
    let with = |hash: &str, size| Digest {
        hash: hash.to_owned(),
        size_bytes: size,
    };
    for bad in [
        with("../../etc/passwd", 1),
        with(&good.hash.to_uppercase(), 1),
        with(&good.hash[1..], 1),
        with(&format!("{}g", &good.hash[1..]), 1),
        with(&good.hash, -1),
    ] {
        assert_eq!(tree_name(&bad), None, "{bad:?}");
    }
    // A big node under a digest with no name is not a candidate.
    let mut big = directory(300, 1, &[]);
    let odd = with("zz", 5);
    let root = directory(0, 0, &[("t", &big.0)]);
    let mut root = root;
    root.1.directories[0].digest = Some(odd.clone());
    big.0 = odd.clone();
    let all = loaded(&[&root, &big]);
    let mut sums = HashMap::new();
    sums.insert(label(&odd), summary(300, 300));
    let chosen = select(&all, &sums, &root.0, &reserved(&[], ""), LIMITS, 100).unwrap();
    assert!(chosen.is_empty(), "{chosen:?}");
}

/// Catches summaries growing without a bound: past the cap the map is emptied, then
/// holds what was just found.
#[test]
fn summaries_are_emptied_past_their_cap() {
    let kept = Summaries::with_cap(2);
    let one: HashMap<_, _> = [("a".to_owned(), summary(1, 1))].into();
    kept.extend(&one);
    kept.extend(&[("b".to_owned(), summary(2, 2))].into());
    assert_eq!(
        (kept.get("a"), kept.get("b")),
        (Some(summary(1, 1)), Some(summary(2, 2)))
    );
    kept.extend(&[("c".to_owned(), summary(3, 3))].into());
    assert_eq!(
        (kept.get("a"), kept.get("b"), kept.get("c")),
        (None, None, Some(summary(3, 3)))
    );
    assert_eq!(Summaries::default().get("c"), None);
}
