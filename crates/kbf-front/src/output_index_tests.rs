//! The neutral file index: what it refuses, and the REAPI trees and tierstore forms derived from it.

use std::os::unix::fs::PermissionsExt;

use kbf_proto::reapi::{ActionResult, Digest as WireDigest, Tree};
use kbf_testkit::dir::TestDir;
use prost::Message as _;

use super::*;

fn sha(bytes: &[u8]) -> Digest {
    kbf_segments::sha256(bytes)
}

fn file(path: &str, bytes: &[u8], exec: bool) -> String {
    let d = sha(bytes);
    let offset = if bytes.is_empty() {
        String::new()
    } else {
        ",\"offset\":0".to_owned()
    };
    format!(
        r#"{{"path":"{path}","size":{},"exec":{exec},"sha256":"{}"{offset}}}"#,
        d.size_bytes,
        d.hash_hex()
    )
}

fn index(files: &[String], links: &[&str], dirs: &[&str]) -> Vec<u8> {
    let links: Vec<String> = links
        .iter()
        .map(|l| {
            let (path, target) = l.split_once('=').expect("path=target");
            format!(r#"{{"path":"{path}","target":"{target}"}}"#)
        })
        .collect();
    let dirs: Vec<String> = dirs.iter().map(|d| format!("\"{d}\"")).collect();
    format!(
        r#"{{"v":1,"files":[{}],"symlinks":[{}],"dirs":[{}]}}"#,
        files.join(","),
        links.join(","),
        dirs.join(",")
    )
    .into_bytes()
}

/// The index of the tree the real collection below reads: top-level files, a nested directory
/// with an empty one, an empty file, a symlink and an executable.
fn sample() -> Vec<u8> {
    index(
        &[
            file("out/d/b", b"bee", true),
            file("out/d/e", b"", false),
            file("out/d/sub/z", b"zed", false),
            file("out/d/sub/deep/y", b"why", false),
            file("out/d/sub2/z", b"zed", false),
            file("out/top", b"top", false),
        ],
        &["out/d/a=b"],
        &[
            "out/d",
            "out/d/sub",
            "out/d/sub/deep",
            "out/d/sub/none",
            "out/d/sub2",
        ],
    )
}

#[test]
fn the_tree_derived_is_the_one_kbf_outputs_builds_from_the_directory() {
    let scratch = TestDir::under(std::env::temp_dir(), "kbf-front-output-index");
    let dir = scratch.to_path_buf();
    for (path, bytes) in [
        ("d/b", &b"bee"[..]),
        ("d/e", b""),
        ("d/sub/z", b"zed"),
        ("d/sub/deep/y", b"why"),
        ("d/sub2/z", b"zed"),
    ] {
        let at = dir.join(path);
        std::fs::create_dir_all(at.parent().expect("parent")).expect("mkdir");
        std::fs::write(at, bytes).expect("write");
    }
    std::fs::create_dir(dir.join("d/sub/none")).expect("mkdir");
    std::os::unix::fs::symlink("b", dir.join("d/a")).expect("symlink");
    std::fs::set_permissions(dir.join("d/b"), PermissionsExt::from_mode(0o755)).expect("chmod");

    struct Memory;
    impl kbf_outputs::Store for Memory {
        async fn put(&self, bytes: Vec<u8>) -> Result<WireDigest, kbf_outputs::StoreError> {
            Ok(kbf_outputs::digest_of(&bytes))
        }
        async fn put_file(
            &self,
            _: std::fs::File,
            d: WireDigest,
        ) -> Result<WireDigest, kbf_outputs::StoreError> {
            Ok(d)
        }
        fn in_flight(&self) -> usize {
            4
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let mut result = ActionResult::default();
    runtime
        .block_on(kbf_outputs::collect(
            &Memory,
            &dir,
            "",
            &["d".to_owned()],
            kbf_outputs::OutputLimits::DEFAULT,
            &mut result,
        ))
        .expect("collected");
    let real = result.output_directories[0]
        .tree_digest
        .clone()
        .expect("tree digest");

    // The same tree, as the index says it, under the output directory `d`.
    let derived_index = index(
        &[
            file("d/b", b"bee", true),
            file("d/e", b"", false),
            file("d/sub/z", b"zed", false),
            file("d/sub/deep/y", b"why", false),
            file("d/sub2/z", b"zed", false),
        ],
        &["d/a=b"],
        &["d", "d/sub", "d/sub/deep", "d/sub/none", "d/sub2"],
    );
    let derived = OutputIndex::parse(&derived_index)
        .expect("parsed")
        .tree("d")
        .expect("derived");
    assert_eq!(
        derived.digest_hex(),
        real.hash,
        "the Tree is byte for byte the real one"
    );
    // The root, `sub`, `sub/deep`, `sub/none` and `sub2`.
    assert_eq!(derived.directories.len(), 5);
}

#[test]
fn a_tree_is_derived_in_the_walks_order_with_kept_directories_for_the_manifests() {
    let parsed = OutputIndex::parse(&sample()).expect("parsed");
    let tree = parsed.tree("out/d").expect("derived");
    let root = &tree.directories[&tree.root];
    let names: Vec<&str> = root.files.iter().map(|f| f.0.as_str()).collect();
    assert_eq!(names, ["b", "e"]);
    assert_eq!(root.symlinks, [("a".to_owned(), "b".to_owned())]);
    let subs: Vec<&str> = root.directories.iter().map(|d| d.0.as_str()).collect();
    assert_eq!(subs, ["sub", "sub2"]);
    assert!(root.files[0].2);
    assert_eq!(
        parsed.digests().len(),
        4,
        "bee, zed, why, top; the empty file is no record"
    );
    assert_eq!(parsed.file("out/top").map(|f| f.exec), Some(false));
    assert_eq!(parsed.link("out/d/a"), Some("b"));
    assert!(parsed.file("out/nowhere").is_none() && parsed.link("out/nowhere").is_none());
}

#[test]
fn what_is_not_an_index_is_refused_for_its_own_reason() {
    let shape = |bytes: &[u8]| OutputIndex::parse(bytes).expect_err("refused");
    assert!(matches!(shape(b"{not json"), IndexError::Shape(_)));
    assert!(matches!(shape(b"{}"), IndexError::Shape(_)));
    for broken in [
        r#"{"files":[{"size":1,"exec":false,"sha256":"00"}],"symlinks":[],"dirs":[]}"#,
        r#"{"files":[{"path":"a","exec":false,"sha256":"00"}],"symlinks":[],"dirs":[]}"#,
        r#"{"files":[{"path":"a","size":1,"sha256":"00"}],"symlinks":[],"dirs":[]}"#,
        r#"{"files":[{"path":"a","size":1,"exec":false,"sha256":"00"}],"symlinks":[],"dirs":[]}"#,
        r#"{"files":[{"path":"a","size":1,"exec":false,"sha256":"gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg"}],"symlinks":[],"dirs":[]}"#,
        r#"{"files":[{"path":"a","size":1,"exec":false,"sha256":"0000000000000000000000000000000000000000000000000000000000000000ff"}],"symlinks":[],"dirs":[]}"#,
        r#"{"files":[],"symlinks":[{"path":"a"}],"dirs":[]}"#,
        r#"{"files":[],"symlinks":[{"target":"a"}],"dirs":[]}"#,
        r#"{"files":[],"symlinks":[],"dirs":[1]}"#,
        r#"{"files":[],"symlinks":[],"dirs":[]"#,
    ] {
        assert!(
            matches!(shape(broken.as_bytes()), IndexError::Shape(_)),
            "{broken}"
        );
    }
    for path in ["", "/abs", "a//b", "a/./b", "a/../b", "a/"] {
        let bytes = index(&[], &[], &[path]);
        assert_eq!(
            OutputIndex::parse(&bytes).expect_err("refused"),
            IndexError::Path(path.to_owned())
        );
    }
    let twice = index(&[file("a", b"x", false), file("a", b"y", false)], &[], &[]);
    assert_eq!(shape(&twice), IndexError::Twice("a".to_owned()));
    let two_kinds = index(&[file("a", b"x", false)], &[], &["a"]);
    assert_eq!(shape(&two_kinds), IndexError::Twice("a".to_owned()));
    let below_file = index(
        &[file("a", b"x", false), file("a/b", b"y", false)],
        &[],
        &[],
    );
    assert_eq!(
        shape(&below_file),
        IndexError::NotDirectory("a/b".to_owned())
    );
    let below_link = index(&[file("a/b", b"y", false)], &["a=z"], &[]);
    assert_eq!(
        shape(&below_link),
        IndexError::NotDirectory("a/b".to_owned())
    );
}

#[test]
fn an_output_directory_the_index_lacks_or_nests_too_deep_has_no_tree() {
    let parsed = OutputIndex::parse(&sample()).expect("parsed");
    for missing in ["out/none", "out/top", "nowhere", "out/d/b"] {
        assert_eq!(
            parsed.tree(missing).expect_err("no tree"),
            IndexError::NoDirectory(missing.to_owned())
        );
    }
    let mut dirs = Vec::new();
    let mut path = String::new();
    for level in 0..=MAX_DEPTH + 1 {
        path.push_str(&format!("{}d", if level == 0 { "" } else { "/" }));
        dirs.push(path.clone());
    }
    let refs: Vec<&str> = dirs.iter().map(String::as_str).collect();
    let deep = OutputIndex::parse(&index(&[], &[], &refs)).expect("parsed");
    assert_eq!(deep.tree("d").expect_err("too deep"), IndexError::TooDeep);
    // The same directories within the bound derive.
    let shallow = OutputIndex::parse(&index(&[], &[], &refs[..16])).expect("parsed");
    assert!(shallow.tree("d").is_ok());
}

#[test]
fn a_wire_digest_of_the_tree_names_the_same_bytes() {
    let parsed = OutputIndex::parse(&sample()).expect("parsed");
    let tree = parsed.tree("out/d").expect("derived");
    assert_eq!(tree.digest, sha(&tree_bytes(&parsed)));
}

fn tree_bytes(parsed: &OutputIndex) -> Vec<u8> {
    let mut children = Vec::new();
    let (root, _) = parsed.directory("out/d", &mut children).expect("directory");
    let tree = Tree {
        root: Some(root),
        children: children.into_iter().map(|(message, _)| message).collect(),
    };
    tree.encode_to_vec()
}
