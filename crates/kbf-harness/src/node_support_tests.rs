//! What a farm's tests read off a node's disk and the large blob they write.

use super::*;

#[test]
fn the_large_input_is_longer_than_every_chunk_and_the_same_each_time() {
    // Catches: an input a chunk or a copy buffer holds whole (the read path's chunking would
    // go untested), and one that changes between calls (a blob written and read back under
    // two contents).
    let bytes = input_bytes();
    assert!(bytes.len() > 3 << 20, "{}", bytes.len());
    assert_eq!(bytes, input_bytes());
    let runs: std::collections::BTreeSet<&[u8]> = bytes.chunks(64).collect();
    assert_eq!(runs.len(), bytes.len().div_ceil(64));
}

#[test]
fn files_holding_finds_each_inode_that_holds_the_bytes_whole() {
    // Catches: a hot copy missed (the test then proves a blob is not stored twice when it
    // is), one inode counted once per name (a hard link is no second copy), a file that holds
    // only the start of the bytes (or all of it but the end) taken for a copy, and a path that is not there ending the
    // search. Catches: the missing path `unwrap`ped; `any` on the anchor alone.
    let d = crate::scratch("files-holding");
    let bytes: Vec<u8> = (0..200u8).collect();
    let (dir, other) = (d.join("a/b"), d.join("c"));
    std::fs::create_dir_all(&dir).expect("dirs");
    std::fs::create_dir_all(&other).expect("dirs");
    std::fs::write(dir.join("whole"), [b"x".as_slice(), &bytes, b"y"].concat()).expect("write");
    std::fs::hard_link(dir.join("whole"), other.join("link")).expect("link");
    std::fs::write(other.join("start"), &bytes[..100]).expect("write");
    std::fs::write(other.join("short"), b"short").expect("write");
    // Long enough, with the anchor (the first 64 bytes) but not the rest: not a copy.
    let torn = [&bytes[..64], &[0u8; 200][..]].concat();
    std::fs::write(other.join("torn"), torn).expect("write");
    std::fs::write(other.join("zeros"), [0u8; 300]).expect("write");
    let found = files_holding(&[d.clone(), d.join("not-there")], &bytes);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].1, 202);
    assert!(files_holding(&[d.join("not-there")], &bytes).is_empty());
}
