//! tierstore's tree manifest reader at kbf's tierstore pin: the reader the manifest writer's tests
//! (docs/design/actions-as-jobs.md, section 11, items 10 and 11) check every manifest with, so
//! that writer and reader cannot drift. These tests hold the pin to a release that has it and
//! reads the shape kbf will write: a file whose bytes are a span of a pack, a file read from its
//! own `cas/l/` object, an executable, a symlink and a nested directory.

use std::collections::BTreeMap;

use tierstore::{
    BlobSource, EntryKind, MAX_DEPTH, MAX_ENTRIES, MAX_MANIFEST_BYTES, TreeDigest, TreeError,
    flatten, parse_manifest,
};

/// A SHA-256 in hex: the digest of a file's contents as a manifest names it.
const CONTENTS: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

/// The manifests of a two-level tree, by digest, and the root's digest.
fn tree() -> (BTreeMap<String, Vec<u8>>, TreeDigest) {
    let sub = format!(
        r#"{{"v":1,"children":[{{"name":"a.h","kind":"file","sha256":"{CONTENTS}","size":5,"source":{{"key":"cas/f/w/p0","offset":17}}}}]}}"#
    )
    .into_bytes();
    let sub_digest = TreeDigest::of(&sub);
    let root = format!(
        r#"{{"v":1,"children":[{{"name":"cc","kind":"file","exec":true,"sha256":"{CONTENTS}","size":5}},{{"name":"inc","kind":"dir","sha256":"{sub_digest}","size":{}}},{{"name":"lib","kind":"symlink","target":"inc"}}]}}"#,
        sub.len()
    )
    .into_bytes();
    let root_digest = TreeDigest::of(&root);
    let manifests = BTreeMap::from([
        (sub_digest.to_string(), sub),
        (root_digest.to_string(), root),
    ]);
    (manifests, root_digest)
}

/// Catches: a pin without the reader, or one whose reader lists a tree of kbf's shape in another
/// order, with other parents, or loses a span's pack key and offset, the executable bit or a
/// link's target.
#[test]
fn the_pinned_reader_flattens_a_tree_of_kbfs_shape() {
    let (manifests, root) = tree();
    let entries = flatten(&root, |digest| {
        manifests
            .get(&digest.to_string())
            .cloned()
            .ok_or("no such manifest")
    })
    .expect("a tree");
    let shape: Vec<(u32, Option<u32>, &str)> = entries
        .iter()
        .map(|e| (e.index, e.parent, &*e.name))
        .collect();
    assert_eq!(
        shape,
        [
            (0, None, ""),
            (1, Some(0), "cc"),
            (2, Some(0), "inc"),
            (3, Some(2), "a.h"),
            (4, Some(0), "lib"),
        ]
    );
    let contents = TreeDigest::from_hex(CONTENTS).expect("hex");
    assert_eq!(
        entries[1].kind,
        EntryKind::File {
            digest: contents,
            size: 5,
            exec: true,
            source: None
        }
    );
    assert_eq!(
        entries[3].kind,
        EntryKind::File {
            digest: contents,
            size: 5,
            exec: false,
            source: Some(BlobSource {
                key: "cas/f/w/p0".into(),
                offset: 17
            })
        }
    );
    assert_eq!(
        entries[4].kind,
        EntryKind::Symlink {
            target: "inc".into()
        }
    );
}

/// Catches: a pinned reader that takes what the writer must never produce, or refuses it under
/// another name: bytes that do not hash to their digest, a whiteout name, and a file of size 0
/// with a source. The limits the writer's tests hit at their boundaries are the design's.
#[test]
fn the_pinned_reader_refuses_by_name_and_has_the_designs_limits() {
    let (manifests, root) = tree();
    let root_bytes = &manifests[&root.to_string()];
    let other = TreeDigest::of(b"another");
    assert_eq!(
        parse_manifest(root_bytes, &other),
        Err(TreeError::DigestMismatch {
            expected: other,
            actual: root
        })
    );
    let whiteout = br#"{"v":1,"children":[{"name":".wh.x","kind":"symlink","target":"t"}]}"#;
    assert_eq!(
        parse_manifest(whiteout, &TreeDigest::of(whiteout)),
        Err(TreeError::UnsupportedName(".wh.x".to_owned()))
    );
    let empty = format!(
        r#"{{"v":1,"children":[{{"name":"e","kind":"file","sha256":"{CONTENTS}","size":0,"source":{{"key":"k","offset":0}}}}]}}"#
    );
    assert_eq!(
        parse_manifest(empty.as_bytes(), &TreeDigest::of(empty.as_bytes())),
        Err(TreeError::EmptyFileWithSource("e".to_owned()))
    );
    assert_eq!(MAX_ENTRIES, (1 << 24) - 1);
    assert_eq!(MAX_MANIFEST_BYTES, 64 * 1024 * 1024);
    assert_eq!(MAX_DEPTH, 4096);
}
