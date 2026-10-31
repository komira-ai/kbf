//! A cache that does not hold the lock of its directory: what it serves, and that it writes
//! nothing to the directory; and the cache that takes the lock after it was free.
//!
//! In these tests `Rig::cache`, which takes no lock, stands for the process that holds the
//! directory and writes to it (it fills, and leaves what a fill that is not finished leaves),
//! and `Rig::locked` makes the caches whose mode the lock decides. The listing of the whole
//! directory (`testing::tree`: names, sizes, modes, inodes and times) before and after is the
//! proof that nothing was written.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::Notify;

use super::layout::{Sidecar, data_path, meta_path, stage_dir};
use super::metrics::read;
use super::testing::{Rig, digest, pattern, tree};
use super::{Mode, Tick};
use crate::{Backend, ByteRange, Error, Op};

/// A 64-digit name that starts with `first`.
fn hex_of(first: &str) -> String {
    format!("{first}{}", "0".repeat(64 - first.len()))
}

/// Writes `bytes` to `path`, making the directory.
fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// A sidecar that says `size` bytes.
fn sidecar(size: u64) -> Vec<u8> {
    let sidecar = Sidecar {
        key: "cas/b/x".to_owned(),
        size,
        sha256: digest(b"x"),
        tag: "tag".to_owned(),
        modified: 0,
    };
    serde_json::to_vec(&sidecar).unwrap()
}

/// What the process that holds the directory leaves in it while it works: a sidecar whose data
/// file is not linked yet, a data file with no sidecar, and a fill in `.stage/`.
fn leave_the_middle_of_a_publish_and_a_fill(root: &Path) {
    write(&meta_path(root, &hex_of("aabb")), &sidecar(4));
    write(&data_path(root, &hex_of("ccdd")), b"no sidecar");
    write(&stage_dir(root).join("0000000000000009"), b"half a fill");
}

/// Asserts that a cache that opens beside the holder reads the directory and changes nothing in
/// it: the sidecar whose data file is not linked yet, the data file with no sidecar and the file
/// in `.stage/` are all still there, nothing is counted as discarded, and the index holds the
/// object that is whole. It would catch an open that cleans up under the process that holds the
/// directory: a fill of the holder lost, a publish of the holder undone.
#[tokio::test]
async fn a_read_only_open_deletes_and_empties_nothing() {
    let rig = Rig::new("tscache-ro-open");
    rig.put("cas/b/k1", &pattern(30)).await;
    let _ = rig.cache.get("cas/b/k1", None).await.unwrap();
    let holder = rig.locked();
    let root = &rig.config.path;
    leave_the_middle_of_a_publish_and_a_fill(root);
    let before = tree(root);
    let reader = rig.locked();
    assert_eq!(tree(root), before);
    assert_eq!(reader.counters().discarded_at_start(), 0);
    assert_eq!(reader.held(), (30, 1));
    assert_eq!(reader.mode(), Mode::ReadOnly);
    assert_eq!(holder.mode(), Mode::ReadWrite);
}

/// Asserts that a read-only cache serves a hit from the directory; sends a miss, a ranged miss
/// (with no size HEAD first: it keeps nothing, so the size is of no use) and a HEAD of an object
/// it does not hold to the upstream; sends a write-through put and delete to the upstream and
/// forgets a deleted object in its index; and changes nothing in the directory in any of it. It
/// would catch a read-only cache that fills, keeps, or removes the files of the holder.
#[tokio::test]
async fn a_read_only_cache_serves_hits_and_sends_the_rest_upstream_and_writes_nothing() {
    let rig = Rig::writing("tscache-ro-serve");
    for key in ["cas/b/hit", "cas/b/miss", "cas/b/ranged"] {
        rig.put(key, &pattern(30)).await;
    }
    let _ = rig.cache.get("cas/b/hit", None).await.unwrap();
    let holder = rig.locked();
    let reader = rig.locked();
    let root = &rig.config.path;
    let before = tree(root);

    assert_eq!(reader.get("cas/b/hit", None).await.unwrap(), pattern(30));
    assert_eq!(reader.get("cas/b/miss", None).await.unwrap(), pattern(30));
    let heads = rig.calls(Op::Head);
    let range = Some(ByteRange::new(5, 10));
    let got = reader.get("cas/b/ranged", range).await.unwrap();
    assert_eq!(got, pattern(30)[5..15]);
    assert_eq!(rig.calls(Op::Head), heads);
    assert_eq!(reader.head("cas/b/hit").await.unwrap().size, 30);
    assert_eq!(reader.head("cas/b/miss").await.unwrap().size, 30);

    reader
        .put("cas/b/new", Bytes::from_static(b"new"))
        .await
        .unwrap();
    assert_eq!(rig.tamper.get("cas/b/new", None).await.unwrap(), "new");
    reader.delete("cas/b/hit").await.unwrap();
    assert!(rig.tamper.get("cas/b/hit", None).await.is_err());
    // The cache forgot the object it was told is deleted, so a read is a miss and finds it gone.
    let gone = reader.get("cas/b/hit", None).await;
    assert!(matches!(gone, Err(Error::NotFound(_))), "{gone:?}");

    assert_eq!(tree(root), before);
    let counters = reader.counters();
    assert_eq!(read(&counters.hits), 1);
    assert_eq!(read(&counters.misses), 3);
    assert_eq!(read(&counters.fills), 0);
    assert_eq!(read(&counters.staging_files), 0);
    assert_eq!(reader.mode(), Mode::ReadOnly);
    assert_eq!(holder.mode(), Mode::ReadWrite);
}

/// Asserts that an object the holder evicted since the scan is a miss, answered from the upstream,
/// and that the entry stays in the index and every file the holder left is where it is: a read
/// error is not a reason to delete in a directory that is not ours (a read-write cache drops the
/// entry and the files). It would catch a read-only cache that deletes what the holder may have
/// just filled again.
#[tokio::test]
async fn a_read_error_is_a_miss_in_a_read_only_cache_and_drops_nothing() {
    let rig = Rig::new("tscache-ro-gone");
    rig.put("cas/b/k1", &pattern(30)).await;
    let _ = rig.cache.get("cas/b/k1", None).await.unwrap();
    let holder = rig.locked();
    let reader = rig.locked();
    // The holder evicts it behind the scan of the reader.
    std::fs::remove_file(rig.file("cas/b/k1")).unwrap();
    let root = &rig.config.path;
    let before = tree(root);
    assert_eq!(reader.get("cas/b/k1", None).await.unwrap(), pattern(30));
    assert_eq!(tree(root), before);
    assert_eq!(reader.held(), (30, 1));
    assert_eq!(read(&reader.counters().hits), 0);
    assert_eq!(read(&reader.counters().misses), 1);
    assert_eq!(reader.mode(), Mode::ReadOnly);
    assert_eq!(holder.mode(), Mode::ReadWrite);
}

/// Asserts that a read-only cache over a budget it exceeds evicts nothing, and that neither a
/// pass nor the loop that runs the passes, halted, stamps a file time: the passes and the
/// stamps are the holder's. It would catch a read-only cache that evicts the objects of the
/// holder or writes the record of use into its files.
#[tokio::test]
async fn a_read_only_cache_does_not_evict_and_does_not_stamp_file_times() {
    let rig = Rig::new("tscache-ro-evict");
    for n in 0..3 {
        let key = format!("cas/b/k{n}");
        rig.put(&key, &pattern(30)).await;
        let _ = rig.cache.get(&key, None).await.unwrap();
    }
    let holder = rig.locked();
    let reader = rig.locked_with(|config| config.budget = 10);
    let _ = reader.get("cas/b/k0", None).await.unwrap();
    let root = &rig.config.path;
    let before = tree(root);
    assert_eq!(reader.evict().evicted, 0);
    let halt = Arc::new(Notify::new());
    halt.notify_one();
    reader.maintain(Duration::from_secs(10), halt).await;
    assert!(rig.disk.touched.lock().unwrap().is_empty());
    assert_eq!(tree(root), before);
    assert_eq!(reader.held(), (90, 3));
    assert_eq!(reader.mode(), Mode::ReadOnly);
    assert_eq!(holder.mode(), Mode::ReadWrite);
}

/// Asserts that an object the holder published after the scan of a read-only cache is a miss
/// there, answered by one read of the upstream, and not found on the disk: the index of the
/// read-only cache is the scan it made, and it is not updated by what it did not scan. It would
/// catch a cache that goes to the directory for a name it does not know, which may be in the
/// middle of a publish.
#[tokio::test]
async fn an_object_published_after_the_scan_is_a_miss_served_from_the_upstream() {
    let rig = Rig::new("tscache-ro-late");
    rig.put("cas/b/late", &pattern(30)).await;
    let holder = rig.locked();
    let reader = rig.locked();
    assert_eq!(reader.held(), (0, 0));
    // The holder fills it now: it is on the disk, with its sidecar.
    let _ = rig.cache.get("cas/b/late", None).await.unwrap();
    assert!(rig.has_file("cas/b/late"));
    let gets = rig.calls(Op::Get);
    assert_eq!(reader.get("cas/b/late", None).await.unwrap(), pattern(30));
    assert_eq!(rig.calls(Op::Get), gets + 1);
    assert_eq!(read(&reader.counters().hits), 0);
    assert_eq!(read(&reader.counters().misses), 1);
    assert_eq!(reader.held(), (0, 0));
    assert_eq!(reader.mode(), Mode::ReadOnly);
    assert_eq!(holder.mode(), Mode::ReadWrite);
}

/// Asserts the whole of a read-only cache taking the lock: with the lock held a tick changes
/// nothing in the directory; when the lock is free the tick that takes it empties `.stage/`,
/// removes the sidecar that has no data file and the data file that has no sidecar, counts
/// the three as discarded, scans the directory
/// again (the object the holder published since is a hit now, with no read of the upstream), and
/// only then is the cache read-write: the next miss is a fill, counted, with its file on the
/// disk. It would catch a cache that is read-write before it has cleaned up and scanned, one
/// that keeps an index that the holder made stale, and one that changes the directory while the
/// holder holds the lock.
#[tokio::test]
async fn a_read_only_cache_takes_a_free_lock_then_cleans_up_scans_and_fills() {
    let rig = Rig::new("tscache-ro-take");
    for key in ["cas/b/old", "cas/b/late", "cas/b/new"] {
        rig.put(key, &pattern(30)).await;
    }
    let _ = rig.cache.get("cas/b/old", None).await.unwrap();
    let holder = rig.locked();
    let reader = rig.locked();
    assert_eq!(reader.held(), (30, 1));
    let root = &rig.config.path;
    let _ = rig.cache.get("cas/b/late", None).await.unwrap();
    leave_the_middle_of_a_publish_and_a_fill(root);
    let before = tree(root);
    assert_eq!(reader.tick().unwrap(), Tick::Waiting);
    assert_eq!(tree(root), before);
    assert_eq!(reader.mode(), Mode::ReadOnly);

    drop(holder);
    assert_eq!(reader.mode(), Mode::ReadOnly);
    assert_eq!(reader.tick().unwrap(), Tick::Took);
    assert_eq!(reader.mode(), Mode::ReadWrite);
    assert_eq!(rig.staged_files(), 0);
    assert!(!meta_path(root, &hex_of("aabb")).exists());
    // The file in `.stage/`, the sidecar with no data file and the data file with no sidecar.
    assert_eq!(reader.counters().discarded_at_start(), 3);
    assert_eq!(reader.held(), (60, 2));

    let gets = rig.calls(Op::Get);
    assert_eq!(reader.get("cas/b/late", None).await.unwrap(), pattern(30));
    assert_eq!(rig.calls(Op::Get), gets);
    assert_eq!(read(&reader.counters().hits), 1);
    assert_eq!(read(&reader.counters().fills), 0);
    assert_eq!(reader.get("cas/b/new", None).await.unwrap(), pattern(30));
    assert_eq!(read(&reader.counters().fills), 1);
    assert!(rig.has_file("cas/b/new"));
    assert_eq!(reader.tick().unwrap(), Tick::Held);
}
