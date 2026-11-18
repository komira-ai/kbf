//! The action tree as one tierstore tree list, version 2 (`TreeWriter::write_listing`): what the
//! stored list names for each kind of entry, the base tree's entries, the ranges it says the tree
//! reads, the key it is stored at, and every refusal and store fault, by name.
//!
//! A stored list is read back with tierstore's own reader at kbf's pin (`TreeListV2::open`, then
//! `build`, as the mount provider reads it) and the tree it builds described entry by entry; no
//! test reads the encoded bytes.

use kbf_front::action_tree::Listing;
use tierstore::{BuiltTree, ListRange, Mode, RecordChild, TreeLimits, TreeListV2, TreeRecord};

use super::*;

/// The SHA-256 of no bytes, in hex: what a file of size 0 has.
pub(super) const EMPTY_HEX: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// The base of the rig with every entry below its root listed, from the manifests the farm
/// holds.
pub(super) fn listed_base(rig: &Rig) -> BaseTree {
    rig.base
        .tree
        .clone()
        .listed(|digest: &TreeDigest| {
            rig.farm
                .object(&whole_key(&digest.to_string()))
                .map(|bytes| bytes.to_vec())
                .ok_or("no such manifest")
        })
        .expect("a listed base tree")
}

/// A writer over the rig's farm that refuses a list of more than `entries` entries.
fn limited(rig: &Rig, entries: u64) -> TreeWriter {
    let source = Arc::clone(&rig.farm) as Arc<dyn BlobSource>;
    let store = Arc::clone(&rig.farm) as Arc<dyn LargeStore>;
    let clock: kbf_front::Clock = Arc::new(|| UNIX_EPOCH + Duration::from_secs(START_SECS));
    TreeWriter::new(source, store, clock).with_list_entries(entries)
}

/// Writes the list of the Action over `root` over the rig's listed base.
pub(super) async fn list(rig: &Rig, root: &Digest) -> Result<Listing, NoTree> {
    let kept = rig.kept(root).await;
    rig.writer.write_listing(&kept, &listed_base(rig)).await
}

/// A mode as the file modes of the list are written.
fn mode_name(mode: Mode) -> &'static str {
    match mode.bits() {
        0o644 => "0644",
        0o755 => "0755",
        _ => "other",
    }
}

/// Every entry of the tree `built` by path, described: `dir <mode>`, `symlink <target>` or
/// `file <mode> size=<n> src=<key>@<offset or -> sha=<hex>`.
fn describe(built: &BuiltTree) -> BTreeMap<String, String> {
    let records: BTreeMap<&[u8; 32], &TreeRecord> = built
        .records
        .iter()
        .map(|(digest, record)| (digest.as_bytes(), record))
        .collect();
    let sources: BTreeMap<&str, String> = built
        .files
        .iter()
        .filter_map(|file| {
            let source = file.source.as_ref()?;
            Some((
                file.path.as_str(),
                format!("{}@{}", source.key, source.offset),
            ))
        })
        .collect();
    let mut described = BTreeMap::new();
    let mut stack = vec![(String::new(), records[built.root.as_bytes()])];
    while let Some((at, record)) = stack.pop() {
        for child in record.children() {
            let path = if at.is_empty() {
                child.name().to_owned()
            } else {
                format!("{at}/{}", child.name())
            };
            let text = match child {
                RecordChild::Dir { digest, .. } => {
                    let below = records[digest.as_bytes()];
                    stack.push((path.clone(), below));
                    format!("dir {}", mode_name(below.mode()))
                }
                RecordChild::Symlink { target, .. } => format!("symlink {target}"),
                RecordChild::File {
                    mode, size, sha256, ..
                } => {
                    let source = sources.get(path.as_str()).map_or("-", String::as_str);
                    format!(
                        "file {} size={size} src={source} sha={sha256}",
                        mode_name(*mode)
                    )
                }
            };
            assert!(
                described.insert(path.clone(), text).is_none(),
                "{path} is listed twice"
            );
        }
    }
    described
}

/// The list stored at `key`, read by tierstore's version 2 reader as its mount provider reads it:
/// the tree it builds, and its ranges as the key of the pack, the first byte and the length (a
/// range names its key by its number in the list's keys).
fn built(rig: &Rig, key: &str) -> (BuiltTree, Vec<(String, u64, u64)>) {
    let bytes = rig.farm.object(key).expect("the list is stored");
    let list = TreeListV2::open(&bytes).expect("a list the reader opens");
    let built = list
        .build(&TreeLimits::default(), |_| {})
        .expect("a list the reader builds");
    let ranges = (list.ranges())
        .map(|range: ListRange| {
            let key =
                (list.keys().get(range.key as usize)).expect("a range names a key of the list");
            (key.to_string(), range.offset, range.len)
        })
        .collect();
    (built, ranges)
}

/// The list stored at `key`, described.
fn stored(rig: &Rig, key: &str) -> BTreeMap<String, String> {
    describe(&built(rig, key).0)
}

/// The list the writer stored for `listing`.
pub(super) fn read(rig: &Rig, listing: &Listing) -> BTreeMap<String, String> {
    stored(rig, listing.key())
}

/// A file line of the list.
pub(super) fn file_line(mode: &str, size: u64, src: &str, sha: &str) -> String {
    format!("file {mode} size={size} src={src} sha={sha}")
}

/// The paths the base tree of the rig lists.
const BASE_PATHS: [&str; 6] = [
    ".kbf-tools",
    ".kbf-tools/kbf-task",
    "README",
    "bin",
    "usr",
    "usr/cc",
];

/// An input root holding `files` as `f0`, `f1`, ...
async fn root_of(rig: &Rig, files: &[Digest]) -> Digest {
    let nodes = files
        .iter()
        .enumerate()
        .map(|(at, digest)| file(&format!("f{at}"), digest, false))
        .collect();
    rig.store(&directory(nodes, vec![], vec![])).await
}

/// Catches: a packed file listed without its pack key, offset, size or SHA-256, or with the
/// wrong mode: it names `cas/f/w/p0` at its offset with its size and hash, 0644, and an
/// executable file 0755.
#[tokio::test]
async fn a_packed_file_is_listed_with_its_span_size_mode_and_hash() {
    let rig = Rig::new().await;
    let plain = rig.farm.packed("int main;", "cas/f/w/p0", 17).await;
    let exec = rig.farm.packed("echo ran\n", "cas/f/w/p0", 40).await;
    let root = rig
        .store(&directory(
            vec![file("a.c", &plain, false), file("run.sh", &exec, true)],
            vec![],
            vec![],
        ))
        .await;
    let listing = list(&rig, &root).await.expect("a list");
    let got = read(&rig, &listing);
    assert_eq!(
        got.get("kbf-work/a.c"),
        Some(&file_line("0644", 9, "cas/f/w/p0@17", &plain.hash_hex()))
    );
    assert_eq!(
        got.get("kbf-work/run.sh"),
        Some(&file_line("0755", 9, "cas/f/w/p0@40", &exec.hash_hex()))
    );
    assert_eq!(rig.farm.puts(), [listing.key().to_owned()]);
}

/// Catches: the Action and Command listed as other than the files they are: `.kbf/action` and
/// `.kbf/command` name their pack spans, sizes and hashes, 0644, and `.kbf` is a 0755 directory.
#[tokio::test]
async fn the_action_and_command_are_listed_under_kbf() {
    let rig = Rig::new().await;
    let root = root_of(&rig, &[]).await;
    let action = rig.action(&root).await;
    let listing = list(&rig, &root).await.expect("a list");
    let got = read(&rig, &listing);
    assert_eq!(got.get(".kbf").map(String::as_str), Some("dir 0755"));
    assert_eq!(
        got.get(".kbf/action"),
        Some(&file_line(
            "0644",
            action.size_bytes,
            "cas/f/w/p9@100",
            &action.hash_hex()
        ))
    );
    assert_eq!(
        got.get(".kbf/command"),
        Some(&file_line(
            "0644",
            rig.command.size_bytes,
            "cas/f/w/p9@0",
            &rig.command.hash_hex()
        ))
    );
}

/// Catches: a file found only as a whole object listed with a pack key, a wrong offset, or
/// written again: it names `cas/l/<sha256>` at offset 0 and nothing but the list is written.
#[tokio::test]
async fn a_whole_object_file_is_listed_at_its_key_and_not_written_again() {
    let rig = Rig::new().await;
    let big = rig.farm.whole("a large object").await;
    let root = root_of(&rig, &[big]).await;
    let listing = list(&rig, &root).await.expect("a list");
    let key = whole_key(&big.hash_hex());
    assert_eq!(
        read(&rig, &listing).get("kbf-work/f0"),
        Some(&file_line("0644", 14, &format!("{key}@0"), &big.hash_hex()))
    );
    assert_eq!(rig.farm.puts(), [listing.key().to_owned()]);
}

/// Catches: a blob in no pack and no `cas/l/` object named by a list before it exists: it is
/// written whole to `cas/l/<sha256>` before the list, and the list names that object at offset 0.
#[tokio::test]
async fn an_unlocated_blob_is_written_whole_before_the_list_names_it() {
    let rig = Rig::new().await;
    let bytes = Bytes::from_static(b"only in a storage node's write tier");
    let blob = rig.farm.cas(bytes.clone()).await;
    let root = root_of(&rig, &[blob]).await;
    let listing = list(&rig, &root).await.expect("a list");
    let key = whole_key(&blob.hash_hex());
    assert_eq!(rig.farm.object(&key), Some(bytes));
    assert_eq!(rig.farm.puts(), [key.clone(), listing.key().to_owned()]);
    assert_eq!(
        read(&rig, &listing).get("kbf-work/f0"),
        Some(&file_line(
            "0644",
            blob.size_bytes,
            &format!("{key}@0"),
            &blob.hash_hex()
        ))
    );
}

/// Catches: an empty file given a source, another hash or a wrong size: it has size 0, names no
/// object and has the SHA-256 of no bytes (version 2's rule for a file of no bytes), and no object
/// is written for it.
#[tokio::test]
async fn an_empty_file_has_size_zero_and_no_source() {
    let rig = Rig::new().await;
    let empty = kbf_segments::sha256(&[]);
    let root = root_of(&rig, &[empty]).await;
    let listing = list(&rig, &root).await.expect("a list");
    assert_eq!(
        read(&rig, &listing).get("kbf-work/f0"),
        Some(&file_line("0644", 0, "-", EMPTY_HEX))
    );
    assert_eq!(rig.farm.puts(), [listing.key().to_owned()]);
}

/// Catches: a symlink or an empty subdirectory left out of the list, a link with another
/// target, or a directory of another mode than 0755: both are listed, and every directory of
/// the list (the base's, `work`, `.kbf` and the input's) is 0755.
#[tokio::test]
async fn a_symlink_and_an_empty_subdirectory_are_listed_and_every_directory_is_0755() {
    let rig = Rig::new().await;
    let empty_dir = rig.store(&directory(vec![], vec![], vec![])).await;
    let root = rig
        .store(&directory(
            vec![],
            vec![subdir("hollow", &empty_dir)],
            vec![link("l", "a.c")],
        ))
        .await;
    let listing = list(&rig, &root).await.expect("a list");
    let got = read(&rig, &listing);
    assert_eq!(
        got.get("kbf-work/l").map(String::as_str),
        Some("symlink a.c")
    );
    assert_eq!(
        got.get("kbf-work/hollow").map(String::as_str),
        Some("dir 0755")
    );
    assert!(!got.keys().any(|path| path.starts_with("kbf-work/hollow/")));
    let dirs: Vec<&str> = got
        .iter()
        .filter(|(_, text)| text.starts_with("dir "))
        .map(|(path, _)| path.as_str())
        .collect();
    assert_eq!(
        dirs,
        [".kbf", ".kbf-tools", "kbf-work", "kbf-work/hollow", "usr"],
        "every directory is listed"
    );
    assert!(
        got.iter()
            .filter(|(_, text)| text.starts_with("dir "))
            .all(|(_, text)| text == "dir 0755")
    );
}

/// Catches: an empty input root that lists entries below `kbf-work`, or drops the base,
/// `kbf-work` or `.kbf`: the list is exactly the base's six entries, `kbf-work`, `.kbf`, `.kbf/action` and
/// `.kbf/command`.
#[tokio::test]
async fn an_empty_input_root_lists_work_with_nothing_below_it() {
    let rig = Rig::new().await;
    let root = rig.store(&directory(vec![], vec![], vec![])).await;
    let listing = list(&rig, &root).await.expect("a list");
    let got = read(&rig, &listing);
    let mut expected: BTreeSet<String> = BASE_PATHS.iter().map(|p| (*p).to_owned()).collect();
    expected.extend(
        ["kbf-work", ".kbf", ".kbf/action", ".kbf/command"]
            .iter()
            .map(|p| (*p).to_owned()),
    );
    assert_eq!(got.keys().cloned().collect::<BTreeSet<_>>(), expected);
    assert_eq!(got.get("kbf-work").map(String::as_str), Some("dir 0755"));
}

/// Catches: the base tree's entries missing, at the wrong paths, or stripped of their source:
/// a nested base file, the base symlink and the base's directories are at their paths; the base
/// file with a source keeps it; one without is named by its whole object `cas/l/<sha256>`.
#[tokio::test]
async fn the_base_trees_entries_are_listed_at_their_paths() {
    let rig = Rig::new().await;
    let root = rig.store(&directory(vec![], vec![], vec![])).await;
    let listing = list(&rig, &root).await.expect("a list");
    let got = read(&rig, &listing);
    let whole = format!("{}@0", whole_key(BASE_FILE));
    let get = |path: &str| got.get(path).map(String::as_str);
    assert_eq!(get(".kbf-tools"), Some("dir 0755"));
    assert_eq!(get("usr"), Some("dir 0755"));
    assert_eq!(get("bin"), Some("symlink usr/bin"));
    assert_eq!(
        get("README"),
        Some(file_line("0644", 5, "cas/f/base/p0@3", BASE_FILE).as_str())
    );
    assert_eq!(
        get("usr/cc"),
        Some(file_line("0755", 5, &whole, BASE_FILE).as_str())
    );
    assert_eq!(
        get(".kbf-tools/kbf-task"),
        Some(file_line("0755", 5, &whole, BASE_FILE).as_str())
    );
    assert_eq!(
        rig.farm.puts(),
        [listing.key().to_owned()],
        "only the list is written"
    );
}

/// Catches: a list stored at another key than `cas/l/<sha256 of its bytes>`, one the reader does
/// not take, a second write of the same inputs that gives another key or fails on the taken key,
/// or inputs that differ given one key.
#[tokio::test]
async fn the_list_is_stored_at_the_hash_of_its_bytes_and_a_taken_key_is_success() {
    let rig = Rig::new().await;
    let blob = rig.farm.packed("one", "cas/f/w/p0", 0).await;
    let root = root_of(&rig, &[blob]).await;
    let first = list(&rig, &root).await.expect("a list");
    let bytes = rig.farm.object(first.key()).expect("stored");
    assert_eq!(first.key(), format!("cas/l/{}", TreeDigest::of(&bytes)));
    // The key is the one tierstore's provider names the list by.
    assert_eq!(first.key(), tierstore::tree_list_key(&bytes));
    assert!(!read(&rig, &first).is_empty());
    rig.farm.clear_records();
    let second = list(&rig, &root).await.expect("a list again");
    assert_eq!(second, first);
    assert_eq!(
        rig.farm.puts(),
        [first.key().to_owned()],
        "written once more"
    );
    // The key is taken only after the write began.
    rig.farm.state().objects.remove(first.key());
    rig.farm.state().late.insert(first.key().to_owned());
    let third = list(&rig, &root).await.expect("a taken key is success");
    assert_eq!(third, first);
    let other = rig.farm.packed("two", "cas/f/w/p0", 9).await;
    let other_root = root_of(&rig, &[other]).await;
    let different = list(&rig, &other_root).await.expect("a list");
    assert_ne!(different.key(), first.key());
}

/// Catches: a directory named at two paths listed once: both `work/a` and `work/b` and the file
/// below each are listed.
#[tokio::test]
async fn a_directory_at_two_paths_is_listed_at_both() {
    let rig = Rig::new().await;
    let x = rig.farm.packed("#define X", "cas/f/w/p1", 0).await;
    let sub = rig
        .store(&directory(vec![file("x.h", &x, false)], vec![], vec![]))
        .await;
    let root = rig
        .store(&directory(
            vec![],
            vec![subdir("a", &sub), subdir("b", &sub)],
            vec![],
        ))
        .await;
    let listing = list(&rig, &root).await.expect("a list");
    let got = read(&rig, &listing);
    let line = file_line("0644", 9, "cas/f/w/p1@0", &x.hash_hex());
    for dir in ["kbf-work/a", "kbf-work/b"] {
        assert_eq!(got.get(dir).map(String::as_str), Some("dir 0755"), "{dir}");
        assert_eq!(got.get(&format!("{dir}/x.h")), Some(&line), "{dir}/x.h");
    }
}

/// Catches: `.kbf-out` at the input root's top given a list: `NoTree::ReservedInput`, unrepresentable,
/// and nothing is written.
#[tokio::test]
async fn kbf_out_at_the_top_of_the_input_root_is_refused() {
    let rig = Rig::new().await;
    let blob = rig.farm.cas("out").await;
    let root = rig
        .store(&directory(
            vec![file(".kbf-out", &blob, false)],
            vec![],
            vec![],
        ))
        .await;
    let refused = list(&rig, &root).await.expect_err("no list");
    assert_eq!(
        refused,
        NoTree::ReservedInput {
            name: ".kbf-out".to_owned()
        }
    );
    assert_eq!(refused.why(), "unrepresentable");
    assert!(rig.farm.puts().is_empty());
}

/// Catches: a Directory with `node_properties` given a list that cannot carry them:
/// `NoTree::NodeProperties` naming the Directory, unrepresentable, and nothing is written.
#[tokio::test]
async fn node_properties_make_no_list() {
    let rig = Rig::new().await;
    let set = Directory {
        node_properties: Some(reapi::NodeProperties {
            unix_mode: Some(0o600),
            ..reapi::NodeProperties::default()
        }),
        ..directory(vec![], vec![], vec![link("l", "t")])
    };
    let root = rig.store(&set).await;
    let refused = list(&rig, &root).await.expect_err("no list");
    assert_eq!(
        refused,
        NoTree::NodeProperties {
            directory: root,
            node: String::new()
        }
    );
    assert_eq!(refused.why(), "unrepresentable");
    assert!(rig.farm.puts().is_empty());
}

/// Catches: a base tree published without its entries listed given a list that omits them:
/// `NoTree::BaseNotListed`, `why=store`, nothing asked of the index and nothing written.
#[tokio::test]
async fn a_base_without_a_listing_makes_no_list() {
    let rig = Rig::new().await;
    let root = root_of(&rig, &[]).await;
    let kept = rig.kept(&root).await;
    rig.farm.clear_records();
    let refused = rig
        .writer
        .write_listing(&kept, &rig.base.tree)
        .await
        .expect_err("no list");
    assert_eq!(refused, NoTree::BaseNotListed);
    assert_eq!(refused.why(), "store");
    assert!(rig.farm.puts().is_empty());
    assert!(rig.farm.asked().is_empty());
}

/// Catches: the list-length limit off by one or enforced after writing: a list of exactly
/// `n` entries is stored, and one entry more is `NoTree::ListTooLong` naming the entries and
/// the limit, unrepresentable, with nothing written.
#[tokio::test]
async fn a_list_of_the_limit_is_stored_and_one_more_is_refused() {
    let rig = Rig::new().await;
    let a = rig.farm.packed("aaa", "cas/f/w/p0", 0).await;
    let b = rig.farm.packed("bbb", "cas/f/w/p0", 3).await;
    let root = root_of(&rig, &[a, b]).await;
    let kept = rig.kept(&root).await;
    // The base's six, `work`, `.kbf`, its two files, and the input's two.
    let exact = 12;
    let base = listed_base(&rig);
    let refused = limited(&rig, exact - 1)
        .write_listing(&kept, &base)
        .await
        .expect_err("one over");
    assert_eq!(
        refused,
        NoTree::ListTooLong {
            entries: exact,
            limit: exact - 1
        }
    );
    assert_eq!(refused.why(), "unrepresentable");
    assert!(rig.farm.puts().is_empty());
    let listing = limited(&rig, exact)
        .write_listing(&kept, &base)
        .await
        .expect("exactly the limit");
    assert_eq!(read(&rig, &listing).len() as u64, exact);
}

/// Catches: a failed pack index or presence check taken for a list, or a list stored after it:
/// `NoTree::Lookup`, `why=store`, and nothing is written.
#[tokio::test]
async fn a_failed_lookup_makes_no_list() {
    for index in [true, false] {
        let rig = Rig::new().await;
        let blob = rig.farm.cas("unpacked").await;
        {
            let mut state = rig.farm.state();
            if index {
                state.faults.index = Fault::Fail;
            } else {
                state.faults.head = Fault::Fail;
            }
        }
        let root = root_of(&rig, &[blob]).await;
        let refused = list(&rig, &root).await.expect_err("no list");
        assert!(matches!(refused, NoTree::Lookup { .. }), "{refused:?}");
        assert_eq!(refused.why(), "store");
        assert!(rig.farm.puts().is_empty());
    }
}

/// Catches: a writer that waits for ever on an index or a presence check that never answers:
/// at the lookups' deadline (tokio's paused clock) it is `NoTree::Deadline`, `why=store`, and
/// nothing is written.
#[tokio::test(start_paused = true)]
async fn lookups_that_never_answer_make_no_list() {
    for index in [true, false] {
        let rig = Rig::new().await;
        let blob = rig.farm.cas("unpacked").await;
        {
            let mut state = rig.farm.state();
            if index {
                state.faults.index = Fault::Hang;
            } else {
                state.faults.head = Fault::Hang;
            }
        }
        let root = root_of(&rig, &[blob]).await;
        let refused = list(&rig, &root).await.expect_err("no list");
        assert_eq!(refused, NoTree::Deadline);
        assert_eq!(refused.why(), "store");
        assert!(rig.farm.puts().is_empty());
    }
}

/// Catches: a refused write of the list's key taken for stored: `NoTree::Put` naming the list's
/// key, `why=store`, no object under that key, and the blobs written before it stay written.
#[tokio::test]
async fn a_refused_write_of_the_list_is_put() {
    let rig = Rig::new().await;
    let blob = rig.farm.cas("only in the write tier").await;
    let root = root_of(&rig, &[blob]).await;
    let key = list(&rig, &root).await.expect("a list").key().to_owned();
    // The same inputs again, the list's key refused and nothing stored under it.
    rig.farm.state().objects.remove(&key);
    rig.farm.state().faults.put = Some(key.clone());
    let refused = list(&rig, &root).await.expect_err("no list");
    assert!(
        matches!(&refused, NoTree::Put { key: named, .. } if *named == key),
        "{refused:?}"
    );
    assert_eq!(refused.why(), "store");
    assert_eq!(rig.farm.object(&key), None);
    assert!(rig.farm.object(&whole_key(&blob.hash_hex())).is_some());
}

/// Catches: a name tierstore's tree-list reader refuses (`..`, a name given twice) stored in a
/// list anyway: `NoTree::ListingRefused`, `why=unrepresentable`, and nothing
/// is written, not even the blob that is nowhere yet.
#[tokio::test]
async fn a_name_the_lists_reader_refuses_makes_no_list() {
    let rig = Rig::new().await;
    let blob = rig.farm.cas("only in the write tier").await;
    let leaf = rig
        .store(&directory(vec![], vec![], vec![link("x", "t")]))
        .await;
    let cases = [
        directory(vec![], vec![], vec![link("..", "t")]),
        directory(
            vec![file("x", &blob, false)],
            vec![subdir("x", &leaf)],
            vec![],
        ),
    ];
    for message in cases {
        let root = rig.store(&message).await;
        let refused = list(&rig, &root).await.expect_err("no list");
        assert!(
            matches!(refused, NoTree::ListingRefused { .. }),
            "{refused:?}"
        );
        assert_eq!(refused.why(), "unrepresentable");
        assert!(rig.farm.puts().is_empty(), "nothing is written");
    }
}

/// Catches: an input root that holds an empty file and an empty directory (one digest, the
/// SHA-256 of no bytes, for both) listed without the directory, or refused as a directory the
/// closure did not keep: both are listed, the file of no bytes and the directory 0755.
#[tokio::test]
async fn an_empty_file_beside_an_empty_directory_are_both_listed() {
    let rig = Rig::new().await;
    let empty = kbf_segments::sha256(&[]);
    let root = rig
        .store(&directory(
            vec![file("blank", &empty, false)],
            vec![subdir("hollow", &empty)],
            vec![],
        ))
        .await;
    let listing = list(&rig, &root).await.expect("a list");
    let got = read(&rig, &listing);
    assert_eq!(
        got.get("kbf-work/blank"),
        Some(&file_line("0644", 0, "-", EMPTY_HEX))
    );
    assert_eq!(
        got.get("kbf-work/hollow").map(String::as_str),
        Some("dir 0755")
    );
    assert!(!got.keys().any(|path| path.starts_with("kbf-work/hollow/")));
}

/// Catches: a ranges table that leaves out a pack's spans, merges spans of one pack further
/// apart than 64 KiB, merges two packs, or leaves out the base's or `.kbf`'s files: the files of
/// `cas/f/w/p0` exactly 64 KiB apart are one range and the one a byte further is another, the
/// whole object and the base's spans each have theirs, and the Action and Command (100 bytes
/// apart in `cas/f/w/p9`) are one.
#[tokio::test]
async fn the_ranges_table_holds_every_pack_span_merged_within_64_kib() {
    let rig = Rig::new().await;
    let gap = 64 * 1024;
    let a = rig.farm.packed("aaa", "cas/f/w/p0", 0).await;
    let b = rig.farm.packed("bbb", "cas/f/w/p0", 3 + gap).await;
    let c = rig.farm.packed("ccc", "cas/f/w/p0", 6 + 2 * gap + 1).await;
    let big = rig.farm.whole("a large object").await;
    let root = root_of(&rig, &[a, b, c, big]).await;
    let listing = list(&rig, &root).await.expect("a list");
    let (_, ranges) = built(&rig, listing.key());
    let of = |key: &str| -> Vec<(u64, u64)> {
        (ranges.iter())
            .filter(|(of, _, _)| of == key)
            .map(|&(_, offset, len)| (offset, len))
            .collect()
    };
    assert_eq!(of("cas/f/w/p0"), [(0, 6 + gap), (6 + 2 * gap + 1, 3)]);
    assert_eq!(of(&whole_key(&big.hash_hex())), [(0, 14)]);
    assert_eq!(of("cas/f/base/p0"), [(3, 5)]);
    // `usr/cc` and `.kbf-tools/kbf-task` are one whole object.
    assert_eq!(of(&whole_key(BASE_FILE)), [(0, 5)]);
    let action = rig.action(&root).await;
    assert_eq!(of("cas/f/w/p9"), [(0, 100 + action.size_bytes)]);
    assert_eq!(ranges.len(), 6);
}

/// Catches: a pack key the reader refuses (over its 1024 bytes) stored in a list anyway:
/// `NoTree::ListingRefused` naming the key's fault, unrepresentable, and nothing is written.
#[tokio::test]
async fn a_pack_key_the_reader_refuses_makes_no_list() {
    let rig = Rig::new().await;
    let long = format!("cas/f/{}", "k".repeat(1100));
    let blob = rig.farm.packed("far away", &long, 0).await;
    let root = root_of(&rig, &[blob]).await;
    let refused = list(&rig, &root).await.expect_err("no list");
    assert!(
        matches!(&refused, NoTree::ListingRefused { why } if why.contains("key")),
        "{refused:?}"
    );
    assert_eq!(refused.why(), "unrepresentable");
    assert!(rig.farm.puts().is_empty());
}

/// Catches: a name longer than a record holds (255 bytes) cut short or wrapped: a file named by
/// 256 bytes is `NoTree::ListingRefused`, unrepresentable, and nothing is written; one of 255 is
/// listed at its path.
#[tokio::test]
async fn a_name_over_255_bytes_makes_no_list_and_one_of_255_is_listed() {
    let rig = Rig::new().await;
    let blob = rig.farm.packed("named", "cas/f/w/p0", 0).await;
    let over = rig
        .store(&directory(
            vec![file(&"n".repeat(256), &blob, false)],
            vec![],
            vec![],
        ))
        .await;
    let refused = list(&rig, &over).await.expect_err("no list");
    assert!(
        matches!(&refused, NoTree::ListingRefused { why } if why.contains("256 bytes")),
        "{refused:?}"
    );
    assert_eq!(refused.why(), "unrepresentable");
    assert!(rig.farm.puts().is_empty());
    let longest = "n".repeat(255);
    let fits = rig
        .store(&directory(
            vec![file(&longest, &blob, false)],
            vec![],
            vec![],
        ))
        .await;
    let listing = list(&rig, &fits).await.expect("a list");
    assert!(read(&rig, &listing).contains_key(&format!("kbf-work/{longest}")));
}

/// Catches: a tree whose list could not fit in the 256 MiB tierstore reads laid out (and held in
/// memory) before it is refused, or refused at the writer's own limit when that is the larger: an
/// input root of 64 directories, each of the same 64, four levels deep (33,820,736 entries from
/// five Directory messages) is `NoTree::ListTooLong` against the 4,194,302 entries that fit, with
/// nothing asked of the index and nothing written.
#[tokio::test]
async fn a_tree_too_large_for_any_list_is_refused_before_anything_is_looked_up() {
    let rig = Rig::new().await;
    let blob = rig.farm.packed("leaf", "cas/f/w/p0", 0).await;
    let mut level = rig
        .store(&directory(vec![file("leaf", &blob, false)], vec![], vec![]))
        .await;
    for _ in 0..4 {
        let fan = (0..64)
            .map(|at| subdir(&format!("d{at}"), &level))
            .collect();
        level = rig.store(&directory(vec![], fan, vec![])).await;
    }
    let kept = rig.kept(&level).await;
    rig.farm.clear_records();
    let refused = rig
        .writer
        .write_listing(&kept, &listed_base(&rig))
        .await
        .expect_err("no list");
    // 64 + 64^2 + 64^3 + 64^4 directories, a leaf in each of the 64^4, the base's six and four.
    let entries = 64 + 4096 + 262_144 + 16_777_216 + 16_777_216 + 6 + 4;
    // (256 MiB - the 64-byte header) / 64 bytes an entry, less the root.
    assert_eq!(
        refused,
        NoTree::ListTooLong {
            entries,
            limit: 4_194_302
        }
    );
    assert_eq!(refused.why(), "unrepresentable");
    assert!(rig.farm.asked().is_empty());
    assert!(rig.farm.puts().is_empty());
}

/// Catches: a list that makes another tree than the action tree's manifests name (a name, mode,
/// size, digest or link target changed, an entry left out or added, a directory at two paths
/// listed once): the tree tierstore builds from the list has the digest of the tree it builds
/// from the manifests [`TreeWriter::write`] stores for the same Action, flattened.
#[tokio::test]
async fn the_list_makes_the_tree_the_manifests_of_the_same_action_name() {
    let rig = Rig::new().await;
    let x = rig.farm.packed("#define X", "cas/f/w/p1", 0).await;
    let run = rig.farm.packed("echo ran\n", "cas/f/w/p1", 9).await;
    let big = rig.farm.whole("a large object").await;
    let empty = kbf_segments::sha256(&[]);
    let hollow = rig.store(&directory(vec![], vec![], vec![])).await;
    let sub = rig
        .store(&directory(
            vec![file("x.h", &x, false), file("blank", &empty, false)],
            vec![subdir("hollow", &hollow)],
            vec![link("up", "..")],
        ))
        .await;
    let root = rig
        .store(&directory(
            vec![file("run.sh", &run, true), file("big", &big, false)],
            vec![subdir("a", &sub), subdir("b", &sub)],
            vec![link("l", "a/x.h")],
        ))
        .await;
    let listing = list(&rig, &root).await.expect("a list");
    let (from_list, _) = built(&rig, listing.key());
    let tree = rig.write(&root).await.expect("the manifests");
    let flattened = rig.farm.flatten(&tree).expect("flattened");
    let mut paths: Vec<String> = Vec::new();
    let mut entries = Vec::new();
    for entry in &flattened {
        let path = match entry.parent.map(|parent| paths[parent as usize].as_str()) {
            None => String::new(),
            Some("") => entry.name.to_string(),
            Some(parent) => format!("{parent}/{}", entry.name),
        };
        paths.push(path.clone());
        if entry.parent.is_none() {
            continue;
        }
        let kind = match &entry.kind {
            tierstore::EntryKind::Dir { .. } => tierstore::ListKind::Dir { mode: Mode::DIR },
            tierstore::EntryKind::File {
                digest,
                size,
                exec,
                source,
            } => tierstore::ListKind::File {
                mode: if *exec { Mode::DIR } else { Mode::FILE },
                size: *size,
                source: (*size > 0).then(|| {
                    source.clone().unwrap_or_else(|| tierstore::BlobSource {
                        key: whole_key(&digest.to_string()).into(),
                        offset: 0,
                    })
                }),
                sha256: Some(*digest),
            },
            tierstore::EntryKind::Symlink { target } => tierstore::ListKind::Symlink {
                target: target.to_string(),
            },
        };
        entries.push(tierstore::ListEntry { path, kind });
    }
    let new_entries = tierstore::TreeList { entries }
        .into_entries()
        .expect("the manifests' entries");
    let from_manifests =
        tierstore::build_tree(None, new_entries, &TreeLimits::default()).expect("a tree");
    // The root's digest covers every name, mode, size, digest and target below it.
    assert_eq!(from_list.root, from_manifests.root);
}

/// Catches: a base file of no bytes given a source (its whole object, which does not exist) or
/// another digest when the base is listed: it is listed with size 0, no object and the SHA-256
/// of no bytes, beside the input root.
#[tokio::test]
async fn an_empty_file_of_the_base_names_no_object() {
    let rig = Rig::new().await;
    let root = format!(
        r#"{{"v":1,"children":[{{"name":"empty","kind":"file","sha256":"{EMPTY_HEX}","size":0}}]}}"#
    );
    let facts = BaseFacts {
        entries: 1,
        depth: 1,
    };
    let base = BaseTree::new(root.as_bytes(), facts)
        .expect("a base tree")
        .listed(|_: &TreeDigest| Ok::<Vec<u8>, &str>(root.clone().into_bytes()))
        .expect("listed");
    let input = root_of(&rig, &[]).await;
    let kept = rig.kept(&input).await;
    let listing = rig
        .writer
        .write_listing(&kept, &base)
        .await
        .expect("a list");
    let got = read(&rig, &listing);
    assert_eq!(
        got.get("empty"),
        Some(&file_line("0644", 0, "-", EMPTY_HEX))
    );
    assert_eq!(got.get("kbf-work").map(String::as_str), Some("dir 0755"));
}
