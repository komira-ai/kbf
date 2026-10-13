//! Measurements, not checks: the index at one million entries
//! (`docs/design/worker-cache.md#68-crash-and-restart` and `#69-the-index`). Each prints
//! its numbers and asserts nothing about them, so it is ignored and off the gate. Run by
//! hand, in release as the daemon ships, on an otherwise idle machine:
//!
//! `cargo test -p kbf-daemon --test cache --release -- --ignored --nocapture measure`

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::support::cache_kit::{FakeClock, Gates, config, stop};
use crate::support::scratch;
use kbf_daemon::cache::{Cache, CacheConfig, Key};

const ENTRIES: u64 = 1_000_000;
const RUNS: usize = 3;

/// `ENTRIES` distinct keys of 1 KiB to 1 MiB, in key order, spread over the 256
/// prefixes.
fn keys() -> Vec<Key> {
    let mut keys: Vec<_> = (0..ENTRIES)
        .map(|i| {
            let mut hash = [0; 32];
            hash[..8].copy_from_slice(&i.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes());
            hash[8..16].copy_from_slice(&i.to_le_bytes());
            Key {
                hash,
                size: 1024 << (i % 11),
                executable: i % 20 == 0,
            }
        })
        .collect();
    keys.sort();
    keys
}

/// The snapshot of `keys` in its documented format, last uses one second apart.
fn snapshot(keys: &[Key]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(28 + keys.len() * 56);
    bytes.extend_from_slice(b"KBFCIDX\0");
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(&(keys.len() as u64).to_le_bytes());
    for (n, key) in (0u64..).zip(keys) {
        bytes.push(u8::from(key.executable));
        bytes.extend_from_slice(&[0; 7]);
        bytes.extend_from_slice(&key.hash);
        bytes.extend_from_slice(&key.size.to_le_bytes());
        bytes.extend_from_slice(&(1000 + n).to_le_bytes());
    }
    let crc = crc32fast::hash(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    bytes
}

/// A cache in `dir` with no budget pressure and no timed snapshot during a run; its
/// first adopt pass is held (`Gates::new`), so the index stays as loaded.
fn quiet(dir: &Path, gates: &Arc<Gates>) -> CacheConfig {
    let mut config = config(dir, u64::MAX, Arc::clone(gates), &FakeClock::at(1000));
    config.snapshot_every = Duration::from_secs(3600);
    config
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Start-up's load (`cache_index_load_ms`: read, decode, build the index) and the index
/// lock's hold while a snapshot copies the entries. The hold is measured as a lease
/// sees it: the longest a call that takes the lock (`Cache::stats`) waited, polled
/// without pause while snapshots are written beside it.
#[test]
#[ignore = "a measurement at 1M entries, run by hand (see the module doc)"]
fn measure_the_index_at_one_million_entries() {
    let dir = scratch("cache-measure");
    let gates = Gates::new();
    stop(Cache::open(quiet(&dir, &gates)).expect("lay out"), &gates);
    let bytes = snapshot(&keys());
    println!("measure: entries={ENTRIES} snapshot_bytes={}", bytes.len());
    for run in 0..RUNS {
        std::fs::write(dir.join("cache/v1/index.snap"), &bytes).expect("plant the snapshot");
        let gates = Gates::new();
        let opened = Instant::now();
        let cache = Cache::open(quiet(&dir, &gates)).expect("open");
        let open_ms = ms(opened.elapsed());
        let stats = cache.stats();
        assert_eq!(
            stats.entries, ENTRIES,
            "the planted snapshot was not loaded"
        );
        let writing = AtomicBool::new(true);
        let (writes, longest_wait) = std::thread::scope(|s| {
            let writer = s.spawn(|| {
                let writes: Vec<_> = (0..5)
                    .map(|_| {
                        let started = Instant::now();
                        cache.write_snapshot().expect("a snapshot");
                        ms(started.elapsed())
                    })
                    .collect();
                writing.store(false, Ordering::Relaxed);
                writes
            });
            let mut longest = Duration::ZERO;
            while writing.load(Ordering::Relaxed) {
                let started = Instant::now();
                let _ = cache.stats();
                longest = longest.max(started.elapsed());
            }
            (writer.join().expect("writer"), longest)
        });
        let writes = writes.iter().map(|w| format!("{w:.0}")).collect::<Vec<_>>();
        println!(
            "measure: run={run} cache_index_load_ms={} open_ms={open_ms:.0} \
             longest_lock_wait_ms={:.1} snapshot_write_ms={}",
            stats.index_load_ms,
            ms(longest_wait),
            writes.join(","),
        );
        stop(cache, &gates);
    }
}
