//! The read cache.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use kbf_pack::{CacheError, ReadCache};

use crate::support::{blob, cache_dir, filler};

fn name_of(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

/// Sets the modification time of `path` to `secs` seconds after the epoch.
fn set_age(path: &Path, secs: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

/// How many files of `dir` are temporary files of a write.
fn temp_files(dir: &Path) -> usize {
    let listing = std::fs::read_dir(dir).unwrap();
    let names = listing.flatten().map(|entry| name_of(&entry.path()));
    names.filter(|name| name.ends_with(".tmp")).count()
}

/// Asserts that a blob put in the cache is got back, that the cache counts it, and that it is
/// one file named for the digest.
#[test]
fn a_blob_put_is_got_back_and_counted() {
    let dir = cache_dir("cache-round-trip");
    let cache = ReadCache::open(dir.path(), 1000).unwrap();
    let x = blob("alpha");
    assert!(cache.get(&x.digest).is_none());

    cache.put(&x.digest, &x.bytes).unwrap();

    assert_eq!(cache.get(&x.digest), Some(x.bytes.clone()));
    assert_eq!(cache.entries(), 1);
    assert_eq!(cache.used_bytes(), 5);
    assert!(cache.path_of(&x.digest).is_file());
    assert_eq!(temp_files(dir.path()), 0);
}

/// Asserts that bytes that are not the digest they are offered under are refused and leave no
/// file.
#[test]
fn bytes_that_are_not_the_digest_are_not_kept() {
    let dir = cache_dir("cache-mismatch");
    let cache = ReadCache::open(dir.path(), 1000).unwrap();
    let x = blob("alpha");

    let error = cache.put(&x.digest, b"omega").unwrap_err();

    assert!(matches!(error, CacheError::Mismatch { .. }), "{error}");
    assert!(error.to_string().contains("do not hash"), "{error}");
    assert_eq!(cache.entries(), 0);
    assert!(!cache.path_of(&x.digest).exists());
}

/// Asserts that a file whose bytes are not its digest is not served: it is dropped from the
/// cache and from the disk, and the blob is not held any more.
#[test]
fn a_file_that_is_not_its_digest_is_dropped_when_read() {
    let dir = cache_dir("cache-corrupt");
    let cache = ReadCache::open(dir.path(), 1000).unwrap();
    let x = blob("alpha");
    cache.put(&x.digest, &x.bytes).unwrap();
    std::fs::write(cache.path_of(&x.digest), b"AAAAA").unwrap();

    assert!(cache.get(&x.digest).is_none());

    assert_eq!(cache.entries(), 0);
    assert_eq!(cache.used_bytes(), 0);
    assert!(!cache.path_of(&x.digest).exists());
}

/// Asserts that a file that has gone from the disk is dropped from the cache when it is read.
#[test]
fn a_file_that_is_gone_is_dropped_when_read() {
    let dir = cache_dir("cache-gone");
    let cache = ReadCache::open(dir.path(), 1000).unwrap();
    let x = blob("alpha");
    cache.put(&x.digest, &x.bytes).unwrap();
    std::fs::remove_file(cache.path_of(&x.digest)).unwrap();

    assert!(cache.get(&x.digest).is_none());

    assert_eq!(cache.entries(), 0);
    assert_eq!(cache.used_bytes(), 0);
}

/// Asserts that the least recently used blob goes first when the cache is over its budget:
/// reading a blob makes it the most recently used, so the blob put before the one read is the
/// one that goes, and its file goes with it.
#[test]
fn the_least_recently_used_blob_is_evicted_to_the_budget() {
    let dir = cache_dir("cache-lru");
    let cache = ReadCache::open(dir.path(), 100).unwrap();
    let (a, b, c) = (filler(40, 1), filler(40, 2), filler(40, 3));
    cache.put(&a.digest, &a.bytes).unwrap();
    cache.put(&b.digest, &b.bytes).unwrap();
    assert!(cache.get(&a.digest).is_some());

    cache.put(&c.digest, &c.bytes).unwrap();

    assert!(cache.get(&b.digest).is_none());
    assert!(cache.get(&a.digest).is_some());
    assert!(cache.get(&c.digest).is_some());
    assert_eq!(cache.used_bytes(), 80);
    assert!(!cache.path_of(&b.digest).exists());
}

/// Asserts that putting a blob the cache holds keeps one copy and makes it the most recently
/// used.
#[test]
fn a_blob_put_again_is_kept_once_and_becomes_the_most_recent() {
    let dir = cache_dir("cache-again");
    let cache = ReadCache::open(dir.path(), 100).unwrap();
    let (a, b, c) = (filler(40, 1), filler(40, 2), filler(40, 3));
    cache.put(&a.digest, &a.bytes).unwrap();
    cache.put(&b.digest, &b.bytes).unwrap();
    cache.put(&a.digest, &a.bytes).unwrap();
    assert_eq!(cache.entries(), 2);
    assert_eq!(cache.used_bytes(), 80);

    cache.put(&c.digest, &c.bytes).unwrap();

    assert!(cache.get(&b.digest).is_none());
    assert!(cache.get(&a.digest).is_some());
}

/// Asserts that a blob larger than the whole budget is not kept, and does not push out the
/// blobs that are.
#[test]
fn a_blob_larger_than_the_budget_is_not_kept() {
    let dir = cache_dir("cache-large");
    let cache = ReadCache::open(dir.path(), 100).unwrap();
    let small = filler(40, 1);
    let large = filler(101, 2);
    cache.put(&small.digest, &small.bytes).unwrap();

    cache.put(&large.digest, &large.bytes).unwrap();

    assert!(cache.get(&large.digest).is_none());
    assert!(!cache.path_of(&large.digest).exists());
    assert_eq!(cache.entries(), 1);
    assert!(cache.get(&small.digest).is_some());
}

/// Asserts that an empty blob is kept and got back, and takes no bytes of the budget.
#[test]
fn an_empty_blob_is_kept() {
    let dir = cache_dir("cache-empty");
    let cache = ReadCache::open(dir.path(), 100).unwrap();
    let empty = blob("");

    cache.put(&empty.digest, &empty.bytes).unwrap();

    assert_eq!(cache.get(&empty.digest), Some(empty.bytes.clone()));
    assert_eq!(cache.entries(), 1);
    assert_eq!(cache.used_bytes(), 0);
}

/// Asserts that the blobs of a cache are there when the directory is opened again, and that
/// when the budget is smaller the blobs modified longest ago go first.
#[test]
fn blobs_survive_a_reopen_and_the_oldest_files_go_first_under_a_smaller_budget() {
    let dir = cache_dir("cache-reopen");
    let (a, b) = (filler(40, 1), filler(40, 2));
    {
        let cache = ReadCache::open(dir.path(), 100).unwrap();
        cache.put(&a.digest, &a.bytes).unwrap();
        cache.put(&b.digest, &b.bytes).unwrap();
        set_age(&cache.path_of(&a.digest), 1000);
        set_age(&cache.path_of(&b.digest), 2000);
    }

    let same = ReadCache::open(dir.path(), 100).unwrap();
    assert_eq!(same.entries(), 2);
    assert_eq!(same.used_bytes(), 80);
    assert_eq!(same.get(&a.digest), Some(a.bytes.clone()));
    assert_eq!(same.get(&b.digest), Some(b.bytes.clone()));
    drop(same);

    let smaller = ReadCache::open(dir.path(), 50).unwrap();
    assert_eq!(smaller.entries(), 1);
    assert!(smaller.get(&a.digest).is_none());
    assert!(!smaller.path_of(&a.digest).exists());
    assert_eq!(smaller.get(&b.digest), Some(b.bytes.clone()));
}

/// Asserts that opening a directory removes what the cache made and cannot use (a temporary
/// file a crash left, a blob file of the wrong length) and leaves alone every file whose name
/// is not exactly a blob's or a temporary file's: no name, a name that is not a hash, a size
/// that is not a number, a hash in capitals, and names that end in the temporary suffix but
/// are not a blob's name, a dot and a number.
#[test]
fn open_removes_debris_and_leaves_other_files_alone() {
    let dir = cache_dir("cache-debris");
    let kept = blob("alpha");
    let short = blob("bravo");
    let leftover = {
        let cache = ReadCache::open(dir.path(), 1000).unwrap();
        cache.put(&kept.digest, &kept.bytes).unwrap();
        std::fs::write(cache.path_of(&short.digest), b"brav").unwrap();
        let temp = format!("{}.3.tmp", name_of(&cache.path_of(&kept.digest)));
        std::fs::write(dir.path().join(&temp), b"half a blob").unwrap();
        temp
    };
    let foreign = [
        "notes.txt".to_owned(),
        "short-5".to_owned(),
        format!("{}-abc", "00".repeat(32)),
        format!("{}-5", "AB".repeat(32)),
        "notes.tmp".to_owned(),
        "notes.7.tmp".to_owned(),
        format!("{}-5.x.tmp", "00".repeat(32)),
    ];
    for name in &foreign {
        std::fs::write(dir.path().join(name), b"keep").unwrap();
    }

    let cache = ReadCache::open(dir.path(), 1000).unwrap();

    assert_eq!(cache.entries(), 1);
    assert_eq!(cache.used_bytes(), 5);
    assert_eq!(cache.get(&kept.digest), Some(kept.bytes.clone()));
    assert!(!cache.path_of(&short.digest).exists());
    assert!(!dir.path().join(leftover).exists());
    for name in &foreign {
        assert!(dir.path().join(name).exists(), "{name}");
    }
}

/// Asserts that the directories the open makes and the files the cache writes give no access to
/// the group or to others, so that other users of the machine cannot read the blobs.
#[test]
fn the_directories_and_files_the_cache_makes_are_private_to_the_owner() {
    let dir = cache_dir("cache-private");
    let outer = dir.path().join("outer");
    let inner = outer.join("inner");
    let cache = ReadCache::open(&inner, 1000).unwrap();
    let x = blob("alpha");

    cache.put(&x.digest, &x.bytes).unwrap();

    for path in [outer, inner, cache.path_of(&x.digest)] {
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{path:?}");
    }
}

/// Asserts that a cache directory that is a file is an error of the open, not a cache.
#[test]
fn a_cache_directory_that_is_a_file_is_an_error() {
    let dir = cache_dir("cache-file");
    let path = dir.path().join("not-a-directory");
    std::fs::write(&path, b"x").unwrap();

    let error = ReadCache::open(&path, 10).err().unwrap();

    assert!(matches!(error, CacheError::Io { op: "open", .. }));
    assert!(error.to_string().contains("cannot open"), "{error}");
}

/// Asserts that a put into a directory that has gone is an error and holds nothing.
#[test]
fn a_put_into_a_directory_that_is_gone_is_an_error() {
    let dir = cache_dir("cache-gone-dir");
    let cache = ReadCache::open(dir.path(), 1000).unwrap();
    std::fs::remove_dir_all(dir.path()).unwrap();
    let x = blob("alpha");

    let error = cache.put(&x.digest, &x.bytes).unwrap_err();

    assert!(matches!(error, CacheError::Io { op: "write", .. }));
    assert_eq!(cache.entries(), 0);
}

/// Asserts that a file that cannot be renamed into place is an error, holds nothing, and
/// leaves no temporary file behind.
#[test]
fn a_file_that_cannot_be_renamed_into_place_is_an_error() {
    let dir = cache_dir("cache-rename");
    let cache = ReadCache::open(dir.path(), 1000).unwrap();
    let x = blob("alpha");
    let occupied = cache.path_of(&x.digest).join("inner");
    std::fs::create_dir_all(occupied).unwrap();

    let error = cache.put(&x.digest, &x.bytes).unwrap_err();

    assert!(matches!(error, CacheError::Io { op: "write", .. }));
    assert_eq!(cache.entries(), 0);
    assert_eq!(temp_files(dir.path()), 0);
}
