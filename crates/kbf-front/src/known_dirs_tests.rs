//! [`KnownDirs::uploaded`]: which uploaded blobs are decoded as Directories, that one is cached
//! as a walk that read it would have cached it, and that nothing else is.

use kbf_proto::reapi::{Directory, DirectoryNode, FileNode};
use prost::Message as _;

use super::*;
use crate::dir_cache::DirCache;
use crate::input_tree::Decoded;
use crate::prepare_run::DIRECTORY_MEMORY_BYTES;

fn sha(bytes: &[u8]) -> Digest {
    kbf_segments::sha256(bytes)
}

/// `message` encoded: its digest and bytes.
fn encoded(message: &Directory) -> (Digest, Bytes) {
    let bytes = Bytes::from(message.encode_to_vec());
    (sha(&bytes), bytes)
}

/// A Directory naming the files `files` and the Directories `dirs`, each by its digest in hex.
fn dir(files: &[Digest], dirs: &[Digest]) -> (Digest, Bytes) {
    encoded(&Directory {
        files: files
            .iter()
            .map(|d| FileNode {
                name: d.hash_hex(),
                digest: Some(crate::digest_to_proto(d)),
                ..FileNode::default()
            })
            .collect(),
        directories: dirs
            .iter()
            .map(|d| DirectoryNode {
                name: d.hash_hex(),
                digest: Some(crate::digest_to_proto(d)),
            })
            .collect(),
        ..Directory::default()
    })
}

/// What `known` has cached for `digest`, if anything (a hit counts as a use).
fn cached(known: &KnownDirs, digest: &Digest) -> Option<Arc<Decoded>> {
    match known.dirs.lookup(digest) {
        Lookup::Cached(decoded) => Some(decoded),
        _ => None,
    }
}

/// Catches an uploaded Directory cached as anything but what a walk that read it would cache
/// (its files and Directories, by digest), or not kept with its names for the preparation: the
/// Directory is cached, decoded as `decode` decodes it, and the preparation's cache holds it with
/// the names its blob gives.
#[test]
fn an_uploaded_directory_is_cached_and_kept_with_its_names() {
    let (a, b) = (sha(b"file a"), sha(b"file b"));
    let sub = dir(&[b], &[]);
    let root = dir(&[a], &[sub.0]);
    let known = KnownDirs::default();
    let keep = Arc::new(DirectoryCache::in_memory(DIRECTORY_MEMORY_BYTES));
    known.keep_into(Arc::clone(&keep));

    assert!(known.uploaded(&root.0, &root.1));

    let decoded = cached(&known, &root.0).expect("cached");
    assert_eq!((&*decoded.files, &*decoded.dirs), (&[a][..], &[sub.0][..]));
    let kept = keep.recent(&root.0).expect("kept with its names");
    let parsed = KeptDirectory::parse(&root.0, root.1.clone()).expect("parses");
    assert_eq!(kept, parsed.kept);
    assert!(
        cached(&known, &sub.0).is_none(),
        "nothing else was uploaded"
    );
}

/// Catches the bound on what is decoded moved or dropped: a blob of exactly
/// [`UPLOADED_DIRECTORY_BYTES`] that starts with a Directory's key may be one, a byte more may
/// not; the empty blob is never one; and of the first bytes, only the keys of a Directory's four
/// length-delimited fields are.
#[test]
fn only_a_small_blob_that_starts_as_a_directory_may_be_one() {
    let some = sha(b"some blob");
    let at_bound = vec![0x0a; UPLOADED_DIRECTORY_BYTES];
    let over = vec![0x0a; UPLOADED_DIRECTORY_BYTES + 1];
    assert!(may_be_directory(&some, &at_bound));
    assert!(!may_be_directory(&some, &over));
    assert!(!may_be_directory(&sha(b""), b""), "the empty blob");
    assert!(!may_be_directory(&some, b""), "no first byte");
    for key in [0x0a, 0x12, 0x1a, 0x2a] {
        assert!(may_be_directory(&some, &[key, 0]), "{key:#x}");
    }
    for other in [0x08, 0x22, 0x32, b'#', b'h', 0xff] {
        assert!(!may_be_directory(&some, &[other, 0]), "{other:#x}");
    }
}

/// Catches a blob that is not a Directory cached, or a failed decode that leaves its digest
/// marked as being read (every walk that reaches it would then wait on a read nobody makes):
/// text, bytes that start like a Directory and do not decode, and a Directory naming a digest
/// that is not one are each left uncached, and the next lookup of each leads a read of its own.
#[test]
fn an_upload_that_is_not_a_directory_caches_nothing_and_leaves_no_read_waiting() {
    let broken_node = encoded(&Directory {
        files: vec![FileNode {
            name: "f".to_owned(),
            digest: Some(kbf_proto::reapi::Digest {
                hash: "not hex".to_owned(),
                size_bytes: 1,
            }),
            ..FileNode::default()
        }],
        ..Directory::default()
    });
    let truncated = Bytes::from_static(&[0x0a, 0x05, 0x0a]);
    let text = Bytes::from_static(b"#include <stdio.h>\n");
    let known = KnownDirs::default();

    for (digest, bytes) in [
        broken_node,
        (sha(&truncated), truncated),
        (sha(&text), text),
    ] {
        assert!(!known.uploaded(&digest, &bytes), "{digest}");
        assert!(
            matches!(known.dirs.lookup(&digest), Lookup::Lead(_)),
            "{digest} is cached or left being read"
        );
    }
    assert_eq!(known.dirs.len(), 0);
}

/// Catches an upload that replaces a Directory a walk is reading, or that a walk has cached
/// (the read ends with these same bytes, and the cached entry is the same content): neither is
/// cached anew, and the read in flight finishes as it would have.
#[test]
fn a_directory_cached_or_being_read_is_left_as_it_is() {
    let reading = dir(&[sha(b"read")], &[]);
    let known = KnownDirs::default();
    let Lookup::Lead(lead) = known.dirs.lookup(&reading.0) else {
        panic!("nobody reads it yet");
    };

    assert!(!known.uploaded(&reading.0, &reading.1), "being read");
    assert_eq!(known.dirs.len(), 0);
    lead.finish(Arc::new(decode(&reading.0, &reading.1).expect("decodes")));
    assert!(!known.uploaded(&reading.0, &reading.1), "cached");
    assert_eq!(known.dirs.len(), 1);
}

/// Catches what uploads that decode as Directories (whether or not they are) put in the cache
/// left unbounded: a hundred of them, together far over the bound, leave the cache within it.
#[test]
fn uploads_that_decode_as_directories_stay_within_the_cache_bound() {
    const BOUND: u64 = 2_000;
    let known = KnownDirs::new(DirCache::new(BOUND));

    let mut taken = 0;
    for n in 0..100 {
        let files: Vec<Digest> = (0..4).map(|f| sha(format!("{n}/{f}").as_bytes())).collect();
        let looks_like_one = dir(&files, &[]);
        taken += u64::from(known.uploaded(&looks_like_one.0, &looks_like_one.1));
    }

    assert_eq!(taken, 100);
    assert!(known.dirs.bytes() <= BOUND, "{} bytes", known.dirs.bytes());
    assert!(known.dirs.len() < 100);
}
