//! A fill: what it keeps, what it refuses, and that the readers of one key share it.

use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::Ordering;

use bytes::Bytes;

use super::layout::{Sidecar, data_path, meta_path, name_of};
use super::metrics::{FailedFill, read};
use super::testing::{Quirk, Rig, digest, pattern};
use crate::{Backend, Error, Op};

/// The number of entries in the stage directory of the rig.
fn staged(rig: &Rig) -> usize {
    let stage = rig.config.path.join(".stage");
    std::fs::read_dir(stage).unwrap().count()
}

/// Asserts that the misses of one key, all of them started before the upstream answers any,
/// fill the object once: one HEAD and one GET reach the upstream, one fill is counted, and every
/// reader gets the bytes. A defect that let each miss fetch would show in the call counts.
#[tokio::test]
async fn the_misses_of_one_key_fill_it_once() {
    let rig = Rig::new("tscache-once");
    let body = pattern(5000);
    rig.put("cas/b/obj", &body).await;
    let gate = rig.tamper.hold();
    let mut readers = Vec::new();
    for _ in 0..8 {
        let cache = rig.cache.clone();
        readers.push(tokio::spawn(
            async move { cache.get("cas/b/obj", None).await },
        ));
    }
    while read(&rig.cache.counters().misses) < 8 {
        tokio::task::yield_now().await;
    }
    gate.add_permits(16);
    for reader in readers {
        assert_eq!(reader.await.unwrap().unwrap(), body);
    }
    // One HEAD before the read of the object and one after it (to see it did not change).
    assert_eq!((rig.calls(Op::Head), rig.calls(Op::Get)), (2, 1));
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.fills), 1);
    assert_eq!(read(&counters.hits), 0);
    assert_eq!(read(&counters.misses), 8);
    assert_eq!(read(&counters.bytes_served_upstream), 8 * 5000);
    assert_eq!(rig.cache.held(), (5000, 1));
}

/// Asserts the on-disk form of a kept object: the raw bytes, mode 0444, under the first two
/// pairs of the digits of the name; the name of a key under `cas/l/` is the SHA-256 of the
/// bytes, any other key's the SHA-256 of the key; the sidecar holds the key, the size, the hash
/// of the bytes and the upstream's tag; nothing is left in the stage directory.
#[tokio::test]
async fn a_kept_object_is_raw_bytes_under_its_name_with_a_sidecar() {
    let rig = Rig::new("tscache-layout");
    let body = pattern(700);
    let sha = digest(&body);
    let whole = format!("cas/l/{sha}");
    rig.put(&whole, &body).await;
    rig.put("cas/b/plain", &body).await;
    assert_eq!(rig.get(&whole).await.unwrap(), body);
    assert_eq!(rig.get("cas/b/plain").await.unwrap(), body);

    let root = &rig.config.path;
    let by_content = root.join("obj").join(&sha[..2]).join(&sha[2..4]).join(&sha);
    assert_eq!(rig.file(&whole), by_content);
    let named = digest(b"cas/b/plain");
    assert_eq!(rig.file("cas/b/plain"), data_path(root, &named));
    for key in [whole.as_str(), "cas/b/plain"] {
        let file = rig.file(key);
        assert_eq!(std::fs::read(&file).unwrap(), body);
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o444);
        let hex = name_of(key).hex;
        let text = std::fs::read(meta_path(root, &hex)).unwrap();
        let sidecar: Sidecar = serde_json::from_slice(&text).unwrap();
        let upstream = rig.tamper.memory.head(key).await.unwrap();
        let expected = Sidecar {
            key: key.to_owned(),
            size: 700,
            sha256: sha.clone(),
            tag: upstream.tag,
            modified: upstream.modified,
        };
        assert_eq!(sidecar, expected);
    }
    assert_eq!(staged(&rig), 0);
    assert_eq!(read(&rig.cache.counters().staging_files), 0);
}

/// Asserts that a key named by its content whose bytes hash to another name is not kept, is
/// counted, and is the reader's error (bytes known to be wrong go to nobody), that nothing is
/// left in the stage directory, and that the upstream was read once.
#[tokio::test]
async fn bytes_that_do_not_hash_to_their_name_are_not_kept_and_not_served() {
    let rig = Rig::new("tscache-mismatch");
    let wrong = pattern(50);
    let key = format!("cas/l/{}", digest(b"what the name says"));
    rig.put(&key, &wrong).await;
    let error = rig.get(&key).await.unwrap_err();
    assert!(error.is_unavailable(), "{error}");
    let counters = rig.cache.counters();
    assert_eq!(counters.failures(FailedFill::HashMismatch), 1);
    assert_eq!(read(&counters.fills), 0);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(&key));
    assert_eq!(staged(&rig), 0);
    assert_eq!(read(&counters.bytes_served_upstream), 0);
    assert_eq!(rig.calls(Op::Get), 1);
}

/// Asserts that an upstream that gives fewer bytes than its HEAD said leaves nothing in the
/// cache and is counted as a short body.
#[tokio::test]
async fn a_short_upstream_body_is_not_kept() {
    let rig = Rig::new("tscache-short");
    rig.put("cas/b/obj", &pattern(100)).await;
    rig.tamper
        .short
        .store(true, std::sync::atomic::Ordering::SeqCst);
    // The reader is served from the upstream, which is short too.
    assert_eq!(rig.get("cas/b/obj").await.unwrap().len(), 99);
    assert_eq!(rig.cache.counters().failures(FailedFill::ShortBody), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/obj"));
    assert_eq!(staged(&rig), 0);
}

/// Asserts that an object larger than a chunk is read in chunks and kept whole and equal, with
/// the last chunk the size of the rest; and that an empty object is kept and read back empty.
#[tokio::test]
async fn an_object_is_read_in_chunks_and_an_empty_one_is_kept() {
    let rig = Rig::with("tscache-chunks", |config| config.fill_chunk = 10);
    let body = pattern(25);
    rig.put("cas/b/obj", &body).await;
    rig.put("cas/b/empty", b"").await;
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(rig.calls(Op::Get), 3);
    assert_eq!(std::fs::read(rig.file("cas/b/obj")).unwrap(), body);
    assert!(rig.get("cas/b/empty").await.unwrap().is_empty());
    assert!(rig.get("cas/b/empty").await.unwrap().is_empty());
    assert_eq!(rig.cache.held(), (25, 2));
    assert_eq!(read(&rig.cache.counters().hits), 1);
}

/// Asserts that an upstream that fails a HEAD or a GET during a fill is the reader's error, is
/// counted as an upstream error, leaves nothing staged, and that the next read fills.
#[tokio::test]
async fn an_upstream_that_fails_is_the_readers_error_and_is_counted() {
    let rig = Rig::new("tscache-upstream-error");
    rig.put("cas/b/obj", &pattern(100)).await;
    rig.upstream.fail_next(Op::Head, 1);
    assert!(rig.get("cas/b/obj").await.unwrap_err().is_unavailable());
    rig.upstream.fail_next(Op::Get, 1);
    assert!(rig.get("cas/b/obj").await.unwrap_err().is_unavailable());
    let counters = rig.cache.counters();
    assert_eq!(counters.failures(FailedFill::UpstreamError), 2);
    assert_eq!(staged(&rig), 0);
    assert_eq!(rig.cache.held(), (0, 0));
    assert_eq!(rig.get("cas/b/obj").await.unwrap().len(), 100);
    assert_eq!(read(&counters.fills), 1);
}

/// Asserts that a fill is skipped, counted, and served from the upstream when the object is
/// larger than the budget or would take the cache above its critical mark, and that an object
/// that brings it exactly to the mark is kept.
#[tokio::test]
async fn a_fill_that_does_not_fit_is_skipped_and_the_reader_is_served() {
    let rig = Rig::with("tscache-room", |config| {
        config.budget = 100;
        config.high_water = 100;
        config.low_water = 99;
    });
    for (key, len) in [("big", 200), ("a", 50), ("b", 45), ("c", 1)] {
        rig.put(&format!("cas/b/{key}"), &pattern(len)).await;
    }
    assert_eq!(rig.get("cas/b/big").await.unwrap().len(), 200);
    assert_eq!(rig.cache.held(), (0, 0));
    assert_eq!(rig.get("cas/b/a").await.unwrap().len(), 50);
    assert_eq!(rig.get("cas/b/b").await.unwrap().len(), 45);
    assert_eq!(rig.cache.held(), (95, 2));
    assert_eq!(rig.get("cas/b/c").await.unwrap().len(), 1);
    assert_eq!(rig.cache.held(), (95, 2));
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.fills_skipped), 2);
    assert_eq!(read(&counters.fills), 2);
}

/// Asserts that a stage directory that cannot be written is not the reader's error: the fill
/// fails and is counted as a local error, and the reader is served from the upstream.
#[tokio::test]
async fn an_unwritable_stage_directory_fails_the_fill_not_the_read() {
    let rig = Rig::new("tscache-stage-broken");
    let body = pattern(100);
    rig.put("cas/b/obj", &body).await;
    let stage = rig.config.path.join(".stage");
    std::fs::remove_dir_all(&stage).unwrap();
    std::fs::write(&stage, b"not a directory").unwrap();
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(rig.cache.counters().failures(FailedFill::LocalIo), 1);
    assert_eq!(rig.cache.held(), (0, 0));
}

/// Asserts that a link the file system refuses fails the fill as a local error, leaves no
/// sidecar and no staged file, and serves the reader from the upstream.
#[tokio::test]
async fn a_link_the_file_system_refuses_fails_the_fill_and_leaves_nothing() {
    let rig = Rig::new("tscache-link-refused");
    let body = pattern(100);
    rig.put("cas/b/obj", &body).await;
    rig.disk
        .set(Quirk::LinkFails(std::io::ErrorKind::PermissionDenied));
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(rig.cache.counters().failures(FailedFill::LocalIo), 1);
    let hex = name_of("cas/b/obj").hex;
    assert!(!meta_path(&rig.config.path, &hex).exists());
    assert_eq!(staged(&rig), 0);
    assert_eq!(rig.cache.held(), (0, 0));
}

/// Asserts that a name that is already in the directory when a fill links (the index did not
/// know it) is kept as it is: the fill is not counted, and the read is answered from the file.
#[tokio::test]
async fn a_name_that_exists_is_kept_and_the_fill_is_not_counted() {
    let rig = Rig::new("tscache-exists");
    let body = pattern(100);
    rig.put("cas/b/obj", &body).await;
    let hex = name_of("cas/b/obj").hex;
    let (data, meta) = (
        data_path(&rig.config.path, &hex),
        meta_path(&rig.config.path, &hex),
    );
    std::fs::create_dir_all(data.parent().unwrap()).unwrap();
    std::fs::write(&data, &body).unwrap();
    let upstream = rig.tamper.memory.head("cas/b/obj").await.unwrap();
    let sidecar = Sidecar {
        key: "cas/b/obj".to_owned(),
        size: 100,
        sha256: digest(&body),
        tag: upstream.tag,
        modified: upstream.modified,
    };
    std::fs::write(&meta, serde_json::to_vec(&sidecar).unwrap()).unwrap();
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(read(&rig.cache.counters().fills), 0);
    assert_eq!(rig.cache.held(), (100, 1));
}

/// Asserts that a fill goes on and ends when the reader that started it goes away, so the
/// object is kept and the table of the fills that run is empty again.
#[tokio::test]
async fn a_fill_ends_when_its_reader_goes_away() {
    let rig = Rig::new("tscache-abandoned");
    rig.put("cas/b/obj", &pattern(100)).await;
    let gate = rig.tamper.hold();
    let cache = rig.cache.clone();
    let reader = tokio::spawn(async move { cache.get("cas/b/obj", None).await });
    while read(&rig.cache.counters().misses) < 1 {
        tokio::task::yield_now().await;
    }
    reader.abort();
    assert!(reader.await.unwrap_err().is_cancelled());
    gate.add_permits(16);
    while read(&rig.cache.counters().fills) < 1 {
        tokio::task::yield_now().await;
    }
    assert!(super::locked(&rig.cache.inner.flights).is_empty());
    assert_eq!(rig.cache.held(), (100, 1));
    assert_eq!(rig.get("cas/b/obj").await.unwrap().len(), 100);
    assert_eq!(read(&rig.cache.counters().hits), 1);
}

/// Asserts that a file being filled is counted while it exists and is never evicted by a pass,
/// and that the count is back to zero when the fill ends.
#[tokio::test]
async fn staged_files_are_counted_and_never_evicted() {
    let rig = Rig::with("tscache-staged", |config| {
        config.budget = 1000;
        config.high_water = 100;
        config.low_water = 50;
    });
    rig.put("cas/b/obj", &pattern(100)).await;
    let gate = rig.tamper.hold();
    let cache = rig.cache.clone();
    let reader = tokio::spawn(async move { cache.get("cas/b/obj", None).await });
    while read(&rig.cache.counters().staging_files) < 1 {
        tokio::task::yield_now().await;
    }
    assert_eq!(staged(&rig), 1);
    assert!(
        rig.cache
            .metrics()
            .contains("tierstore_cache_staging_files 1\n")
    );
    assert_eq!(rig.cache.evict().evicted, 0);
    assert_eq!(staged(&rig), 1);
    gate.add_permits(16);
    assert_eq!(reader.await.unwrap().unwrap().len(), 100);
    assert_eq!(read(&rig.cache.counters().staging_files), 0);
    assert_eq!(staged(&rig), 0);
}

/// Asserts that the error of a missing key is the reader's `NotFound` and is not counted as a
/// failed fill, and that a ranged read of an object that is not there says the same.
#[tokio::test]
async fn a_missing_key_is_not_found_and_not_a_failed_fill() {
    let rig = Rig::new("tscache-missing");
    let absent = Error::NotFound("cas/b/none".to_owned());
    assert_eq!(rig.get("cas/b/none").await, Err(absent.clone()));
    let range = crate::ByteRange::new(0, 5);
    assert_eq!(rig.cache.get("cas/b/none", Some(range)).await, Err(absent));
    let counters = rig.cache.counters();
    assert_eq!(counters.failures(FailedFill::UpstreamError), 0);
    assert_eq!(read(&counters.fills_skipped), 0);
}

/// Asserts that a range that starts past the end of an object that is not in the cache is the
/// error any backend gives, after the object was filled (the range is judged by its size).
#[tokio::test]
async fn a_range_past_the_end_of_an_uncached_object_is_an_error() {
    let rig = Rig::new("tscache-range-past");
    rig.put("cas/b/obj", &pattern(100)).await;
    let range = crate::ByteRange::new(101, 5);
    let text = "range starting at 101 is past the end of the 100-byte object";
    let past = rig.cache.get("cas/b/obj", Some(range)).await;
    assert_eq!(past, Err(Error::Other(text.to_owned())));
    assert_eq!(rig.cache.held(), (100, 1));
}

/// Asserts the ranged reads of an object that is not in the cache: a small one fills the whole
/// object and answers the range; one over the limit passes the range through and keeps nothing,
/// while a whole read of the same object fills it.
#[tokio::test]
async fn a_ranged_read_fills_a_small_object_and_passes_a_large_one_through() {
    let rig = Rig::with("tscache-ranged", |config| config.whole_limit = 100);
    let small = pattern(100);
    let large = pattern(101);
    rig.put("cas/b/small", &small).await;
    rig.put("cas/b/large", &large).await;
    let range = crate::ByteRange::new(10, 20);
    let part = rig.cache.get("cas/b/small", Some(range)).await.unwrap();
    assert_eq!(part, small[10..30]);
    assert!(rig.has_file("cas/b/small"));
    assert_eq!(rig.cache.held(), (100, 1));

    let part = rig.cache.get("cas/b/large", Some(range)).await.unwrap();
    assert_eq!(part, large[10..30]);
    assert!(!rig.has_file("cas/b/large"));
    assert_eq!(rig.cache.held(), (100, 1));
    let counters = rig.cache.counters();
    assert_eq!(read(&counters.misses), 2);
    assert_eq!(read(&counters.fills), 1);

    assert_eq!(rig.get("cas/b/large").await.unwrap(), large);
    assert!(rig.has_file("cas/b/large"));
    assert_eq!(rig.cache.held(), (201, 2));
}

/// Asserts that a reader whose miss was answered by a fill that ended before it asked for a fill
/// of its own finds the object in the index and starts none: no HEAD and no GET reach the
/// upstream, and nothing is counted as a fill. A defect that looked only at the table of fills
/// that run would fill again.
#[tokio::test]
async fn a_fill_that_ended_after_the_miss_is_not_run_again() {
    let rig = Rig::new("tscache-late-miss");
    rig.put("cas/b/obj", &pattern(100)).await;
    assert_eq!(rig.get("cas/b/obj").await.unwrap().len(), 100);
    let (heads, gets) = (rig.calls(Op::Head), rig.calls(Op::Get));
    let name = name_of("cas/b/obj");
    let late = rig.cache.inner.join_or_start("cas/b/obj", &name);
    assert_eq!(late.err(), Some(super::fill::Filled::Kept));
    assert_eq!((rig.calls(Op::Head), rig.calls(Op::Get)), (heads, gets));
    assert_eq!(read(&rig.cache.counters().fills), 1);
}

/// Asserts that the bytes of the fills that are running count against the critical mark: two
/// objects of 50 bytes in a cache of 100 (critical 95) filled at once keep one, skip the other
/// (counted) and serve both readers, and the reservation is back to zero when they end. A
/// defect that counted only the objects in the index would keep both.
#[tokio::test]
async fn fills_that_run_together_are_held_to_the_critical_mark() {
    let rig = Rig::with("tscache-reserved", |config| {
        config.budget = 100;
        config.high_water = 100;
        config.low_water = 50;
    });
    rig.put("cas/b/one", &pattern(50)).await;
    rig.put("cas/b/two", &pattern(50)).await;
    let gate = rig.tamper.hold();
    let mut readers = Vec::new();
    for key in ["cas/b/one", "cas/b/two"] {
        let cache = rig.cache.clone();
        readers.push(tokio::spawn(async move { cache.get(key, None).await }));
    }
    while read(&rig.cache.counters().fills_skipped) < 1 {
        tokio::task::yield_now().await;
    }
    assert_eq!(rig.cache.inner.reserved.load(Ordering::SeqCst), 50);
    gate.add_permits(64);
    for reader in readers {
        assert_eq!(reader.await.unwrap().unwrap().len(), 50);
    }
    assert_eq!(rig.cache.inner.reserved.load(Ordering::SeqCst), 0);
    assert_eq!(rig.cache.held(), (50, 1));
    assert_eq!(read(&rig.cache.counters().fills), 1);
}

/// Asserts that an object written over while it is read is not kept: the second HEAD shows
/// another tag, the fill is counted as `changed`, nothing is left staged or indexed, and the
/// reader, who may have been given bytes of the first version, gets an `Unavailable` error
/// and not a mix. Also drives the upstream's conditional put and delete.
#[tokio::test]
async fn an_object_written_over_during_its_fill_is_not_kept() {
    let rig = Rig::new("tscache-changed");
    rig.put("cas/b/obj", &pattern(100)).await;
    let gate = rig.tamper.hold();
    let cache = rig.cache.clone();
    let reader = tokio::spawn(async move { cache.get("cas/b/obj", None).await });
    while read(&rig.cache.counters().staging_files) < 1 {
        tokio::task::yield_now().await;
    }
    let old = rig.tamper.head("cas/b/obj").await.unwrap().tag;
    let new = Bytes::from(vec![7u8; 100]);
    let put = rig
        .tamper
        .put_if_match("cas/b/obj", new.clone(), &old)
        .await;
    assert!(put.is_ok());
    gate.add_permits(64);
    assert!(reader.await.unwrap().unwrap_err().is_unavailable());
    assert_eq!(rig.cache.counters().failures(FailedFill::Changed), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/obj"));
    assert_eq!(staged(&rig), 0);
    rig.tamper.delete("cas/b/obj").await.unwrap();
    let absent = Error::NotFound("cas/b/obj".to_owned());
    assert_eq!(rig.get("cas/b/obj").await, Err(absent));
}

/// Asserts that a write the cache directory refuses at each step of publishing (a name in the
/// way of the directory, of the sidecar's temporary file, of the sidecar) fails the fill as a
/// local error, not the read, and keeps nothing.
#[tokio::test]
async fn each_step_of_publishing_that_fails_fails_the_fill_not_the_read() {
    for step in ["dirs", "sidecar-write", "sidecar-rename"] {
        let rig = Rig::new(&format!("tscache-publish-{step}"));
        let body = pattern(100);
        rig.put("cas/b/obj", &body).await;
        let hex = name_of("cas/b/obj").hex;
        let root = &rig.config.path;
        match step {
            "dirs" => std::fs::write(root.join("obj").join(&hex[..2]), b"x").unwrap(),
            "sidecar-write" => {
                std::fs::create_dir(root.join(".stage").join("0000000000000000.meta")).unwrap();
            }
            _ => std::fs::create_dir_all(meta_path(root, &hex)).unwrap(),
        }
        assert_eq!(rig.get("cas/b/obj").await.unwrap(), body, "{step}");
        assert_eq!(
            rig.cache.counters().failures(FailedFill::LocalIo),
            1,
            "{step}"
        );
        assert_eq!(rig.cache.held(), (0, 0), "{step}");
        assert!(!rig.has_file("cas/b/obj"), "{step}");
    }
}

/// Asserts the modes of what a fill makes however the umask is set (a restrictive one and none
/// at all): the directories 0755, the data file 0444, the sidecar 0644. A defect that left them
/// to the umask would give 0700 directories and a 0400 file, or a world-writable sidecar.
#[tokio::test]
async fn modes_do_not_depend_on_the_umask() {
    for mask in [0o077, 0o000] {
        let previous = rustix::process::umask(rustix::fs::Mode::from_bits_truncate(mask));
        let rig = Rig::new(&format!("tscache-umask-{mask:o}"));
        rig.put("cas/b/obj", &pattern(100)).await;
        let got = rig.get("cas/b/obj").await;
        let _ = rustix::process::umask(previous);
        assert_eq!(got.unwrap().len(), 100);
        let mode =
            |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let file = rig.file("cas/b/obj");
        let bb = file.parent().unwrap();
        let aa = bb.parent().unwrap();
        let obj = aa.parent().unwrap();
        for dir in [bb, aa, obj, &rig.config.path.join(".stage")] {
            assert_eq!(mode(dir), 0o755, "{mask:o} {}", dir.display());
        }
        assert_eq!(mode(&file), 0o444, "{mask:o}");
        let hex = name_of("cas/b/obj").hex;
        assert_eq!(mode(&meta_path(&rig.config.path, &hex)), 0o644, "{mask:o}");
    }
}

/// Asserts that a read error that does not say the file is gone (here a directory where the
/// file was) leaves the entry and the files alone, and the reader is served from the upstream,
/// while a file that is gone drops the entry.
#[tokio::test]
async fn only_a_missing_file_drops_the_entry() {
    let rig = Rig::new("tscache-read-error");
    let body = pattern(100);
    rig.put("cas/b/obj", &body).await;
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    let file = rig.file("cas/b/obj");
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(rig.cache.held(), (100, 1));
    std::fs::remove_dir(&file).unwrap();
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(read(&rig.cache.counters().fills), 2);
    assert_eq!(rig.cache.held(), (100, 1));
}
