//! The neutral file index: full paths, offsets from the object, nothing the object lacks.

use kbf_proto::reapi::{DirectoryNode, FileNode, SymlinkNode};
use kbf_segments::sha256;

use super::*;

fn file(name: &str, bytes: &[u8], exec: bool) -> FileNode {
    FileNode {
        name: name.to_owned(),
        digest: Some(digest::to_reapi(&sha256(bytes))),
        is_executable: exec,
        ..FileNode::default()
    }
}

/// A tree: root holds `b` (executable), the symlink `a`, an empty file `e` and `sub`, which
/// holds the file `z` and the empty directory `none`.
fn tree() -> Tree {
    let none = Directory::default();
    let sub = Directory {
        files: vec![file("z", b"zed", false)],
        directories: vec![DirectoryNode {
            name: "none".to_owned(),
            digest: Some(digest::to_reapi(&sha256(&none.encode_to_vec()))),
        }],
        ..Directory::default()
    };
    let root = Directory {
        files: vec![file("b", b"bee", true), file("e", b"", false)],
        directories: vec![DirectoryNode {
            name: "sub".to_owned(),
            digest: Some(digest::to_reapi(&sha256(&sub.encode_to_vec()))),
        }],
        symlinks: vec![SymlinkNode {
            name: "a".to_owned(),
            target: "b".to_owned(),
            ..SymlinkNode::default()
        }],
        ..Directory::default()
    };
    Tree {
        root: Some(root),
        children: vec![sub, none],
    }
}

fn offsets(digest: &KbfDigest) -> Option<u64> {
    [
        (sha256(b"bee"), 10),
        (sha256(b"zed"), 20),
        (sha256(b"top"), 30),
    ]
    .into_iter()
    .find(|(d, _)| d == digest)
    .map(|(_, at)| at)
}

fn manifest() -> TaskManifest {
    TaskManifest {
        lease: "lease-1".to_owned(),
        action: sha256(b"action"),
        exit_code: 0,
        stdout: None,
        stderr: None,
        output_files: vec![kbf_task_pack::OutputFile {
            path: "out/top".to_owned(),
            digest: sha256(b"top"),
            is_executable: false,
        }],
        output_directories: Vec::new(),
        output_symlinks: vec![kbf_task_pack::OutputSymlink {
            path: "out/link".to_owned(),
            target: "top".to_owned(),
        }],
        timing: kbf_task_pack::Timing::default(),
        usage: kbf_task_pack::Usage::default(),
        packs: 1,
        large: Vec::new(),
        inputs: None,
        objects: None,
    }
}

#[test]
fn every_file_symlink_and_directory_is_listed_by_its_full_path_with_its_offset() {
    let json = build(&manifest(), &[("out/d".to_owned(), tree())], &offsets).expect("built");
    let (bee, zed, top, empty) = (
        sha256(b"bee").hash_hex(),
        sha256(b"zed").hash_hex(),
        sha256(b"top").hash_hex(),
        sha256(b"").hash_hex(),
    );
    let want = format!(
        concat!(
            r#"{{"v":1,"files":["#,
            r#"{{"path":"out/d/b","size":3,"exec":true,"sha256":"{bee}","offset":10}},"#,
            r#"{{"path":"out/d/e","size":0,"exec":false,"sha256":"{empty}"}},"#,
            r#"{{"path":"out/d/sub/z","size":3,"exec":false,"sha256":"{zed}","offset":20}},"#,
            r#"{{"path":"out/top","size":3,"exec":false,"sha256":"{top}","offset":30}}],"#,
            r#""symlinks":[{{"path":"out/d/a","target":"b"}},"#,
            r#"{{"path":"out/link","target":"top"}}],"#,
            r#""dirs":["out/d","out/d/sub","out/d/sub/none"]}}"#,
        ),
        bee = bee,
        zed = zed,
        top = top,
        empty = empty
    );
    assert_eq!(String::from_utf8(json).expect("utf-8"), want);
}

#[test]
fn no_outputs_is_an_empty_index() {
    let mut none = manifest();
    none.output_files.clear();
    none.output_symlinks.clear();
    let json = build(&none, &[], &offsets).expect("built");
    assert_eq!(json, br#"{"v":1,"files":[],"symlinks":[],"dirs":[]}"#);
}

#[test]
fn a_tree_with_no_root_is_an_empty_directory() {
    let mut none = manifest();
    none.output_files.clear();
    none.output_symlinks.clear();
    let json = build(&none, &[("d".to_owned(), Tree::default())], &offsets).expect("built");
    assert_eq!(
        json,
        br#"{"v":1,"files":[],"symlinks":[],"dirs":["d"]}"#.to_vec()
    );
}

#[test]
fn what_the_object_cannot_back_is_refused_for_what_is_missing() {
    let nowhere = |_: &KbfDigest| None;
    let why = build(&manifest(), &[], &nowhere).expect_err("no file");
    assert!(matches!(why, FilesError::NoFile(_)), "{why}");

    let mut none = manifest();
    none.output_files.clear();
    let mut no_child = tree();
    no_child.children.clear();
    let why = build(&none, &[("d".to_owned(), no_child)], &offsets).expect_err("no directory");
    assert!(matches!(why, FilesError::NoDirectory(_)), "{why}");

    let mut absent = tree();
    absent.root.as_mut().expect("root").files[0].digest = None;
    let why = build(&none, &[("d".to_owned(), absent)], &offsets).expect_err("no digest");
    assert!(matches!(why, FilesError::Digest(_)), "{why}");

    let mut bad = tree();
    bad.root.as_mut().expect("root").files[0].digest = Some(Digest {
        hash: "NOT-HEX".to_owned(),
        size_bytes: 1,
    });
    let why = build(&none, &[("d".to_owned(), bad)], &offsets).expect_err("bad digest");
    assert!(matches!(why, FilesError::Digest(_)), "{why}");
}
