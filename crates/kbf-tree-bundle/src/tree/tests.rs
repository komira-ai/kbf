//! The manifests a tree is written as, read back by tierstore's own reader and flattener.

use std::collections::BTreeMap;

use tierstore::{EntryKind, TreeDigest, flatten, parse_manifest};

use super::{Built, Dir, Limits, MAX_DEPTH, MAX_ENTRIES, MAX_MANIFEST_BYTES, Node, TreeError};

/// `/bin` -> `usr/bin`, `/etc/hosts` (0644), `/usr/bin/tool` (0755), `/usr/bin/empty` (empty),
/// `/tmp` (an empty directory).
fn small() -> Dir {
    let mut bin = Dir::new();
    bin.insert("tool", Node::file(b"\x7fELF tool", true));
    bin.insert("empty", Node::file(b"", false));
    let mut usr = Dir::new();
    usr.insert("bin", Node::Dir(bin));
    let mut etc = Dir::new();
    etc.insert("hosts", Node::file(b"127.0.0.1 localhost\n", false));
    let mut root = Dir::new();
    root.insert("usr", Node::Dir(usr));
    root.insert("etc", Node::Dir(etc));
    root.insert("tmp", Node::Dir(Dir::new()));
    root.insert(
        "bin",
        Node::Symlink {
            target: "usr/bin".to_owned(),
        },
    );
    root
}

/// The manifests of `built` by digest.
fn by_digest(built: &Built) -> BTreeMap<String, Vec<u8>> {
    built.manifests.iter().cloned().collect()
}

/// Asserts every manifest of a tree is one tierstore's reader takes under its digest, that
/// tierstore's flattener counts the entries the tree says, and that each manifest comes after
/// those it names, the root's last. Catches: a manifest tierstore refuses (unsorted, a member
/// it reads otherwise), a digest that is not the bytes', an entry count that is not
/// tierstore's, and an order that publishes a manifest before what it names.
#[test]
fn every_manifest_is_read_by_tierstore_and_the_counts_are_tierstores() {
    let built = small().build().expect("built");
    let manifests = by_digest(&built);
    for (hex, bytes) in &built.manifests {
        let digest = TreeDigest::from_hex(hex).expect("hex");
        let manifest = parse_manifest(bytes, &digest).expect("tierstore reads it");
        for child in &manifest.children {
            if let EntryKind::Dir { digest, size } = &child.kind {
                let at = built
                    .manifests
                    .iter()
                    .position(|(h, _)| *h == digest.to_string())
                    .expect("named manifest is in the tree");
                let me = built
                    .manifests
                    .iter()
                    .position(|(h, _)| h == hex)
                    .expect("me");
                assert!(at < me, "{} named before it is written", child.name);
                assert_eq!(*size, manifests[&digest.to_string()].len() as u64);
            }
        }
    }
    assert_eq!(built.manifests.last().map(|(h, _)| h), Some(&built.root));
    let root = TreeDigest::from_hex(&built.root).expect("root");
    let entries = flatten(&root, |d: &TreeDigest| {
        manifests.get(&d.to_string()).cloned().ok_or("missing")
    })
    .expect("tierstore flattens it");
    assert_eq!(entries.len() as u64, built.entries + 1);
    assert_eq!(built.entries, 8);
    assert_eq!(built.depth, 3);
}

/// Asserts the root manifest's bytes exactly: children sorted bytewise, each with its members in
/// one order and no space, a file with `exec` and no `source`, a directory by its manifest's
/// digest and length, a symlink by its target. Catches: any change to the bytes, which would
/// change every base tree's digest.
#[test]
fn the_root_manifest_is_these_bytes() {
    let built = small().build().expect("built");
    let manifests = by_digest(&built);
    let mut etc = Dir::new();
    etc.insert("hosts", Node::file(b"127.0.0.1 localhost\n", false));
    let etc = etc.build().expect("etc");
    let empty = Dir::new().build().expect("empty");
    assert_eq!(empty.manifests[0].1, br#"{"v":1,"children":[]}"#.to_vec());
    assert_eq!(
        String::from_utf8(etc.manifests[0].1.clone()).expect("utf8"),
        format!(
            r#"{{"v":1,"children":[{{"name":"hosts","kind":"file","sha256":"{}","size":20,"exec":false}}]}}"#,
            hex_of(b"127.0.0.1 localhost\n")
        )
    );
    let mut bin = Dir::new();
    bin.insert("tool", Node::file(b"t", true));
    assert!(
        String::from_utf8(bin.build().expect("bin").manifests[0].1.clone())
            .expect("utf8")
            .ends_with(r#""size":1,"exec":true}]}"#)
    );
    let root = String::from_utf8(manifests[&built.root].clone()).expect("utf8");
    let usr = &built.manifests[built.manifests.len() - 2];
    assert_eq!(
        root,
        format!(
            concat!(
                r#"{{"v":1,"children":["#,
                r#"{{"name":"bin","kind":"symlink","target":"usr/bin"}},"#,
                r#"{{"name":"etc","kind":"dir","sha256":"{etc}","size":{etc_len}}},"#,
                r#"{{"name":"tmp","kind":"dir","sha256":"{empty}","size":21}},"#,
                r#"{{"name":"usr","kind":"dir","sha256":"{usr}","size":{usr_len}}}]}}"#
            ),
            etc = etc.root,
            etc_len = etc.manifests[0].1.len(),
            empty = empty.root,
            usr = usr.0,
            usr_len = usr.1.len(),
        )
    );
}

/// The SHA-256 of `bytes` in hex.
fn hex_of(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// Asserts a subtree that occurs twice is one manifest in the list, and is counted at each
/// place. Catches: a manifest written twice (published twice), or a count that is not
/// tierstore's (a directory named twice counted twice).
#[test]
fn a_subtree_named_twice_is_written_once_and_counted_twice() {
    let mut twin = Dir::new();
    twin.insert("f", Node::file(b"f", false));
    let mut root = Dir::new();
    root.insert("a", Node::Dir(twin.clone()));
    root.insert("b", Node::Dir(twin));
    let built = root.build().expect("built");
    assert_eq!(built.manifests.len(), 2);
    assert_eq!(built.entries, 4);
    assert_eq!(built.depth, 2);
}

/// Asserts an empty root is a tree of no entry and depth 0. Catches: a depth or count off by one
/// at the root.
#[test]
fn an_empty_root_has_no_entry_and_depth_zero() {
    let built = Dir::new().build().expect("built");
    assert_eq!((built.entries, built.depth), (0, 0));
    assert_eq!(built.manifests.len(), 1);
}

/// Asserts each name tierstore refuses is refused by name and directory, at the root and below.
/// Catches: a name that would make tierstore refuse the whole tree on the nodes (a `.wh.` left
/// after the whiteouts, among them) passing the build.
#[test]
fn each_name_tierstore_refuses_is_refused_with_its_directory() {
    for name in ["", ".", "..", "a/b", "a\0b", ".wh.gone", ".wh..wh..opq"] {
        let mut root = Dir::new();
        root.insert(name, Node::file(b"x", false));
        assert_eq!(
            root.build(),
            Err(TreeError::Name {
                path: "/".to_owned(),
                name: name.to_owned()
            }),
            "{name:?}"
        );
        let mut etc = Dir::new();
        etc.insert(name, Node::file(b"x", false));
        let mut root = Dir::new();
        root.insert("etc", Node::Dir(etc));
        assert_eq!(
            root.build(),
            Err(TreeError::Name {
                path: "/etc".to_owned(),
                name: name.to_owned()
            })
        );
    }
    let mut root = Dir::new();
    root.insert(".whiteout-like", Node::file(b"x", false));
    root.insert("..a", Node::file(b"x", false));
    assert!(root.build().is_ok(), "names that only look alike are taken");
}

/// A chain of `levels` directories, `d/d/...`, with a file at the bottom.
fn chain(levels: usize) -> Dir {
    let mut dir = Dir::new();
    dir.insert("f", Node::file(b"f", false));
    for _ in 0..levels {
        let mut up = Dir::new();
        up.insert("d", Node::Dir(dir));
        dir = up;
    }
    dir
}

/// Asserts the depth limit at its boundary and one past it. Catches: a tree one level too deep
/// for tierstore passing, or one at the limit refused.
#[test]
fn the_depth_limit_holds_at_its_boundary() {
    let limits = Limits {
        depth: 4,
        ..Limits::TIERSTORE
    };
    let at = chain(3).build_within(limits).expect("at the limit");
    assert_eq!(at.depth, 4);
    assert_eq!(
        chain(4).build_within(limits),
        Err(TreeError::TooDeep {
            path: "/d/d/d/d/f".to_owned()
        })
    );
}

/// Asserts the entry limit at its boundary and one past it. Catches: a tree with one entry too
/// many for tierstore passing, or one at the limit refused.
#[test]
fn the_entry_limit_holds_at_its_boundary() {
    let limits = Limits {
        entries: 5,
        ..Limits::TIERSTORE
    };
    let mut root = Dir::new();
    for name in ["a", "b", "c", "d", "e"] {
        root.insert(name, Node::file(name.as_bytes(), false));
    }
    assert_eq!(root.build_within(limits).expect("at the limit").entries, 5);
    root.insert("f", Node::file(b"f", false));
    assert_eq!(
        root.build_within(limits),
        Err(TreeError::TooManyEntries { entries: 6 })
    );
}

/// Asserts the manifest size limit at its boundary and one past it, at the root and below.
/// Catches: a manifest tierstore would refuse as too large passing the build.
#[test]
fn the_manifest_size_limit_holds_at_its_boundary() {
    let empty = br#"{"v":1,"children":[]}"#.len();
    let limits = Limits {
        manifest_bytes: empty,
        ..Limits::TIERSTORE
    };
    assert!(Dir::new().build_within(limits).is_ok());
    let mut root = Dir::new();
    root.insert("a", Node::Dir(Dir::new()));
    let size = root.build().expect("built").manifests[1].1.len();
    assert_eq!(
        root.build_within(limits),
        Err(TreeError::ManifestTooLarge {
            path: "/".to_owned(),
            size
        })
    );
    let mut deeper = Dir::new();
    deeper.insert("x", Node::Dir(root));
    let limits = Limits {
        manifest_bytes: size - 1,
        ..Limits::TIERSTORE
    };
    assert_eq!(
        deeper.build_within(limits),
        Err(TreeError::ManifestTooLarge {
            path: "/x".to_owned(),
            size
        })
    );
}

/// Asserts the limits the build holds a tree to are tierstore's own. Catches: a limit here that
/// drifted from the reader on the nodes.
#[test]
fn the_limits_are_tierstores() {
    assert_eq!(MAX_ENTRIES, tierstore::MAX_ENTRIES);
    assert_eq!(MAX_DEPTH, tierstore::MAX_DEPTH);
    assert_eq!(MAX_MANIFEST_BYTES, tierstore::MAX_MANIFEST_BYTES);
}

/// Asserts the accessors of a directory: a child got, changed, replaced, taken out, and all of
/// them cleared, the children listed in bytewise order. Catches: a builder of the tree (the
/// layers applied in order) that cannot see or change what an earlier layer made.
#[test]
fn a_directory_is_changed_in_place() {
    let mut dir = Dir::new();
    assert_eq!(dir.insert("b", Node::file(b"1", false)), None);
    assert!(dir.insert("a", Node::Dir(Dir::new())).is_none());
    assert_eq!(
        dir.insert("b", Node::file(b"2", false)),
        Some(Node::file(b"1", false))
    );
    assert_eq!(dir.get("b"), Some(&Node::file(b"2", false)));
    if let Some(Node::Dir(a)) = dir.get_mut("a") {
        a.insert("c", Node::file(b"3", true));
    }
    let names: Vec<&str> = dir.children().map(|(n, _)| n).collect();
    assert_eq!(names, ["a", "b"]);
    assert!(matches!(dir.get("a"), Some(Node::Dir(a)) if a.get("c").is_some()));
    assert!(dir.remove("b").is_some());
    assert!(dir.get("b").is_none());
    dir.clear();
    assert_eq!(dir.children().count(), 0);
}
