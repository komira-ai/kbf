//! The input cache across daemon runs (`docs/design/worker-cache.md#68-crash-and-restart`
//! and `#69-the-index`): the snapshot loaded at start, entries re-hashed on first use,
//! the drain to a lowered budget, a bad snapshot discarded, files the index does not
//! list adopted, and the slot races between fills, the evictor and the adopt pass.
//! Each "run" is a cache opened on the same directory after the last one stopped.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::support::cache_kit::{
    CountingCas, FakeClock, Gates, config, digest, entries, entry_path, file, key, lease,
    on_disk_sum, stop, toolchain, wait_for_fill_waits,
};
use crate::support::scratch;
use kbf_daemon::cache::{Cache, CacheConfig, Discarded, FIXED_MTIME, Key, Point};
use kbf_testkit::forked::ForkedChild;

const GIB: u64 = 1 << 30;

/// The snapshot `v1/index.snap` of the cache in `dir/cache`, decoded here from its
/// documented format: each key with its last use.
fn snapshot(dir: &Path) -> BTreeMap<Key, u64> {
    let bytes = std::fs::read(dir.join("cache/v1/index.snap")).expect("a snapshot");
    assert_eq!(&bytes[..8], b"KBFCIDX\0");
    let count = u64::from_le_bytes(bytes[16..24].try_into().expect("count"));
    let body = &bytes[24..bytes.len() - 4];
    assert_eq!(body.len() as u64, count * 56);
    body.chunks_exact(56)
        .map(|e| {
            let mut hash = [0; 32];
            hash.copy_from_slice(&e[8..40]);
            let size = u64::from_le_bytes(e[40..48].try_into().expect("size"));
            let key = Key {
                hash,
                size,
                executable: e[0] & 1 == 1,
            };
            (
                key,
                u64::from_le_bytes(e[48..56].try_into().expect("last use")),
            )
        })
        .collect()
}

/// Rewrites the snapshot's CRC over what is before it.
fn recrc(bytes: &mut [u8]) {
    let at = bytes.len() - 4;
    let crc = crc32fast::hash(&bytes[..at]);
    bytes[at..].copy_from_slice(&crc.to_le_bytes());
}

/// Waits up to 30 seconds for `done`.
fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "{what}: not within 30 s");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Opens a run whose first adopt pass is held until the returned hold is let go.
fn paused(
    config: CacheConfig,
    gates: &Arc<Gates>,
) -> (Arc<Cache>, crate::support::cache_kit::HoldId) {
    let pass = gates.hold(|p| *p == Point::PassStarts);
    let cache = Cache::open(config).expect("open");
    gates.wait_reached(pass);
    (cache, pass)
}

/// Lets the held first pass run and waits until it has saved its snapshot.
fn finish_pass(cache: &Cache, gates: &Gates, pass: crate::support::cache_kit::HoldId) {
    let before = cache.stats().snapshots;
    gates.release(pass);
    eventually("the first pass", || cache.stats().snapshots > before);
}

/// R-2. Catches a cache that does not survive a restart (the "wipe `v1/` at start"
/// defect), a clean stop that saves no snapshot (the "skip the snapshot at a clean
/// stop" defect: the post-pass one is never written in run 1 either), and a start that
/// ignores the snapshot (whose fills would adopt the files: no bytes either, so the
/// misses tell). The first lease of run 2, with the pass held, fetches nothing, every
/// file is a hit from the loaded index, and each entry is re-hashed once; a second
/// lease re-hashes none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_serves_from_the_loaded_index_and_re_hashes_each_entry_once() {
    let dir = scratch("cache-r2");
    let cas = CountingCas::default();
    let tree = toolchain(2000);
    let root = cas.tree(&tree);
    let gates = Gates::running();
    let (cache, _) = paused(
        config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000)),
        &gates,
    );
    lease(&cas, &cache, &root, &dir, "run1")
        .await
        .expect("run 1");
    stop(cache, &gates);
    let fetched = cas.file_bytes();
    let gates = Gates::running();
    let (cache, pass) = paused(
        config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(2000)),
        &gates,
    );
    assert_eq!(cache.stats().index_discarded, None);
    let (_, first) = lease(&cas, &cache, &root, &dir, "run2-a")
        .await
        .expect("run 2");
    assert_eq!(cas.file_bytes(), fetched, "run 2 fetched");
    assert_eq!(first.cache_misses, 0, "a file was not in the loaded index");
    assert_eq!(first.cache_hits, first.files - first.empty_files);
    assert_eq!(first.verify_hashes, first.files - first.empty_files);
    let (_, second) = lease(&cas, &cache, &root, &dir, "run2-b")
        .await
        .expect("run 2 again");
    assert_eq!(
        second.verify_hashes, 0,
        "an entry was re-hashed twice in one run"
    );
    gates.release(pass);
    stop(cache, &gates);
}

/// R-3. Catches an entry changed while the daemon was down being served (the "mark
/// every entry verified at load" defect): one byte flipped, with the size, mode and
/// mtime restored. Its first use after the restart re-hashes it, evicts it and fetches
/// it again, and counts one verify failure.
#[tokio::test]
async fn an_entry_changed_while_stopped_is_fetched_again() {
    let dir = scratch("cache-r3");
    let cas = CountingCas::default();
    let root = cas.tree(&[file("a", b"right bytes", false), file("b", b"other", false)]);
    let gates = Gates::running();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    lease(&cas, &cache, &root, &dir, "run1")
        .await
        .expect("run 1");
    stop(cache, &gates);
    let entry = entry_path(&dir, &key(b"right bytes", false));
    std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    std::fs::write(&entry, b"wrong bytes").expect("flip");
    std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o444)).expect("chmod back");
    let fixed = std::fs::FileTimes::new()
        .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(FIXED_MTIME as u64));
    std::fs::File::open(&entry)
        .expect("open")
        .set_times(fixed)
        .expect("mtime back");
    let gates = Gates::running();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(2000))).expect("open");
    let (at, stats) = lease(&cas, &cache, &root, &dir, "run2")
        .await
        .expect("run 2");
    assert_eq!(std::fs::read(at.join("a")).expect("read"), b"right bytes");
    assert_eq!(cas.reads_of(&digest(b"right bytes")), 2);
    assert_eq!(cas.reads_of(&digest(b"other")), 1);
    assert_eq!(cache.stats().verify_failures, 1);
    assert_eq!(stats.verify_hashes, 1, "only the good entry hashed right");
    stop(cache, &gates);
}

/// Run 1 of R-4 in `dir`: keys K1..K6 inserted in order, then used in the order K3, K1,
/// K5, K2, K6, K4, one fake minute apart, and a clean stop. Returns the keys.
async fn lru_run(dir: &Path, cas: &CountingCas) -> Vec<Key> {
    let clock = FakeClock::at(10_000);
    let gates = Gates::running();
    let cache = Cache::open(config(dir, GIB, Arc::clone(&gates), &clock)).expect("open");
    let blobs: Vec<Vec<u8>> = (1..=6_u8).map(|i| vec![i; 100]).collect();
    for (i, blob) in blobs.iter().enumerate() {
        let root = cas.tree(&[file("f", blob, false)]);
        lease(cas, &cache, &root, dir, &format!("insert{i}"))
            .await
            .expect("insert");
        clock.advance(1);
    }
    for k in [3, 1, 5, 2, 6, 4] {
        clock.advance(60);
        let root = cas.tree(&[file("f", &blobs[k - 1], false)]);
        lease(cas, &cache, &root, dir, &format!("use{k}"))
            .await
            .expect("use");
    }
    stop(cache, &gates);
    blobs.iter().map(|b| key(b, false)).collect()
}

/// R-4. Catches a size limit enforced only on insert (the "skip the at-load eviction"
/// defect), and recency lost across a restart (the "write insert time instead of last
/// use" and "update last use only on insert" defects): run 2, with a budget of three
/// entries and no lease, drains exactly K3, K1 and K5, the least recently used in run
/// 1's order, after loading run 1's last uses.
#[tokio::test]
async fn the_drain_at_start_removes_the_least_recently_used() {
    let dir = scratch("cache-r4");
    let cas = CountingCas::default();
    let keys = lru_run(&dir, &cas).await;
    let saved = snapshot(&dir);
    let gates = Gates::running();
    let (cache, pass) = paused(
        config(&dir, 300, Arc::clone(&gates), &FakeClock::at(99_999)),
        &gates,
    );
    assert_eq!(
        snapshot(&dir),
        saved,
        "the last uses loaded are not run 1's"
    );
    for (k, held) in keys.iter().zip([false, true, false, true, false, true]) {
        assert_eq!(cache.holds(k), held, "K{}", k.hash[0] % 7);
    }
    assert_eq!(cache.stats().bytes, 300);
    assert_eq!(cache.stats().bytes, on_disk_sum(&dir));
    gates.release(pass);
    stop(cache, &gates);
}

/// R-4, run 2b. Catches a drain that removes keys from the index outside their slots
/// (the "remove every drained key first, then unlink each under its slot" defect): the
/// drain is held inside K1's slot (removed from the index, not yet unlinked), K3 is
/// already drained, K5 not reached. A lease needing K3, K1 and K5 refetches K3, waits
/// on K1's slot and refetches it once the drain lets go, and hits K5, which its pin
/// keeps from the drain. Afterwards every indexed key's file exists and the total is
/// the on-disk sum.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_during_the_drain_waits_on_the_slot_it_holds() {
    let dir = scratch("cache-r4b");
    let cas = Arc::new(CountingCas::default());
    let keys = lru_run(&dir, &cas).await;
    let run2 = Arc::new(CountingCas::default());
    let blobs: Vec<Vec<u8>> = (1..=6_u8).map(|i| vec![i; 100]).collect();
    let root = run2.tree(&[
        file("k3", &blobs[2], false),
        file("k1", &blobs[0], false),
        file("k5", &blobs[4], false),
    ]);
    let gates = Gates::running();
    let k1 = keys[0];
    let drain = gates.hold(move |p| *p == Point::Evicting(&k1));
    let pass = gates.hold(|p| *p == Point::PassStarts);
    let cache = Cache::open(config(
        &dir,
        300,
        Arc::clone(&gates),
        &FakeClock::at(99_999),
    ))
    .expect("open");
    let waiting = Arc::clone(&gates);
    tokio::task::spawn_blocking(move || waiting.wait_reached(drain))
        .await
        .expect("held");
    assert!(!cache.holds(&keys[2]) && !cache.holds(&keys[0]) && cache.holds(&keys[4]));
    let writing = {
        let (cas, cache, root, dir) = (
            Arc::clone(&run2),
            Arc::clone(&cache),
            root.clone(),
            dir.clone(),
        );
        tokio::spawn(async move { lease(&cas, &cache, &root, &dir, "during").await })
    };
    // The "during" lease waits on the slot the drain holds.
    wait_for_fill_waits(&cache, 1).await;
    assert_eq!(
        run2.reads_of(&digest(&blobs[0])),
        0,
        "K1 was fetched while the drain held it"
    );
    gates.release(drain);
    let (at, _) = writing.await.expect("joined").expect("the lease");
    assert_eq!(std::fs::read(at.join("k1")).expect("read"), blobs[0]);
    assert_eq!(run2.reads_of(&digest(&blobs[2])), 1);
    assert_eq!(run2.reads_of(&digest(&blobs[0])), 1);
    assert_eq!(
        run2.reads_of(&digest(&blobs[4])),
        0,
        "K5 was drained under its pin"
    );
    gates.wait_reached(pass);
    for k in &keys {
        assert_eq!(
            cache.holds(k),
            entry_path(&dir, k).exists(),
            "K{}",
            k.hash[0]
        );
    }
    assert_eq!(cache.stats().bytes, on_disk_sum(&dir));
    gates.release(pass);
    stop(cache, &gates);
}

/// Fills `dir`'s cache with `count` files and stops cleanly; returns the root.
async fn filled(dir: &Path, cas: &CountingCas, count: usize) -> kbf_proto::reapi::Digest {
    let root = cas.tree(&toolchain(count));
    let gates = Gates::running();
    let (cache, _) = paused(
        config(dir, GIB, Arc::clone(&gates), &FakeClock::at(1000)),
        &gates,
    );
    lease(cas, &cache, &root, dir, "fill").await.expect("fill");
    stop(cache, &gates);
    root
}

/// R-6. Catches trusting a snapshot that fails its CRC (the "skip the CRC check"
/// defect), a bad snapshot stopping the start or the cache (the "refuse to start" and
/// "disable the cache" defects), and a pass that deletes what the index does not list
/// (the "make the pass unlink unknown files" defect). One byte of one entry's last use
/// is flipped: the run starts empty, its lease's keys are adopted by their fills with
/// nothing fetched, and one pass indexes every file, unlinking none.
#[tokio::test]
async fn a_corrupt_snapshot_is_discarded_and_its_files_adopted() {
    let dir = scratch("cache-r6");
    let cas = CountingCas::default();
    filled(&dir, &cas, 60).await;
    // The run-2 lease needs half the files: the pass must adopt the rest.
    let half = cas.tree(&toolchain(60)[..30]);
    let snap = dir.join("cache/v1/index.snap");
    let mut bytes = std::fs::read(&snap).expect("snapshot");
    bytes[24 + 56 + 50] ^= 1;
    std::fs::write(&snap, &bytes).expect("corrupt");
    let before = entries(&dir);
    let fetched = cas.file_bytes();
    let gates = Gates::running();
    let (cache, pass) = paused(
        config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(2000)),
        &gates,
    );
    assert_eq!(cache.stats().index_discarded, Some(Discarded::Corrupt));
    assert_eq!(cache.stats().bytes, 0);
    let (_, stats) = lease(&cas, &cache, &half, &dir, "run2")
        .await
        .expect("run 2");
    assert_eq!(cas.file_bytes(), fetched);
    assert_eq!(stats.cache_misses, stats.files - stats.empty_files);
    finish_pass(&cache, &gates, pass);
    assert!(
        cache.stats().adopted_entries > 0,
        "the pass adopted nothing"
    );
    assert_eq!(entries(&dir), before, "a file was unlinked or replaced");
    assert_eq!(cache.stats().entries as usize, before.len());
    assert_eq!(cache.stats().bytes, on_disk_sum(&dir));
    stop(cache, &gates);
}

/// R-6. Catches a snapshot of another format version read as this one (with its CRC
/// recomputed, only the version check refuses it), and a total left over the budget
/// once the pass has adopted the files: LRU eviction follows the pass.
#[tokio::test]
async fn a_snapshot_of_another_version_is_discarded_and_the_budget_holds_after_the_pass() {
    let dir = scratch("cache-r6-version");
    let cas = CountingCas::default();
    filled(&dir, &cas, 60).await;
    let snap = dir.join("cache/v1/index.snap");
    let mut bytes = std::fs::read(&snap).expect("snapshot");
    bytes[8] = 2;
    recrc(&mut bytes);
    std::fs::write(&snap, &bytes).expect("version 2");
    let sum = on_disk_sum(&dir);
    let gates = Gates::running();
    let (cache, pass) = paused(
        config(&dir, sum / 2, Arc::clone(&gates), &FakeClock::at(2000)),
        &gates,
    );
    assert_eq!(cache.stats().index_discarded, Some(Discarded::Version));
    finish_pass(&cache, &gates, pass);
    assert!(cache.stats().bytes <= sum / 2);
    assert_eq!(cache.stats().bytes, on_disk_sum(&dir));
    stop(cache, &gates);
}

/// R-7. Catches a short or padded snapshot read as entries (the "read entries until EOF
/// without the length check" defect), a short read that crashes the start (the "unwrap
/// the short read" defect), and a key counted twice (the "skip the duplicate-key check"
/// defect). Each malformed snapshot is discarded as corrupt, the run starts with a
/// total of 0, and the pass adopts every file once. The count and duplicate forgeries
/// carry a valid CRC.
#[tokio::test]
async fn malformed_snapshots_are_discarded() {
    let dir = scratch("cache-r7");
    let cas = CountingCas::default();
    filled(&dir, &cas, 20).await;
    let snap = dir.join("cache/v1/index.snap");
    let good = std::fs::read(&snap).expect("snapshot");
    let files = entries(&dir).len() as u64;
    let mut cut_header = good.clone();
    cut_header.truncate(10);
    // The header alone, with a CRC that matches it: only the length check refuses it.
    let mut header_only = good[..16].to_vec();
    header_only.extend_from_slice(&[0; 4]);
    recrc(&mut header_only);
    let mut cut_entry = good.clone();
    cut_entry.drain(good.len() - 20..good.len() - 4);
    let mut padded = good.clone();
    padded.push(0);
    let mut counted = good.clone();
    counted[16] += 1;
    recrc(&mut counted);
    let mut twice = good.clone();
    let second = 24 + 56;
    let first: Vec<u8> = twice[24..24 + 56].to_vec();
    twice[second..second + 56].copy_from_slice(&first);
    recrc(&mut twice);
    for (name, bytes) in [
        ("cut header", cut_header),
        ("header only", header_only),
        ("cut entry", cut_entry),
        ("padded", padded),
        ("count", counted),
        ("twice", twice),
    ] {
        std::fs::write(&snap, &bytes).expect("forge");
        let gates = Gates::running();
        let (cache, pass) = paused(
            config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(2000)),
            &gates,
        );
        assert_eq!(
            cache.stats().index_discarded,
            Some(Discarded::Corrupt),
            "{name}"
        );
        assert_eq!(cache.stats().bytes, 0, "{name}");
        finish_pass(&cache, &gates, pass);
        assert_eq!(cache.stats().adopted_entries, files, "{name}");
        stop(cache, &gates);
    }
}

/// R-8. Catches an indexed file that is gone failing a lease, and drift never
/// corrected (the "make the pass only adopt" defect): two entries deleted while the
/// daemon is stopped; a lease needing one fetches it again, and one pass drops the
/// other.
#[tokio::test]
async fn entries_deleted_while_stopped_are_refetched_or_dropped() {
    let dir = scratch("cache-r8");
    let cas = CountingCas::default();
    let root = cas.tree(&[
        file("a", b"one", false),
        file("b", b"two", false),
        file("c", b"three", false),
    ]);
    let used = cas.tree(&[file("a", b"one", false)]);
    let gates = Gates::running();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    lease(&cas, &cache, &root, &dir, "run1")
        .await
        .expect("run 1");
    stop(cache, &gates);
    for bytes in [&b"one"[..], b"two"] {
        std::fs::remove_file(entry_path(&dir, &key(bytes, false))).expect("delete");
    }
    let gates = Gates::running();
    let (cache, pass) = paused(
        config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(2000)),
        &gates,
    );
    let (at, _) = lease(&cas, &cache, &used, &dir, "run2")
        .await
        .expect("run 2");
    assert_eq!(std::fs::read(at.join("a")).expect("read"), b"one");
    assert_eq!(cas.reads_of(&digest(b"one")), 2);
    finish_pass(&cache, &gates, pass);
    assert!(
        !cache.holds(&key(b"two", false)),
        "the pass kept an entry with no file"
    );
    assert_eq!(cache.stats().dropped_entries, 1);
    assert_eq!(cache.stats().bytes, on_disk_sum(&dir));
    stop(cache, &gates);
}

/// R-9 (b). Catches the pass dropping an entry on a stale listing (the "drop an entry
/// without the `fstatat`" defect): the pass lists K's directory, a fill then inserts
/// K, and the pass, comparing, keeps it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pass_keeps_an_entry_inserted_after_its_listing() {
    let dir = scratch("cache-r9b");
    let cas = CountingCas::default();
    let gates = Gates::running();
    let (cache, pass) = paused(
        config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000)),
        &gates,
    );
    let k = key(b"inserted late", false);
    let prefix = k.hash[0];
    let listed = gates.hold(move |p| *p == Point::Listed(prefix));
    let passing = {
        let cache = Arc::clone(&cache);
        std::thread::spawn(move || cache.adopt_pass())
    };
    let waiting = Arc::clone(&gates);
    tokio::task::spawn_blocking(move || waiting.wait_reached(listed))
        .await
        .expect("listed");
    lease(
        &cas,
        &cache,
        &cas.tree(&[file("f", b"inserted late", false)]),
        &dir,
        "late",
    )
    .await
    .expect("late");
    gates.release(listed);
    passing.join().expect("the pass");
    assert!(
        cache.holds(&k),
        "the pass dropped an entry whose file exists"
    );
    assert_eq!(cache.stats().dropped_entries, 0);
    assert_eq!(cache.stats().bytes, on_disk_sum(&dir));
    gates.release(pass);
    stop(cache, &gates);
}

/// R-9 (c). Catches the pass adopting on its listing's view (the "adopt without
/// re-checking the index under the slot" defect): K is on disk and not indexed; the
/// pass compares and is held before it takes K's slot; a fill adopts K meanwhile; the
/// pass then finds K indexed and leaves it, so K is adopted and counted once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pass_re_checks_the_index_under_the_slot() {
    let dir = scratch("cache-r9c");
    let cas = CountingCas::default();
    let root = cas.tree(&[file("f", b"on disk", false)]);
    let gates = Gates::running();
    let cache =
        Cache::open(config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    lease(&cas, &cache, &root, &dir, "run1")
        .await
        .expect("run 1");
    stop(cache, &gates);
    std::fs::remove_file(dir.join("cache/v1/index.snap")).expect("lose the index");
    let gates = Gates::running();
    let (cache, pass) = paused(
        config(&dir, GIB, Arc::clone(&gates), &FakeClock::at(2000)),
        &gates,
    );
    let k = key(b"on disk", false);
    let compared = gates.hold(move |p| *p == Point::Compared(&k));
    gates.release(pass);
    let waiting = Arc::clone(&gates);
    tokio::task::spawn_blocking(move || waiting.wait_reached(compared))
        .await
        .expect("compared");
    let (_, stats) = lease(&cas, &cache, &root, &dir, "run2")
        .await
        .expect("run 2");
    assert_eq!(
        (stats.cache_misses, stats.verify_hashes),
        (1, 1),
        "the fill did not adopt K"
    );
    gates.release(compared);
    eventually("the pass", || cache.stats().snapshots >= 2);
    assert_eq!(cache.stats().adopted_entries, 0, "the pass adopted K too");
    assert_eq!(cache.stats().bytes, k.size);
    assert_eq!(cache.stats().index_drift_bytes, 0);
    stop(cache, &gates);
}

/// R-9, the pass's side of a held slot. Catches the adopt pass changing a key whose
/// slot someone holds (the "drop the held-slot check" defect): the evictor is held
/// between removing K from the index and unlinking it, so K's file is on disk and not
/// indexed; a pass meanwhile leaves K alone, the evictor unlinks it, and K is neither
/// indexed nor counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn the_pass_leaves_a_key_whose_slot_is_held() {
    let dir = scratch("cache-r9-held");
    let gates = Gates::new();
    let cache =
        Cache::open(config(&dir, 100, Arc::clone(&gates), &FakeClock::at(1000))).expect("open");
    let cas = Arc::new(CountingCas::default());
    let k = key(&[b'k'; 100], false);
    lease(
        &cas,
        &cache,
        &cas.tree(&[file("f", &[b'k'; 100], false)]),
        &dir,
        "k",
    )
    .await
    .expect("k");
    let held = gates.hold(move |p| *p == Point::Evicting(&k));
    let evicting = {
        let (cas, cache, dir) = (Arc::clone(&cas), Arc::clone(&cache), dir.clone());
        let root = cas.tree(&[file("f", &[b'l'; 100], false)]);
        tokio::spawn(async move { lease(&cas, &cache, &root, &dir, "l").await })
    };
    let waiting = Arc::clone(&gates);
    tokio::task::spawn_blocking(move || waiting.wait_reached(held))
        .await
        .expect("held");
    assert!(entry_path(&dir, &k).exists() && !cache.holds(&k));
    let passing = Arc::clone(&cache);
    tokio::task::spawn_blocking(move || passing.adopt_pass())
        .await
        .expect("the pass");
    gates.release(held);
    evicting.await.expect("joined").expect("l");
    assert!(!cache.holds(&k), "the pass adopted a key the evictor held");
    assert!(!entry_path(&dir, &k).exists());
    assert_eq!(cache.stats().adopted_entries, 0);
    assert_eq!(cache.stats().bytes, on_disk_sum(&dir));
    stop(cache, &gates);
}

/// Catches a cache directory still locked after the cache is stopped and dropped, when a
/// process forked while it was open has not exec'd: the lock was released only by closing
/// its descriptor, and a child holds a copy of that until its exec. The daemon starts an
/// action per lease, and the tests of one binary run on threads of one process, so a
/// restart in such a process was refused as `locked` (the daemon would then run
/// uncached). The child here is held before exec until the reopen has been made, so the
/// interleaving is the failing one every time.
///
/// Catches: the `Drop` of `DirLock` removed.
#[test]
fn a_child_forked_while_the_cache_was_open_does_not_keep_the_directory_locked() {
    let dir = scratch("cache-forked-lock");
    let gates = Gates::new();
    let clock = FakeClock::at(1000);
    let cache = Cache::open(config(&dir, GIB, Arc::clone(&gates), &clock)).expect("open");
    let child = ForkedChild::before_exec();
    stop(cache, &gates);
    let again = Cache::open(config(&dir, GIB, Arc::clone(&gates), &clock));
    let again = again.expect("an open after the stop, with the forked child still alive");
    stop(again, &gates);
    child.finish();
}
