//! What tierstore cannot represent: `node_properties`, names its reader refuses, and each of
//! its limits at its boundary and one past it, counted over the whole action tree with the
//! base's facts. Each is `why=unrepresentable`, by name, and writes nothing.

use kbf_front::action_tree::{BaseTreeError, RESERVED_NAMES};
use kbf_proto::reapi::NodeProperties;
use tierstore::{MAX_DEPTH, MAX_ENTRIES, MAX_MANIFEST_BYTES, TreeError};

use super::*;

/// `node_properties` that set one thing each, and one that sets nothing.
fn properties() -> [(NodeProperties, bool); 4] {
    let named = NodeProperties {
        properties: vec![reapi::NodeProperty {
            name: "owner".to_owned(),
            value: "root".to_owned(),
        }],
        ..NodeProperties::default()
    };
    let mtime = NodeProperties {
        mtime: Some(prost_types::Timestamp {
            seconds: 1,
            nanos: 0,
        }),
        ..NodeProperties::default()
    };
    let mode = NodeProperties {
        unix_mode: Some(0o600),
        ..NodeProperties::default()
    };
    [
        (named, true),
        (mtime, true),
        (mode, true),
        (NodeProperties::default(), false),
    ]
}

/// Catches: `node_properties` dropped from a tree that cannot carry them (on the Directory, on
/// a file node, on a symlink node, in a subdirectory), or an empty `NodeProperties` refused
/// though it sets nothing.
#[tokio::test]
async fn node_properties_anywhere_make_no_tree() {
    for (set, refused) in properties() {
        let rig = Rig::new().await;
        let blob = rig.farm.packed("f", "cas/f/w/p0", 0).await;
        let on_dir = Directory {
            node_properties: Some(set.clone()),
            ..directory(vec![], vec![], vec![link("l", "t")])
        };
        let on_file = directory(
            vec![FileNode {
                node_properties: Some(set.clone()),
                ..file("f", &blob, false)
            }],
            vec![],
            vec![],
        );
        let on_link = directory(
            vec![],
            vec![],
            vec![SymlinkNode {
                node_properties: Some(set.clone()),
                ..link("l", "t")
            }],
        );
        let nested_sub = rig.store(&on_dir).await;
        let nested = directory(vec![], vec![subdir("sub", &nested_sub)], vec![]);
        let cases = [
            (on_dir.clone(), ""),
            (on_file, "f"),
            (on_link, "l"),
            (nested, ""),
        ];
        for (message, node) in cases {
            let root = rig.store(&message).await;
            let answer = rig.write(&root).await;
            if refused {
                let directory = if message.directories.is_empty() {
                    root
                } else {
                    nested_sub
                };
                let expected = NoTree::NodeProperties {
                    directory,
                    node: node.to_owned(),
                };
                assert_eq!(answer, Err(expected));
                assert_eq!(answer.unwrap_err().why(), "unrepresentable");
            } else {
                answer.expect("an empty NodeProperties sets nothing");
            }
        }
        if refused {
            assert!(rig.farm.puts().is_empty(), "nothing is written");
        }
    }
}

/// Catches: a name tierstore's reader refuses (a whiteout's `.wh.` prefix, `..`, `.`, empty, a
/// slash) or one name given twice across REAPI's node lists, stored in a manifest: no tree,
/// the reader's own refusal named, nothing written.
#[tokio::test]
async fn a_name_tierstores_reader_refuses_makes_no_tree() {
    let rig = Rig::new().await;
    let blob = rig.farm.packed("f", "cas/f/w/p0", 0).await;
    let leaf = rig
        .store(&directory(vec![], vec![], vec![link("x", "t")]))
        .await;
    let cases = [
        (
            directory(vec![file(".wh.x", &blob, false)], vec![], vec![]),
            TreeError::UnsupportedName(".wh.x".to_owned()),
        ),
        (
            directory(vec![], vec![], vec![link("..", "t")]),
            TreeError::UnsupportedName("..".to_owned()),
        ),
        (
            directory(vec![], vec![subdir(".", &leaf)], vec![]),
            TreeError::UnsupportedName(".".to_owned()),
        ),
        (
            directory(vec![file("", &blob, false)], vec![], vec![]),
            TreeError::UnsupportedName(String::new()),
        ),
        (
            directory(vec![], vec![], vec![link("a/b", "t")]),
            TreeError::UnsupportedName("a/b".to_owned()),
        ),
        (
            directory(
                vec![file("x", &blob, false)],
                vec![subdir("x", &leaf)],
                vec![],
            ),
            TreeError::Duplicate("x".to_owned()),
        ),
    ];
    for (message, error) in cases {
        rig.farm.clear_records();
        let root = rig.store(&message).await;
        let answer = rig.write(&root).await;
        assert_eq!(
            answer,
            Err(NoTree::Refused {
                directory: root,
                error
            })
        );
        assert_eq!(answer.unwrap_err().why(), "unrepresentable");
        assert!(rig.farm.puts().is_empty(), "nothing is written");
    }
}

/// Catches: entries counted without the base's, without the four the action tree adds, or
/// with a shared subtree counted once where tierstore counts it at each place: an input root
/// that fits alone but not beside the base is refused, one entry under the limit is a tree.
#[tokio::test]
async fn the_entry_limit_is_counted_over_the_whole_action_tree() {
    let rig = Rig::new().await;
    let a = rig.farm.packed("a", "cas/f/w/p0", 0).await;
    let b = rig.farm.packed("b", "cas/f/w/p0", 1).await;
    let deep = rig
        .store(&directory(
            vec![file("a", &a, false), file("b", &b, false)],
            vec![],
            vec![],
        ))
        .await;
    let one = rig
        .store(&directory(vec![], vec![subdir("d", &deep)], vec![]))
        .await;
    let two = rig
        .store(&directory(vec![], vec![subdir("d", &deep)], vec![]))
        .await;
    let root = rig
        .store(&directory(
            vec![],
            vec![subdir("one", &one), subdir("two", &two)],
            vec![],
        ))
        .await;
    // two, one, their `d` each, and `d`'s two files at each place.
    let input = 2 + 2 + 4;
    let written = rig.write(&root).await.expect("a tree");
    let read = rig.farm.flatten(&written).expect("the reader takes it");
    assert_eq!(read.len() as u64 - 1, Base::FACTS.entries + input + 4);
    let at = |entries: u64| {
        rig.base.with_facts(BaseFacts {
            entries,
            depth: Base::FACTS.depth,
        })
    };
    rig.writer.forget(&root);
    rig.write_over(&root, &at(MAX_ENTRIES - input - 4))
        .await
        .expect("at the limit");
    rig.writer.forget(&root);
    rig.farm.clear_records();
    let answer = rig.write_over(&root, &at(MAX_ENTRIES - input - 3)).await;
    assert_eq!(
        answer,
        Err(NoTree::TooManyEntries {
            entries: MAX_ENTRIES + 1
        })
    );
    assert_eq!(answer.unwrap_err().why(), "unrepresentable");
    assert!(rig.farm.puts().is_empty());
}

/// A chain of `levels` nested directories, the innermost empty: an input root `levels` deep.
async fn chain(rig: &Rig, levels: usize) -> Digest {
    let mut inner = kbf_segments::sha256(&[]);
    for _ in 1..levels {
        inner = rig
            .store(&directory(vec![], vec![subdir("d", &inner)], vec![]))
            .await;
    }
    rig.store(&directory(vec![], vec![subdir("d", &inner)], vec![]))
        .await
}

/// Catches: the depth limit counted without `work`'s level, or without the base's depth: an
/// input root one level short of the limit beside `work` is a tree the reader takes, one level
/// more is refused; a base at the limit is a tree, one past it is refused.
#[tokio::test]
async fn the_depth_limit_is_counted_over_the_whole_action_tree() {
    let rig = Rig::new().await;
    let fits = chain(&rig, MAX_DEPTH - 1).await;
    let written = rig.write(&fits).await.expect("a tree at the limit");
    rig.farm.flatten(&written).expect("the reader takes it");
    let deeper = chain(&rig, MAX_DEPTH).await;
    let answer = rig.write(&deeper).await;
    assert_eq!(
        answer,
        Err(NoTree::TooDeep {
            depth: MAX_DEPTH + 1
        })
    );
    assert_eq!(answer.unwrap_err().why(), "unrepresentable");
    let flat = rig
        .store(&directory(vec![], vec![], vec![link("l", "t")]))
        .await;
    let base = |depth| {
        rig.base.with_facts(BaseFacts {
            entries: Base::FACTS.entries,
            depth,
        })
    };
    rig.write_over(&flat, &base(MAX_DEPTH))
        .await
        .expect("at the limit");
    assert_eq!(
        rig.write_over(&flat, &base(MAX_DEPTH + 1)).await,
        Err(NoTree::TooDeep {
            depth: MAX_DEPTH + 1
        })
    );
}

/// A base whose root holds one more child than the test base's: the symlink `zz-pad` (sorted last) to a
/// target of `pad` bytes.
fn padded_base(rig: &Rig, pad: usize) -> BaseTree {
    let base = String::from_utf8(rig.base.root.clone()).expect("text");
    let children = base
        .strip_prefix(r#"{"v":1,"children":["#)
        .and_then(|rest| rest.strip_suffix("]}"))
        .expect("a manifest");
    let root = format!(
        r#"{{"v":1,"children":[{children},{{"name":"zz-pad","kind":"symlink","target":"{}"}}]}}"#,
        "p".repeat(pad)
    );
    BaseTree::new(root.as_bytes(), Base::FACTS).expect("a base")
}

/// Catches: a manifest over tierstore's size limit stored (its reader would refuse the tree),
/// or one at the limit refused: the action tree's root, the base's children and `work` and
/// `.kbf` beside them, at exactly the limit is a tree; one byte more is refused by name.
#[tokio::test]
async fn the_manifest_size_limit_is_kept_to_the_byte() {
    let rig = Rig::new().await;
    let root = rig
        .store(&directory(vec![], vec![], vec![link("l", "t")]))
        .await;
    let small = rig
        .write_over(&root, &padded_base(&rig, 1))
        .await
        .expect("a tree");
    let small_len = rig.farm.manifest(&small.digest().to_string()).len();
    let pad = 1 + MAX_MANIFEST_BYTES - small_len;
    let at = rig
        .write_over(&root, &padded_base(&rig, pad))
        .await
        .expect("at the limit");
    let at_len = rig
        .farm
        .object(&whole_key(&at.digest().to_string()))
        .map(|bytes| bytes.len());
    assert_eq!(at_len, Some(MAX_MANIFEST_BYTES));
    rig.farm.clear_records();
    let answer = rig.write_over(&root, &padded_base(&rig, pad + 1)).await;
    assert_eq!(
        answer,
        Err(NoTree::RootRefused {
            error: TreeError::ManifestTooLarge {
                size: MAX_MANIFEST_BYTES + 1
            }
        })
    );
    assert_eq!(answer.unwrap_err().why(), "unrepresentable");
    assert!(rig.farm.puts().is_empty());
}

/// Catches: a base tree that holds a name each action tree adds (or `kbf-task`'s run
/// directory), which the root would hold twice, or a root manifest tierstore cannot read,
/// taken as a base.
#[test]
fn a_base_holding_a_reserved_name_or_unreadable_is_refused() {
    assert_eq!(RESERVED_NAMES, ["work", ".kbf", ".kbf-run"]);
    for name in RESERVED_NAMES {
        let root =
            format!(r#"{{"v":1,"children":[{{"name":"{name}","kind":"symlink","target":"t"}}]}}"#);
        let facts = Base::FACTS;
        assert_eq!(
            BaseTree::new(root.as_bytes(), facts),
            Err(BaseTreeError::Reserved(name.to_owned()))
        );
    }
    let unsorted = br#"{"v":1,"children":[{"name":"b","kind":"symlink","target":"t"},{"name":"a","kind":"symlink","target":"t"}]}"#;
    assert_eq!(
        BaseTree::new(unsorted, Base::FACTS),
        Err(BaseTreeError::Manifest(TreeError::Unsorted {
            previous: "b".to_owned(),
            name: "a".to_owned()
        }))
    );
}

/// Catches: a refusal whose text does not say what was refused and why, or a reason counted
/// under the wrong fallback.
#[test]
fn every_refusal_says_what_and_why() {
    let digest = kbf_segments::sha256(b"x");
    let text = |no: &NoTree| (no.to_string(), no.why());
    let cases = [
        (
            NoTree::NodeProperties {
                directory: digest,
                node: "f".to_owned(),
            },
            format!("the Directory {digest} sets node_properties (on \"f\"; empty: on itself)"),
            "unrepresentable",
        ),
        (
            NoTree::Refused {
                directory: digest,
                error: TreeError::Duplicate("x".to_owned()),
            },
            format!(
                "the manifest of the Directory {digest} is one tierstore refuses: duplicate entry name \"x\""
            ),
            "unrepresentable",
        ),
        (
            NoTree::RootRefused {
                error: TreeError::TreeTooLarge,
            },
            "the action tree's root is one tierstore refuses: tree too large: more than 16777215 entries"
                .to_owned(),
            "unrepresentable",
        ),
        (
            NoTree::TooManyEntries { entries: 1 << 24 },
            "the action tree has 16777216 entries, over tierstore's 16777215".to_owned(),
            "unrepresentable",
        ),
        (
            NoTree::TooDeep { depth: 4097 },
            "the action tree is 4097 levels deep, over tierstore's 4096".to_owned(),
            "unrepresentable",
        ),
        (
            NoTree::Lookup {
                why: "down".to_owned(),
            },
            "where the blobs are could not be found out: down".to_owned(),
            "store",
        ),
        (
            NoTree::Deadline,
            "where the blobs are was not found out within 30s".to_owned(),
            "store",
        ),
        (
            NoTree::WrongLength {
                digest,
                length: 7,
            },
            format!("cas/l/ holds an object of 7 bytes for {digest}"),
            "store",
        ),
        (
            NoTree::Read {
                digest,
                why: "gone".to_owned(),
            },
            format!("{digest} could not be read to be written to cas/l/: gone"),
            "store",
        ),
        (
            NoTree::Mismatch { digest },
            format!("the bytes read for {digest} are not its"),
            "store",
        ),
        (
            NoTree::Put {
                key: "cas/l/k".to_owned(),
                why: "refused".to_owned(),
            },
            "the write of cas/l/k failed: refused".to_owned(),
            "store",
        ),
        (
            NoTree::NotKept { directory: digest },
            format!("the closure kept no Directory {digest}"),
            "store",
        ),
    ];
    for (no, message, why) in cases {
        assert_eq!(text(&no), (message, why));
    }
    let base = BaseTreeError::Reserved("work".to_owned());
    assert_eq!(
        base.to_string(),
        "the base tree's root holds \"work\", a name the action tree keeps for itself"
    );
    let base = BaseTreeError::Manifest(TreeError::UnsupportedVersion);
    assert_eq!(
        base.to_string(),
        "the base tree's root manifest: unsupported manifest version"
    );
}
