//! The input root's Merkle tree: what each Directory message holds, in which order, and the
//! paths that make no tree.

use super::*;
use prost::Message as _;

fn file(bytes: &[u8], executable: bool) -> Entry {
    Entry::File {
        bytes: bytes.to_vec(),
        executable,
    }
}

fn dir(tree: &Tree, digest: &Digest) -> Directory {
    Directory::decode(tree.blobs[&digest.hash].as_slice()).expect("a Directory")
}

/// Catches a child listed out of name order (REAPI requires sorted names), a file whose
/// executable bit is lost, a symlink followed or dropped, and a nested directory flattened.
#[test]
fn each_directory_lists_its_children_sorted_with_their_kinds() {
    let tree = build(vec![
        ("b.txt".to_owned(), file(b"b", false)),
        ("a/run".to_owned(), file(b"#!", true)),
        ("a/link".to_owned(), Entry::Symlink("../b.txt".to_owned())),
        ("a/deep/x".to_owned(), file(b"x", false)),
        ("A".to_owned(), file(b"upper", false)),
    ])
    .expect("a tree");
    let root = dir(&tree, &tree.root);
    let names: Vec<&str> = root.files.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["A", "b.txt"]);
    assert_eq!(root.directories.len(), 1);
    assert_eq!(root.directories[0].name, "a");
    let a = dir(&tree, root.directories[0].digest.as_ref().expect("digest"));
    assert_eq!(a.files.len(), 1);
    assert_eq!(a.files[0].name, "run");
    assert!(a.files[0].is_executable);
    assert!(!root.files[0].is_executable);
    assert_eq!(a.symlinks.len(), 1);
    assert_eq!(a.symlinks[0].name, "link");
    assert_eq!(a.symlinks[0].target, "../b.txt");
    assert_eq!(a.directories[0].name, "deep");
    let digest = a.files[0].digest.as_ref().expect("digest");
    assert_eq!(tree.blobs[&digest.hash], b"#!");
    assert_eq!(digest.size_bytes, 2);
}

/// Catches a digest that is not the SHA-256 of the bytes, or a size that is not their length:
/// the empty blob is the well-known one.
#[test]
fn the_digest_is_the_sha256_and_length_of_the_bytes() {
    let empty = digest_of(b"");
    assert_eq!(
        empty.hash,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(empty.size_bytes, 0);
    assert_eq!(digest_of(b"abc").size_bytes, 3);
}

/// Catches two files with the same bytes stored twice, and the root message missing from the
/// blobs: the tree of two equal files has three blobs, the content once and two directories.
#[test]
fn equal_contents_are_one_blob() {
    let tree = build(vec![
        ("x/a".to_owned(), file(b"same", false)),
        ("b".to_owned(), file(b"same", false)),
    ])
    .expect("a tree");
    assert_eq!(tree.blobs.len(), 3);
    assert!(tree.blobs.contains_key(&tree.root.hash));
}

/// Catches a path that climbs out of the root, an absolute one, and an empty or `.` component
/// taken as a name.
#[test]
fn a_path_that_is_not_plain_makes_no_tree() {
    for path in ["", "/etc/x", "a//b", "a/./b", "../x", "a/", "."] {
        let made = build(vec![(path.to_owned(), file(b"", false))]);
        assert_eq!(
            made.err(),
            Some(TreeError::BadPath(path.to_owned())),
            "{path}"
        );
    }
}

/// Catches a later entry silently replacing an earlier one at the same path.
#[test]
fn a_path_given_twice_makes_no_tree() {
    let made = build(vec![
        ("a/b".to_owned(), file(b"1", false)),
        ("a/b".to_owned(), Entry::Symlink("x".to_owned())),
    ]);
    assert_eq!(made.err(), Some(TreeError::Twice("a/b".to_owned())));
}

/// Catches a file and a directory of one name both kept, in either order of the entries.
#[test]
fn a_name_that_is_both_a_file_and_a_directory_makes_no_tree() {
    let file_first = build(vec![
        ("a".to_owned(), file(b"1", false)),
        ("a/b".to_owned(), file(b"2", false)),
    ]);
    assert_eq!(
        file_first.err(),
        Some(TreeError::FileAndDirectory("a/b".to_owned()))
    );
    let dir_first = build(vec![
        ("a/b".to_owned(), file(b"2", false)),
        ("a".to_owned(), file(b"1", false)),
    ]);
    assert_eq!(
        dir_first.err(),
        Some(TreeError::FileAndDirectory("a".to_owned()))
    );
}

/// Catches an empty root made of nothing: no entries is the empty Directory, one blob.
#[test]
fn no_entries_is_the_empty_directory() {
    let tree = build(Vec::new()).expect("a tree");
    assert_eq!(tree.blobs.len(), 1);
    assert_eq!(dir(&tree, &tree.root), Directory::default());
}
