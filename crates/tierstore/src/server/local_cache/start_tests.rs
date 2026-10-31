//! The start of a cache: the directory, the scan, the names, the index and the metrics text.

use std::path::Path;

use super::index::Index;
use super::layout::{self, Sidecar, data_path, is_digest, meta_path, name_of, stage_dir};
use super::metrics::read;
use super::testing::{Quirk, Rig, digest, pattern};
use super::{CacheConfig, Disk, LocalCache, SystemDisk};
use crate::{Backend, Op};
use std::sync::atomic::Ordering;

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

/// Asserts that what a fill that was running at the last stop left in the stage directory, files
/// and directories alike, is removed at start and counted, and that a start finds an empty one.
#[tokio::test]
async fn staging_leftovers_are_removed_at_start() {
    let rig = Rig::new("tscache-start-stage");
    assert_eq!(rig.cache.counters().discarded_at_start(), 0);
    let stage = stage_dir(&rig.config.path);
    write(&stage.join("0000000000000007"), b"half a fill");
    write(&stage.join("selfcheck-old").join("first"), b"probe");
    let again = rig.reopen();
    assert_eq!(std::fs::read_dir(&stage).unwrap().count(), 0);
    assert_eq!(again.counters().discarded_at_start(), 2);
    assert_eq!(read(&again.counters().staging_files), 0);
}

/// Asserts that the scan keeps the objects that are whole and deletes, and counts, every file
/// that is not part of one: a data file with no sidecar, one whose sidecar says another size or
/// is not JSON (and the sidecar they leave), a sidecar with no data file, a file whose name is
/// not a digest, and a digest-named file in the wrong directory; and that a directory it did not
/// make is left alone.
#[tokio::test]
async fn the_scan_keeps_whole_objects_and_deletes_the_rest() {
    let rig = Rig::new("tscache-start-scan");
    for n in 0..3 {
        let key = format!("cas/b/k{n}");
        rig.put(&key, &pattern(30)).await;
        let _ = rig.get(&key).await.unwrap();
    }
    let root = &rig.config.path;
    let (a, b, c, d) = (
        hex_of("aabb"),
        hex_of("ccdd"),
        hex_of("eeff"),
        hex_of("1122"),
    );
    write(&data_path(root, &a), b"no sidecar");
    write(&data_path(root, &b), b"four");
    write(&meta_path(root, &b), &sidecar(99));
    write(&data_path(root, &c), b"four");
    write(&meta_path(root, &c), b"not json");
    write(&meta_path(root, &d), &sidecar(4));
    let junk = root.join("obj/ab/cd/junk.txt");
    write(&junk, b"junk");
    let misplaced = root.join("obj/00/00").join(hex_of("ab99"));
    write(&misplaced, b"misplaced");
    let stray = root.join("obj/ab/cd/a-directory");
    std::fs::create_dir_all(&stray).unwrap();
    write(&root.join("obj/loose-file"), b"loose");

    let again = rig.reopen();
    assert_eq!(again.held(), (90, 3));
    assert_eq!(again.counters().discarded_at_start(), 8);
    for gone in [
        data_path(root, &a),
        data_path(root, &b),
        meta_path(root, &b),
        data_path(root, &c),
        meta_path(root, &c),
        meta_path(root, &d),
        junk,
        misplaced,
    ] {
        assert!(!gone.exists(), "{}", gone.display());
    }
    assert!(stray.is_dir());
    for n in 0..3 {
        assert!(rig.has_file(&format!("cas/b/k{n}")));
    }
}

/// Asserts that a cache that was filled is the same cache after a restart: what it holds is
/// served without a call to the upstream.
#[tokio::test]
async fn a_restart_serves_what_was_kept_without_the_upstream() {
    let rig = Rig::new("tscache-start-kept");
    let body = pattern(200);
    rig.put("cas/b/obj", &body).await;
    let _ = rig.get("cas/b/obj").await.unwrap();
    let again = rig.reopen();
    let gets = rig.upstream.calls(crate::Op::Get);
    assert_eq!(again.get("cas/b/obj", None).await.unwrap(), body);
    assert_eq!(again.head("cas/b/obj").await.unwrap().size, 200);
    assert_eq!(rig.upstream.calls(crate::Op::Get), gets);
    assert_eq!(again.held(), (200, 1));
    assert_eq!(again.path(), rig.config.path);
}

/// Asserts that a directory that cannot be made, because a file is in its place or in the place
/// of a directory in it, is an error at open and not a cache that fails later.
#[tokio::test]
async fn a_directory_that_cannot_be_made_is_an_error_at_open() {
    let rig = Rig::new("tscache-start-bad");
    let upstream: std::sync::Arc<dyn Backend> = rig.upstream.clone();
    let open = |path: std::path::PathBuf| {
        let config = CacheConfig::new(path, 100, Vec::new());
        LocalCache::open(
            config,
            upstream.clone(),
            rig.disk.clone(),
            rig.clock.clone(),
        )
    };
    let file = rig.dir.path().join("a-file");
    write(&file, b"in the way");
    assert!(open(file.clone()).is_err());
    let root = rig.dir.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    write(&root.join("obj"), b"in the way");
    assert!(open(root).is_err());
}

/// Asserts the names: a key under `cas/l/` with the 64 lower-case digits of a SHA-256 is named
/// by them and must hash to them; any other key, `cas/l/` keys that are not such a digest
/// included, is named by the SHA-256 of the key; and the paths put a name under its first two
/// pairs of digits, with the sidecar beside it.
#[test]
fn a_key_is_named_by_its_content_or_by_its_own_hash() {
    let sha = digest(b"content");
    let whole = name_of(&format!("cas/l/{sha}"));
    assert_eq!(
        (whole.hex.as_str(), whole.content.as_deref()),
        (sha.as_str(), Some(sha.as_str()))
    );
    let upper = format!("cas/l/{}", sha.to_uppercase());
    let short = format!("cas/l/{}", &sha[1..]);
    let other = format!("cas/x/{sha}");
    for key in [upper, short, other, "cas/l/".to_owned(), "plain".to_owned()] {
        let name = name_of(&key);
        assert_eq!(name.hex, digest(key.as_bytes()), "{key}");
        assert_eq!(name.content, None, "{key}");
    }
    assert!(is_digest(&sha));
    assert!(!is_digest(&sha.to_uppercase()));
    assert!(!is_digest(&sha[1..]));
    let root = Path::new("/c");
    let expected = format!("/c/obj/{}/{}/{sha}", &sha[..2], &sha[2..4]);
    assert_eq!(data_path(root, &sha), Path::new(&expected));
    assert_eq!(
        meta_path(root, &sha),
        Path::new(&format!("{expected}.meta"))
    );
    assert_eq!(layout::stage_dir(root), Path::new("/c/.stage"));
}

/// Asserts the index: an object added again replaces the one of that name and is charged once, a
/// removal gives the object and takes it out of the order, a use moves an object to the end of
/// the order and is noted for the next write-back, and what is noted is given once, in the order
/// of the names.
#[test]
fn the_index_orders_by_use_and_charges_each_object_once() {
    let mut index = Index::default();
    assert_eq!((index.bytes(), index.len()), (0, 0));
    index.insert("a", 10, "ta".to_owned(), 1);
    index.insert("b", 20, "tb".to_owned(), 2);
    index.insert("c", 30, "tc".to_owned(), 3);
    index.insert("a", 15, "ta2".to_owned(), 4);
    assert_eq!((index.bytes(), index.len()), (65, 3));
    assert_eq!(index.get("a").unwrap().meta().tag, "ta2");
    let sizes = |index: &Index| -> Vec<(String, u64)> { index.oldest_first() };
    let names: Vec<String> = sizes(&index).into_iter().map(|(name, _)| name).collect();
    assert_eq!(names, ["b", "c", "a"]);
    assert_eq!(index.used("b", 50), Some(20));
    assert_eq!(index.used("zz", 60), None);
    assert_eq!(index.used("b", 70), Some(20));
    let names: Vec<String> = sizes(&index).into_iter().map(|(name, _)| name).collect();
    assert_eq!(names, ["c", "a", "b"]);
    assert_eq!(index.take_touched(), vec![("b".to_owned(), 70)]);
    assert!(index.take_touched().is_empty());
    assert_eq!(index.used("c", 80), Some(30));
    assert_eq!(index.remove("c").unwrap().size, 30);
    assert!(index.remove("c").is_none());
    assert!(index.take_touched().is_empty());
    assert_eq!((index.bytes(), index.len()), (35, 2));
}

/// Asserts that the file system calls the cache makes are the system's: a create-only link that
/// refuses a name that exists, a link count that counts names, the space of a file system, and a
/// time set on a file; and that each of them says what went wrong for a path that is not there.
#[test]
fn the_system_disk_does_what_the_cache_relies_on() {
    let rig = Rig::new("tscache-start-disk");
    let dir = rig.dir.path();
    let (first, second) = (dir.join("first"), dir.join("second"));
    write(&first, b"bytes");
    assert_eq!(SystemDisk.links(&first).unwrap(), 1);
    SystemDisk.link(&first, &second).unwrap();
    assert_eq!(SystemDisk.links(&first).unwrap(), 2);
    let again = SystemDisk.link(&first, &second).unwrap_err();
    assert_eq!(again.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&second).unwrap(), b"bytes");

    SystemDisk.touch(&first, 1_700_000_000_123).unwrap();
    let modified = std::fs::metadata(&second).unwrap().modified().unwrap();
    let since = modified.duration_since(std::time::UNIX_EPOCH).unwrap();
    assert_eq!(since.as_millis(), 1_700_000_000_123);

    let space = SystemDisk.space(dir).unwrap();
    assert!(space.total > 0);
    assert!(space.free <= space.total);
    let missing = dir.join("missing");
    assert!(SystemDisk.space(&missing).is_err());
    assert!(SystemDisk.links(&missing).is_err());
    assert!(SystemDisk.touch(&missing, 1).is_err());
    assert!(SystemDisk.link(&missing, &dir.join("third")).is_err());
}

/// Asserts that the text of `/metricz` names every counter and gauge the cache keeps, with the
/// values of what happened: a miss and a fill, then a hit, a fill skipped for want of room, one
/// failed for each reason (a hash that is not the name, a short body, an upstream error, a
/// refused link, an object written over), and one eviction for each reason (the free-space
/// floor, the high water mark). Every labelled line is driven to 1 by its own event, so labels
/// that were swapped would show.
#[tokio::test]
async fn the_metrics_text_has_every_counter_with_its_value() {
    let rig = Rig::with("tscache-metrics", |config| {
        config.budget = 100;
        config.high_water = 60;
        config.low_water = 50;
    });
    rig.disk.set_space(1000, 1000);
    let wrong = format!("cas/l/{}", digest(b"name"));
    for (key, len) in [("a", 40), ("big", 500), ("s", 10), ("u", 10), ("l", 10)] {
        rig.put(&format!("cas/b/{key}"), &pattern(len)).await;
    }
    rig.put(&wrong, &pattern(10)).await;
    let _ = rig.get("cas/b/a").await.unwrap();
    let _ = rig.get("cas/b/a").await.unwrap();
    let _ = rig.get("cas/b/big").await.unwrap();
    assert!(rig.get(&wrong).await.is_err());
    rig.tamper.short.store(true, Ordering::SeqCst);
    // The upstream is short for the reader too, which is the reader's error.
    assert!(rig.get("cas/b/s").await.is_err());
    rig.tamper.short.store(false, Ordering::SeqCst);
    rig.upstream.fail_next(Op::Head, 1);
    assert!(rig.get("cas/b/u").await.is_err());
    rig.disk
        .set(Quirk::LinkFails(std::io::ErrorKind::PermissionDenied));
    let _ = rig.get("cas/b/l").await.unwrap();
    rig.disk.set(Quirk::None);
    let gate = rig.tamper.hold();
    let cache = rig.cache.clone();
    let reader = tokio::spawn(async move { cache.get("cas/b/l", None).await });
    while read(&rig.cache.counters().staging_files) < 1 {
        tokio::task::yield_now().await;
    }
    rig.replace("cas/b/l", &pattern(11)).await;
    gate.add_permits(1000);
    assert_eq!(reader.await.unwrap().unwrap().len(), 11);
    rig.disk.set_space(1000, 50);
    assert_eq!(rig.cache.evict().evicted, 1);
    rig.disk.set_space(1000, 1000);
    rig.put("cas/b/f1", &pattern(40)).await;
    rig.put("cas/b/f2", &pattern(30)).await;
    let _ = rig.get("cas/b/f1").await.unwrap();
    let _ = rig.get("cas/b/f2").await.unwrap();
    let text = rig.cache.metrics();
    let expected = [
        "tierstore_cache_hits_total 1\n",
        "tierstore_cache_misses_total 9\n",
        "tierstore_cache_bytes_served_local_total 40\n",
        "tierstore_cache_fills_total 3\n",
        "tierstore_cache_fills_skipped_total 1\n",
        "tierstore_cache_pinned_skips_total 0\n",
        "tierstore_cache_discarded_at_start_total 0\n",
        "tierstore_cache_bytes 30\n",
        "tierstore_cache_objects 1\n",
        "tierstore_cache_fs_free_bytes 1000\n",
        "tierstore_cache_staging_files 0\n",
        "tierstore_cache_fill_failures_total{reason=\"hash_mismatch\"} 1\n",
        "tierstore_cache_fill_failures_total{reason=\"upstream_error\"} 1\n",
        "tierstore_cache_fill_failures_total{reason=\"short_body\"} 1\n",
        "tierstore_cache_fill_failures_total{reason=\"local_io\"} 1\n",
        "tierstore_cache_fill_failures_total{reason=\"changed\"} 1\n",
        "tierstore_cache_evictions_total{reason=\"budget\"} 1\n",
        "tierstore_cache_evictions_total{reason=\"floor\"} 1\n",
        "tierstore_cache_bytes_served_upstream_total ",
    ];
    for line in expected {
        assert!(text.contains(line), "{line} in {text}");
    }
}
