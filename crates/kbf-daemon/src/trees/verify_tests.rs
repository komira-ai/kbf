//! Unit tests of the check a built tree passes before it is named.

use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::PathBuf;

use kbf_proto::reapi::{Digest, Directory, DirectoryNode, FileNode, SymlinkNode};
use prost::Message as _;

use super::build::verify;
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
    let defects: [Defect; 7] = [
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
