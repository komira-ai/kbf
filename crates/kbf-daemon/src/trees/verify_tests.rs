//! Unit tests of the check a built tree passes before it is named.

use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::PathBuf;

use kbf_proto::reapi::{Digest, Directory, DirectoryNode, FileNode, SymlinkNode};
use prost::Message as _;

use super::build::{bytes_under, publish, remove_tree, verify};
use crate::cas::{digest_of, label};
use crate::tree::Loaded;

fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/kbf-daemon-unit")
        .join(format!("trees-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A root with one file, one executable, one symlink and one subdirectory with a file,
/// and the matching tree written at `dir` the way the builder leaves it.
fn tree(dir: &std::path::Path) -> (Loaded, Digest) {
    let sub = Directory {
        files: vec![FileNode {
            name: "g".to_owned(),
            digest: Some(digest_of(b"gg")),
            ..FileNode::default()
        }],
        ..Directory::default()
    };
    let sub_digest = digest_of(&sub.encode_to_vec());
    let root = Directory {
        files: vec![
            FileNode {
                name: "f".to_owned(),
                digest: Some(digest_of(b"ffff")),
                ..FileNode::default()
            },
            FileNode {
                name: "x".to_owned(),
                digest: Some(digest_of(b"x")),
                is_executable: true,
                ..FileNode::default()
            },
        ],
        directories: vec![DirectoryNode {
            name: "sub".to_owned(),
            digest: Some(sub_digest.clone()),
        }],
        symlinks: vec![SymlinkNode {
            name: "l".to_owned(),
            target: "f".to_owned(),
            ..SymlinkNode::default()
        }],
        ..Directory::default()
    };
    let root_digest = digest_of(&root.encode_to_vec());
    std::fs::create_dir(dir.join("sub")).unwrap();
    for (path, bytes, mode) in [
        ("f", &b"ffff"[..], 0o444),
        ("x", b"x", 0o555),
        ("sub/g", b"gg", 0o444),
    ] {
        std::fs::write(dir.join(path), bytes).unwrap();
        std::fs::set_permissions(dir.join(path), std::fs::Permissions::from_mode(mode)).unwrap();
    }
    symlink("f", dir.join("l")).unwrap();
    let loaded: Loaded = [(label(&sub_digest), sub), (label(&root_digest), root)]
        .into_iter()
        .collect();
    (loaded, root_digest)
}

/// Catches a check that accepts a tree that is not its digest's: the right tree passes
/// and is counted (4 files and symlinks, 7 bytes) with its directories made 0755; then
/// one defect each — an extra entry, a missing one, a wrong size, a lost executable bit,
/// a group-writable file, a wrong symlink target, a file where a directory belongs.
#[test]
fn a_tree_that_is_not_its_directory_is_refused() {
    let dir = scratch("verify");
    let (loaded, root) = tree(&dir);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let found = verify(&loaded, &root, &dir).unwrap();
    assert_eq!((found.files, found.bytes), (4, 7));
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::metadata(dir.join("sub"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );

    type Defect = (&'static str, fn(&std::path::Path));
    let defects: [Defect; 9] = [
        ("a directory where a file belongs", |d| {
            std::fs::remove_file(d.join("f")).unwrap();
            std::fs::create_dir(d.join("f")).unwrap();
        }),
        ("a file where a symlink belongs", |d| {
            std::fs::remove_file(d.join("l")).unwrap();
            std::fs::write(d.join("l"), b"f").unwrap();
        }),
        ("an extra entry", |d| {
            std::fs::write(d.join("extra"), b"").unwrap()
        }),
        ("a missing entry", |d| {
            std::fs::remove_file(d.join("sub/g")).unwrap()
        }),
        ("a wrong size", |d| {
            std::fs::set_permissions(d.join("f"), std::fs::Permissions::from_mode(0o644)).unwrap();
            std::fs::write(d.join("f"), b"fff").unwrap();
        }),
        ("a lost executable bit", |d| {
            std::fs::set_permissions(d.join("x"), std::fs::Permissions::from_mode(0o444)).unwrap();
        }),
        ("a group-writable file", |d| {
            std::fs::set_permissions(d.join("f"), std::fs::Permissions::from_mode(0o464)).unwrap();
        }),
        ("a wrong symlink target", |d| {
            std::fs::remove_file(d.join("l")).unwrap();
            symlink("x", d.join("l")).unwrap();
        }),
        ("a file where a directory belongs", |d| {
            std::fs::remove_dir_all(d.join("sub")).unwrap();
            std::fs::write(d.join("sub"), b"gg").unwrap();
        }),
    ];
    for (name, defect) in defects {
        let dir = scratch("verify-defect");
        let (loaded, root) = tree(&dir);
        defect(&dir);
        let err = verify(&loaded, &root, &dir).expect_err(name);
        assert!(
            matches!(
                err,
                crate::tree::TreeError::Invalid(_) | crate::tree::TreeError::Io { .. }
            ),
            "{name}: {err}"
        );
    }
}

/// Catches a check that trusts a Directory it was not given: a tree whose Directory
/// messages are not loaded is refused, not walked.
#[test]
fn a_directory_that_is_not_loaded_is_refused() {
    let dir = scratch("verify-unloaded");
    let (_, root) = tree(&dir);
    let err = verify(&Loaded::new(), &root, &dir).expect_err("nothing loaded");
    assert!(matches!(err, crate::tree::TreeError::Invalid(_)), "{err}");
}

/// Catches `publish` replacing a tree already named (the "plain rename" mutant would
/// swap a tree a lease has mounted), reporting a refusal as a success, or hiding another
/// error: the first rename names the tree, a second onto a non-empty directory is
/// reported as "not named by this call", a missing source is an error.
#[test]
fn publish_names_a_tree_once() {
    let dir = scratch("publish");
    for name in ["a", "b"] {
        std::fs::create_dir(dir.join(name)).unwrap();
        std::fs::write(dir.join(name).join("f"), name).unwrap();
    }
    assert!(publish(&dir.join("a"), &dir.join("final")).unwrap());
    assert!(!publish(&dir.join("b"), &dir.join("final")).unwrap());
    assert_eq!(std::fs::read(dir.join("final/f")).unwrap(), b"a");
    assert!(b_is_left(&dir));
    assert!(publish(&dir.join("gone"), &dir.join("other")).is_err());
}

fn b_is_left(dir: &std::path::Path) -> bool {
    dir.join("b/f").exists()
}

/// Catches a removal that follows a symlink out of the tree, leaves a directory behind,
/// or fails on what is already gone; and a size count that adds symlinks or special
/// files: a tree with a nested directory, a symlink to a directory outside it and a
/// FIFO is removed whole with the outside untouched; `bytes_under` counts regular files
/// only.
#[test]
fn removal_and_sizes_look_at_regular_files_only() {
    let dir = scratch("remove");
    let outside = dir.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep"), b"keep").unwrap();
    let tree = dir.join("tree");
    std::fs::create_dir_all(tree.join("a/b")).unwrap();
    std::fs::write(tree.join("a/b/f"), b"12345").unwrap();
    std::fs::write(tree.join("g"), b"123").unwrap();
    symlink(&outside, tree.join("a/link")).unwrap();
    rustix::fs::mknodat(
        rustix::fs::CWD,
        tree.join("fifo"),
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
    assert_eq!(bytes_under(&tree).unwrap(), 8);
    remove_tree(&tree).unwrap();
    assert!(!tree.exists());
    assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"keep");
    remove_tree(&tree).expect("what is gone is removed");
}
