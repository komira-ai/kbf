//! The trees a run sends: generated to a shape from a seed, or read from a directory;
//! the sizes `--total-bytes` and `--batch-bytes` take; the latency percentiles.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use kbf_bench::cli::parse_size;
use kbf_bench::report::Latency;
use kbf_bench::tree::{self, HEADER_BYTES, Shape, Tree, TreeError, digest_of};
use kbf_proto::reapi::{Directory, Tree as TreeMessage};
use prost::Message as _;

const PILOT: Shape = Shape {
    files: 15_000,
    empty: 144,
    dirs: 989,
    total_bytes: 307_000_000,
};

/// The `Tree` message among the blobs, checked: the blobs that are no file are it
/// and every `Directory` it holds (the root's too), no more and no fewer, as the
/// daemon stores an output directory.
fn message(tree: &Tree, files: &BTreeSet<String>) -> TreeMessage {
    let blob = tree
        .blobs
        .iter()
        .find(|b| b.digest == tree.tree_digest)
        .expect("the Tree blob");
    assert_eq!(digest_of(&blob.bytes), tree.tree_digest);
    let message = TreeMessage::decode(blob.bytes.as_slice()).expect("a Tree message");
    let mut wanted: BTreeSet<String> = message
        .root
        .iter()
        .chain(&message.children)
        .map(|d| digest_of(&d.encode_to_vec()).hash)
        .collect();
    wanted.insert(tree.tree_digest.hash.clone());
    let others: BTreeSet<String> = tree
        .blobs
        .iter()
        .map(|b| b.digest.hash.clone())
        .filter(|h| !files.contains(h))
        .collect();
    assert_eq!(others, wanted, "the blobs that are no file");
    message
}

/// Every file digest the tree's directories name, the directories below the root,
/// and the indices of the empty files (file `i` is `f<i>`).
fn file_digests(message: &TreeMessage) -> (Vec<String>, u64, Vec<u64>) {
    let all: Vec<&Directory> = message.root.iter().chain(&message.children).collect();
    let held: BTreeSet<String> = message
        .children
        .iter()
        .map(|c| digest_of(&c.encode_to_vec()).hash)
        .collect();
    let mut digests = Vec::new();
    let mut empty = Vec::new();
    for d in &all {
        let names: Vec<&str> = d.files.iter().map(|f| f.name.as_str()).collect();
        assert!(names.is_sorted(), "REAPI wants names sorted: {names:?}");
        let dirs: Vec<&str> = d.directories.iter().map(|f| f.name.as_str()).collect();
        assert!(dirs.is_sorted(), "{dirs:?}");
        for child in &d.directories {
            let digest = child.digest.as_ref().expect("a child digest");
            assert!(
                held.contains(&digest.hash),
                "a child directory the Tree does not hold"
            );
        }
        for f in &d.files {
            let digest = f.digest.as_ref().expect("digest");
            if digest.size_bytes == 0 {
                empty.push(f.name[1..].parse().expect("f<i>"));
            }
            digests.push(digest.hash.clone());
        }
    }
    empty.sort_unstable();
    (digests, all.len() as u64 - 1, empty)
}

/// Catches a generated tree off its shape (files, empty files, directories, bytes),
/// a non-empty file without its seed and index (two runs would share blobs), sizes
/// all alike, and a Tree message that does not name every file and directory.
#[test]
fn a_generated_tree_has_the_pilot_shape() {
    // The pilot's files and directories, a tenth of its bytes (hashing 307 MB takes
    // a debug build long).
    let shape = Shape {
        total_bytes: 30_700_000,
        ..PILOT
    };
    let tree = tree::generate(&shape, 42).expect("tree");
    assert_eq!(
        (
            tree.seed,
            tree.files,
            tree.empty,
            tree.dirs,
            tree.file_bytes,
            tree.skipped
        ),
        (42, 15_000, 144, 989, 30_700_000, 0)
    );
    let files = files_of(&tree);
    assert_eq!(
        files.len(),
        15_000 - 144 + 1,
        "every non-empty file distinct"
    );
    // The Tree and 990 Directories (all distinct: each holds files of its own).
    assert_eq!(tree.blobs.len(), files.len() + 1 + 990);
    let indices: BTreeSet<u64> = tree
        .blobs
        .iter()
        .filter(|b| files.contains(&b.digest.hash) && !b.bytes.is_empty())
        .map(|b| u64::from_le_bytes(b.bytes[8..16].try_into().expect("8 bytes")))
        .collect();
    assert_eq!(indices.len(), 15_000 - 144);
    assert!(indices.iter().all(|&i| i < 15_000));
    let sizes: Vec<usize> = tree.blobs.iter().map(|b| b.bytes.len()).collect();
    let (small, large) = (
        sizes.iter().filter(|&&n| n < 4_096).count(),
        sizes.iter().max(),
    );
    assert!(
        small > 1_000 && *large.expect("blobs") > 20_000,
        "sizes spread: {small}, {large:?}"
    );

    let message = message(&tree, &files);
    let (named, dirs, empty) = file_digests(&message);
    assert_eq!((named.len(), dirs, empty.len()), (15_000, 989, 144));
    // Spread evenly: one in every 15 000 / 144 (104.2) files.
    let gaps: Vec<u64> = empty.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(
        empty[0] < 105 && gaps.iter().all(|&g| g == 104 || g == 105),
        "{empty:?}"
    );
    assert_eq!(named.into_iter().collect::<BTreeSet<_>>(), files);
}

/// The file blobs of `tree`: empty, or starting with its seed.
fn files_of(tree: &Tree) -> BTreeSet<String> {
    tree.blobs
        .iter()
        .filter(|b| b.bytes.is_empty() || b.bytes[..8] == tree.seed.to_le_bytes())
        .map(|b| b.digest.hash.clone())
        .collect()
}

/// Catches a seed that does not fix the bytes (`read` could not check an upload), and
/// two seeds that share blobs (a second run would find them present).
#[test]
fn the_seed_fixes_the_tree_and_another_shares_only_the_empty_blob() {
    let shape = Shape {
        files: 1_500,
        empty: 14,
        dirs: 99,
        total_bytes: 30_000_000,
    };
    let tree = tree::generate(&shape, 42).expect("tree");
    assert_eq!(
        tree::generate(&shape, 42).expect("tree"),
        tree,
        "same seed, same tree"
    );
    let files = files_of(&tree);
    assert_eq!(files.len(), 1_500 - 14 + 1);
    let other = tree::generate(&shape, 43).expect("tree");
    let shared = other
        .blobs
        .iter()
        .filter(|b| files.contains(&b.digest.hash))
        .count();
    assert_eq!(shared, 1, "another seed shares the empty blob alone");
}

/// Catches a shape that cannot be made accepted (or a tight one refused): more empty
/// files than files, too few bytes for the headers, bytes with no file to hold them.
#[test]
fn a_shape_that_cannot_be_made_is_refused() {
    let shape = |files, empty, total_bytes| Shape {
        files,
        empty,
        dirs: 0,
        total_bytes,
    };
    for (files, empty, total) in [(10, 11, 1000), (10, 0, 159), (5, 5, 10)] {
        let refused = tree::generate(&shape(files, empty, total), 1);
        assert!(
            matches!(&refused, Err(TreeError::Shape(e)) if e.contains(&format!("{files} files, {empty} of them empty, holding {total} bytes"))),
            "{refused:?}"
        );
    }
    let tight = tree::generate(&shape(10, 0, 10 * HEADER_BYTES), 1).expect("tree");
    assert!(
        tight
            .blobs
            .iter()
            .all(|b| b.bytes.len() as u64 == HEADER_BYTES || b.bytes[0] != 1)
    );
    let empty = tree::generate(&shape(5, 5, 0), 1).expect("tree");
    assert_eq!(
        (empty.files, empty.empty, empty.file_bytes, empty.dirs),
        (5, 5, 0, 0)
    );
    assert_eq!(
        empty.blobs.len(),
        3,
        "the empty blob, the output directory and the Tree"
    );
    let files = files_of(&empty);
    let message = message(&empty, &files);
    assert_eq!(message.root.expect("root").files.len(), 5);
}

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("kbf-bench-tree")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch");
    dir
}

/// Catches a directory's files left out, or read without the seed salt (a second run
/// would find them all present), symlinks followed or uploaded, and a walk whose order
/// (so whose salt) changes between runs.
#[test]
fn a_directory_is_read_with_its_files_salted_by_the_seed() {
    let dir = scratch("read");
    std::fs::create_dir_all(dir.join("a/b")).expect("mkdir");
    std::fs::create_dir_all(dir.join("c")).expect("mkdir");
    std::fs::write(dir.join("top"), b"top file").expect("write");
    std::fs::write(dir.join("a/one"), b"one").expect("write");
    std::fs::write(dir.join("a/b/two"), b"two two").expect("write");
    std::fs::write(dir.join("c/empty"), b"").expect("write");
    std::os::unix::fs::symlink("top", dir.join("link")).expect("symlink");

    let tree = tree::read_dir(&dir, 9).expect("read");
    assert_eq!(
        (tree.seed, tree.files, tree.empty, tree.dirs, tree.skipped),
        (9, 4, 1, 3, 1)
    );
    assert_eq!(tree.file_bytes, 8 + 3 + 7 + 3 * HEADER_BYTES);
    let salted: Vec<&[u8]> = tree
        .blobs
        .iter()
        .filter_map(|b| b.bytes.strip_prefix(b"two two".as_slice()))
        .collect();
    assert_eq!(salted.len(), 1);
    assert_eq!(&salted[0][..8], &9u64.to_le_bytes());
    assert_eq!(tree::read_dir(&dir, 9).expect("read"), tree);
    let other = tree::read_dir(&dir, 10).expect("read");
    let ours: BTreeSet<&str> = tree.blobs.iter().map(|b| b.digest.hash.as_str()).collect();
    let shared = other
        .blobs
        .iter()
        .filter(|b| ours.contains(b.digest.hash.as_str()))
        .count();
    assert_eq!(shared, 2, "the empty file, and c, which holds it alone");
}

/// Catches a missing directory reported without its path.
#[test]
fn a_directory_that_cannot_be_read_names_its_path() {
    let missing = scratch("missing").join("not-there");
    let error = tree::read_dir(&missing, 1).expect_err("missing");
    assert!(matches!(error, TreeError::Io { .. }));
    assert!(
        error
            .to_string()
            .starts_with(&missing.display().to_string()),
        "{error}"
    );
}

/// Catches a unit read at the wrong scale (KB as KiB, say) and nonsense accepted.
#[test]
fn sizes_take_decimal_and_binary_units() {
    for (text, size) in [
        ("12", 12),
        ("5B", 5),
        ("3KB", 3_000),
        ("307MB", 307_000_000),
        ("2GB", 2_000_000_000),
        ("3KiB", 3 << 10),
        ("4MiB", 4 << 20),
        ("1GiB", 1 << 30),
    ] {
        assert_eq!(parse_size(text), Ok(size), "{text}");
    }
    for text in ["", "MB", "1.5MB", "-1", "1 MB", "1TB", "99999999999999GB"] {
        let refused = parse_size(text).expect_err(text);
        assert!(refused.contains("is not a size"), "{refused}");
    }
}

/// Catches percentiles that are not nearest-rank (an index off by one either way, or
/// unsorted times).
#[test]
fn latencies_are_nearest_rank() {
    let mut times: Vec<Duration> = (1..=200).map(Duration::from_millis).collect();
    times.reverse();
    let l = Latency::of(times);
    assert_eq!(
        (l.count, l.p50_ms, l.p90_ms, l.p99_ms, l.max_ms),
        (200, 100.0, 180.0, 198.0, 200.0)
    );
    let one = Latency::of(vec![Duration::from_millis(7)]);
    assert_eq!((one.p50_ms, one.p99_ms, one.max_ms), (7.0, 7.0, 7.0));
}
