//! The writer of tree list version 2 ([`super::tstree`]): the golden example of tierstore's
//! stored formats written byte for byte, the bytes tierstore's own writer (`TreeList::encode_v2`,
//! at kbf's pin) writes for trees of every shape, the ranges, and what a record cannot hold.

use std::borrow::Cow;

use tierstore::{
    BlobSource, ListEntry, ListKind, ListRange, Mode, TreeDigest, TreeLimits, TreeList, TreeListV2,
};

use super::NoTree;
use super::tstree::{Child, EMPTY_SHA256, Item, RANGE_MERGE_GAP, encode, merged};

/// tierstore's golden example of version 2 (`tests/stored_formats/fixtures/tree_list_v2.bin` of
/// komira-ai/tierstore), copied as hex, 32 bytes a line: the entries of its version 1 example.
const GOLDEN_HEX: &str = include_str!("../../tests/golden/tree_list_v2.hex");

/// The bytes of the golden example.
fn golden() -> Vec<u8> {
    let hex: Vec<u8> = GOLDEN_HEX.bytes().filter(u8::is_ascii_hexdigit).collect();
    hex.chunks(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).expect("ASCII");
            u8::from_str_radix(text, 16).expect("two hex digits")
        })
        .collect()
}

/// The SHA-256 tierstore's golden test pins for the example.
const GOLDEN_SHA256: &str = "fec08016bb468cc39dc5155cada5ebfe6806cdb10b5d0cb58b7df724f6ac72dc";

/// The most bytes a list may have in these tests unless one says otherwise.
const ANY_SIZE: usize = 1 << 28;

/// A tree of the tests.
enum T {
    Dir(u16, Vec<(String, T)>),
    File {
        mode: u16,
        size: u64,
        sha256: [u8; 32],
        source: Option<(String, u64)>,
    },
    Link(String),
}

fn dir(mode: u16, children: Vec<(&str, T)>) -> T {
    T::Dir(
        mode,
        children
            .into_iter()
            .map(|(name, t)| (name.to_owned(), t))
            .collect(),
    )
}

fn file(mode: u16, size: u64, fill: u8, source: Option<(&str, u64)>) -> T {
    T::File {
        mode,
        size,
        sha256: if size == 0 { EMPTY_SHA256 } else { [fill; 32] },
        source: source.map(|(key, offset)| (key.to_owned(), offset)),
    }
}

fn link(target: &str) -> T {
    T::Link(target.to_owned())
}

/// The children of a directory of the tests, in the order the test gave them.
fn children(t: &T) -> Vec<Child<'_, &T>> {
    let T::Dir(_, kids) = t else {
        return Vec::new();
    };
    kids.iter()
        .map(|(name, kid)| Child {
            name,
            item: match kid {
                T::Dir(mode, _) => Item::Dir {
                    mode: *mode,
                    dir: kid,
                },
                T::File {
                    mode,
                    size,
                    sha256,
                    source,
                } => Item::File {
                    mode: *mode,
                    size: *size,
                    sha256: *sha256,
                    source: source
                        .as_ref()
                        .map(|(key, offset)| (Cow::Borrowed(key.as_str()), *offset)),
                },
                T::Link(target) => Item::Link { target },
            },
        })
        .collect()
}

/// The bytes this writer writes for `root`, spans merged across `gap`.
fn written(root: &T, gap: u64) -> Vec<u8> {
    encode(root, children, gap, ANY_SIZE).expect("a list")
}

/// The entries of `root` as tierstore's `TreeList`: every directory listed with its mode, every
/// file and link at its path.
fn entries(root: &T) -> Vec<ListEntry> {
    fn walk(t: &T, path: &str, out: &mut Vec<ListEntry>) {
        let T::Dir(_, kids) = t else { return };
        for (name, kid) in kids {
            let path = if path.is_empty() {
                name.clone()
            } else {
                format!("{path}/{name}")
            };
            let kind = match kid {
                T::Dir(mode, _) => ListKind::Dir {
                    mode: Mode::new(*mode).expect("a mode"),
                },
                T::File {
                    mode,
                    size,
                    sha256,
                    source,
                } => ListKind::File {
                    mode: Mode::new(*mode).expect("a mode"),
                    size: *size,
                    source: source.as_ref().map(|(key, offset)| BlobSource {
                        key: key.as_str().into(),
                        offset: *offset,
                    }),
                    sha256: Some(TreeDigest::from_bytes(*sha256)),
                },
                T::Link(target) => ListKind::Symlink {
                    target: target.clone(),
                },
            };
            out.push(ListEntry {
                path: path.clone(),
                kind,
            });
            walk(kid, &path, out);
        }
    }
    let mut out = Vec::new();
    walk(root, "", &mut out);
    out
}

/// What tierstore's writer writes for `root`.
fn tierstores(root: &T) -> Vec<u8> {
    TreeList {
        entries: entries(root),
    }
    .encode_v2()
    .expect("tierstore's writer takes it")
}

/// The tree of tierstore's golden example: `bin/cc` (0755, a span of `out/a1/0`),
/// `include/empty.h` (0644, no bytes), the link `lib` to `bin`, `share/doc.txt` (0644, a whole
/// object) and the empty directory `tmp` of mode 01777, given here in another order than the
/// sorted one.
fn golden_tree() -> T {
    dir(
        0o755,
        vec![
            ("tmp", dir(0o1777, vec![])),
            (
                "share",
                dir(
                    0o755,
                    vec![(
                        "doc.txt",
                        file(
                            0o644,
                            12,
                            0x44,
                            Some((&format!("cas/l/{}", "3".repeat(64)), 0)),
                        ),
                    )],
                ),
            ),
            ("lib", link("bin")),
            (
                "include",
                dir(0o755, vec![("empty.h", file(0o644, 0, 0, None))]),
            ),
            (
                "bin",
                dir(
                    0o755,
                    vec![(
                        "cc",
                        file(0o755, 1_048_576, 0x22, Some(("out/a1/0", 524_288))),
                    )],
                ),
            ),
        ],
    )
}

/// A tree of every shape the writer lays out: directories three deep, one empty; a name that is
/// in several directories, and a link whose target is a name; executable and plain files; files of
/// no bytes; whole objects; spans of three packs that touch, that are exactly the merge gap apart,
/// one byte more than it apart, that overlap, and one file named at two paths; names of more than
/// one byte a character; and every directory's children given unsorted.
fn every_shape() -> T {
    let gap = RANGE_MERGE_GAP;
    let p = |offset: u64| Some(("cas/f/w/p0", offset));
    dir(
        0o755,
        vec![
            ("zz", dir(0o755, vec![])),
            (
                "src",
                dir(
                    0o755,
                    vec![
                        ("lib", dir(0o755, vec![("a.rs", file(0o644, 10, 1, p(0)))])),
                        ("main.rs", file(0o644, 5, 2, p(10))),
                        ("b.rs", file(0o644, 7, 3, p(15 + gap))),
                        ("c.rs", file(0o644, 7, 4, p(22 + 2 * gap + 1))),
                        ("again.rs", file(0o644, 10, 1, p(0))),
                        ("inner.rs", file(0o644, 2, 5, p(3))),
                        ("é.rs", file(0o644, 3, 6, Some(("cas/f/w/p1", 0)))),
                        ("empty", file(0o644, 0, 0, None)),
                        ("lib.link", link("lib")),
                    ],
                ),
            ),
            (
                "bin",
                dir(
                    0o755,
                    vec![
                        ("tool", file(0o755, 100, 7, Some(("cas/f/w/p1", 1_000_000)))),
                        (
                            "lib",
                            dir(
                                0o755,
                                vec![
                                    (
                                        "deep",
                                        dir(0o755, vec![("x", file(0o644, 1, 8, p(4_000_000)))]),
                                    ),
                                    (
                                        "whole",
                                        file(
                                            0o644,
                                            9,
                                            9,
                                            Some((&format!("cas/l/{}", "9".repeat(64)), 0)),
                                        ),
                                    ),
                                ],
                            ),
                        ),
                        ("lib.link", link("lib/deep/x")),
                    ],
                ),
            ),
            ("README", file(0o644, 20, 10, Some(("cas/f/a/p2", 64)))),
            ("lib", link("src/lib")),
            ("empty.txt", file(0o644, 0, 0, None)),
        ],
    )
}

/// Asserts that the golden example of tierstore's stored formats is named by the digest tierstore
/// pins, that tierstore's writer at kbf's pin writes it, and that this writer writes it byte for
/// byte from the same tree, its children given unsorted.
#[test]
fn the_golden_example_is_written_byte_for_byte() {
    let golden = golden();
    assert_eq!(golden.len(), 824);
    assert_eq!(TreeDigest::of(&golden).to_string(), GOLDEN_SHA256);
    assert_eq!(tierstores(&golden_tree()), golden);
    assert_eq!(written(&golden_tree(), RANGE_MERGE_GAP), golden);
}

/// Asserts that for a tree of every shape (see [`every_shape`]) this writer writes exactly the
/// bytes tierstore's writer writes, and that tierstore's reader reads them: one range for the
/// spans of `cas/f/w/p0` within the gap of each other, another for the span past it, and one for
/// each of the other keys.
#[test]
fn a_tree_of_every_shape_is_the_bytes_tierstores_writer_writes() {
    let tree = every_shape();
    let bytes = written(&tree, RANGE_MERGE_GAP);
    assert_eq!(bytes, tierstores(&tree));
    let list = TreeListV2::open(&bytes).expect("the reader opens it");
    let mut spans = Vec::new();
    list.build(&TreeLimits::default(), |span| spans.push(span))
        .expect("the reader builds it");
    // Eleven files with bytes, two of them of one digest: a span is handed out per digest.
    assert_eq!(spans.len(), 10);
    let gap = RANGE_MERGE_GAP;
    let range = |key: &str, offset: u64, len: u64| ListRange {
        key: key.into(),
        offset,
        len,
    };
    let ranges: Vec<ListRange> = list.ranges().collect();
    assert_eq!(
        ranges,
        [
            range("cas/f/a/p2", 64, 20),
            range("cas/f/w/p0", 0, 22 + gap),
            range("cas/f/w/p0", 22 + 2 * gap + 1, 7),
            range("cas/f/w/p0", 4_000_000, 1),
            range("cas/f/w/p1", 0, 3),
            range("cas/f/w/p1", 1_000_000, 100),
            range(&format!("cas/l/{}", "9".repeat(64)), 0, 9),
        ]
    );
}

/// Asserts that a tree's bytes do not depend on the order its directories give their children
/// in: the tree of every shape, each directory's children reversed, is the same bytes.
#[test]
fn the_children_are_sorted_whatever_order_they_come_in() {
    fn reversed(t: T) -> T {
        match t {
            T::Dir(mode, kids) => T::Dir(
                mode,
                kids.into_iter()
                    .rev()
                    .map(|(name, kid)| (name, reversed(kid)))
                    .collect(),
            ),
            other => other,
        }
    }
    assert_eq!(
        written(&reversed(every_shape()), RANGE_MERGE_GAP),
        written(&every_shape(), RANGE_MERGE_GAP)
    );
}

/// Asserts the merge of spans into ranges: a span that begins exactly the gap after a range's
/// end joins it, one a byte further does not; a span inside a range does not shorten it; spans of
/// two keys are never merged, even when they touch; with a gap of 0 only touching spans merge;
/// and a span that ends past 2^64 ends at 2^64 - 1.
#[test]
fn the_ranges_merge_across_the_gap_and_no_further() {
    let gap = 100;
    assert_eq!(merged(vec![(0, 0, 10), (0, 110, 5)], gap), [(0, 0, 115)]);
    assert_eq!(
        merged(vec![(0, 111, 5), (0, 0, 10)], gap),
        [(0, 0, 10), (0, 111, 116)]
    );
    assert_eq!(merged(vec![(0, 0, 50), (0, 10, 5)], gap), [(0, 0, 50)]);
    assert_eq!(
        merged(vec![(1, 10, 5), (0, 0, 10)], gap),
        [(0, 0, 10), (1, 10, 15)]
    );
    assert_eq!(
        merged(vec![(0, 0, 10), (0, 10, 1), (0, 12, 1)], 0),
        [(0, 0, 11), (0, 12, 13)]
    );
    assert_eq!(
        merged(vec![(0, u64::MAX - 1, 5)], gap),
        [(0, u64::MAX - 1, u64::MAX)]
    );
    assert!(merged(Vec::new(), gap).is_empty());
}

/// Asserts that the gap is the writer's: the tree of every shape with a gap of 0 has a range for
/// each span of `cas/f/w/p0` that does not touch another (the spans at 0, 3 and 10 touch or lie
/// inside one another), and is otherwise the bytes written with the default gap.
#[test]
fn a_smaller_gap_writes_more_ranges_and_nothing_else_differs() {
    let tree = every_shape();
    let narrow = written(&tree, 0);
    let wide = written(&tree, RANGE_MERGE_GAP);
    let list = TreeListV2::open(&narrow).expect("the reader opens it");
    let p0: Vec<(u64, u64)> = list
        .ranges()
        .filter(|range| &*range.key == "cas/f/w/p0")
        .map(|range| (range.offset, range.len))
        .collect();
    let gap = RANGE_MERGE_GAP;
    assert_eq!(
        p0,
        [
            (0, 15),
            (15 + gap, 7),
            (22 + 2 * gap + 1, 7),
            (4_000_000, 1)
        ]
    );
    // The ranges are the last section: everything before them is the same.
    let ranges_at = |bytes: &[u8]| u32::from_le_bytes(bytes[56..60].try_into().expect("4 bytes"));
    assert_eq!(ranges_at(&narrow), ranges_at(&wide));
    let at = ranges_at(&wide) as usize;
    assert_eq!(narrow[..56], wide[..56]);
    assert_eq!(narrow[64..at], wide[64..at]);
}

/// Asserts that a file of no bytes given a source keeps its key in the keys, as tierstore's
/// writer does, and has no key, offset or range of its own.
#[test]
fn an_empty_file_with_a_source_keeps_its_key_in_the_keys_and_no_span() {
    let tree = dir(
        0o755,
        vec![("e", file(0o644, 0, 0, Some(("cas/f/w/p0", 77))))],
    );
    let bytes = written(&tree, RANGE_MERGE_GAP);
    assert_eq!(bytes, tierstores(&tree));
    let list = TreeListV2::open(&bytes).expect("the reader opens it");
    assert_eq!(list.ranges().count(), 0);
    // One key, `cas/f/w/p0`.
    assert_eq!(
        u32::from_le_bytes(bytes[36..40].try_into().expect("4 bytes")),
        1
    );
    // The file's record: no key, no offset, size 0.
    let record = &bytes[64 + 32..64 + 64];
    assert_eq!(record[8..12], [0; 4]);
    assert_eq!(record[16..32], [0; 16]);
}

/// Asserts that a name of 255 bytes, the most a record's length byte holds, is written and read,
/// and that one of 256 is refused by name, `unrepresentable`, wherever it is in the tree.
#[test]
fn a_name_of_255_bytes_is_written_and_one_of_256_is_refused() {
    let longest = "n".repeat(255);
    let tree = dir(0o755, vec![(&longest, file(0o644, 0, 0, None))]);
    let bytes = written(&tree, RANGE_MERGE_GAP);
    TreeListV2::open(&bytes)
        .expect("opened")
        .build(&TreeLimits::default(), |_| {})
        .expect("the reader takes a name of 255 bytes");
    let over = "n".repeat(256);
    let tree = dir(
        0o755,
        vec![("d", dir(0o755, vec![(&over, file(0o644, 0, 0, None))]))],
    );
    let refused = encode(&tree, children, RANGE_MERGE_GAP, ANY_SIZE).expect_err("refused");
    assert_eq!(
        refused,
        NoTree::ListingRefused {
            why: format!("the name {over:?} is 256 bytes, over the 255 a record holds"),
        }
    );
    assert_eq!(refused.why(), "unrepresentable");
}

/// Asserts the size limit: a list of exactly `max_bytes` is written, one byte less refuses it by
/// name (the header and sections laid out), and a tree whose records alone pass the limit is
/// refused while it is walked, before the rest of it is laid out.
#[test]
fn a_list_of_the_limit_is_written_and_one_byte_over_is_refused() {
    let tree = every_shape();
    let size = written(&tree, RANGE_MERGE_GAP).len();
    let exact = encode(&tree, children, RANGE_MERGE_GAP, size).expect("exactly the limit");
    assert_eq!(exact.len(), size);
    let refused = encode(&tree, children, RANGE_MERGE_GAP, size - 1).expect_err("one over");
    assert_eq!(
        refused,
        NoTree::ListingRefused {
            why: format!(
                "the tree list is {size} bytes or more, over the {} tierstore reads",
                size - 1
            ),
        }
    );
    assert_eq!(refused.why(), "unrepresentable");
    // The root and its six children, 64 bytes each, and their 33 bytes of names (`lib`'s target
    // among them): refused once the root is walked.
    let walked = encode(&tree, children, RANGE_MERGE_GAP, 64).expect_err("refused early");
    assert_eq!(
        walked,
        NoTree::ListingRefused {
            why: "the tree list is 481 bytes or more, over the 64 tierstore reads".to_owned(),
        }
    );
}

/// Asserts that an empty tree is the root alone: one entry, no keys, names or ranges, which
/// tierstore's writer writes too and its reader reads.
#[test]
fn an_empty_tree_is_its_root_alone() {
    let tree = dir(0o755, vec![]);
    let bytes = written(&tree, RANGE_MERGE_GAP);
    assert_eq!(bytes, tierstores(&tree));
    let list = TreeListV2::open(&bytes).expect("opened");
    assert_eq!(list.len(), 1);
    list.build(&TreeLimits::default(), |_| {}).expect("built");
}
