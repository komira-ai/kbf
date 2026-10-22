//! The RAM budget of the held set and its metrics (`docs/design/storage-ram-ack.md`, rule
//! 3). These tests call `Cache::hold`, the budgeted primitive under the producer
//! (`ram_ack_cache.rs` and `ram_ack_chain.rs` run the producer), over a cache with no tier (a drain pass then writes to the store).
//!
//! The budget is a non-blocking try-acquire: a blob it cannot take is not held, the caller
//! writes it through, and `writethrough{reason=budget}` counts it. No test waits on a
//! budget, and the ages are read on tokio's paused clock, so none sleeps or races.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kbf_front::{Cache, Hold, MemoryMetaLog, VerifiedBlob, WriteThrough};
use kbf_objstore::MemoryStore;
use kbf_types::Digest;
use tokio::time::Instant;

type MemCache = Cache<MemoryMetaLog, MemoryStore>;

/// A blob of `len` bytes, different for each `seed`.
fn blob(seed: u8, len: usize) -> VerifiedBlob {
    let bytes: Vec<u8> = (0..len)
        .map(|i| (i * 31 % 251) as u8 ^ seed.wrapping_mul(37))
        .collect();
    VerifiedBlob::hashed(Bytes::from(bytes))
}

/// The value of the sample `name` (with `labels`, e.g. `{reason="budget"}`) in a scrape.
fn metric(text: &str, name: &str, labels: &str) -> f64 {
    let key = format!("{name}{labels} ");
    text.lines()
        .find_map(|l| l.strip_prefix(&key))
        .unwrap_or_else(|| panic!("no sample {key} in:\n{text}"))
        .trim()
        .parse()
        .expect("a number")
}

fn gauge(cache: &MemCache, name: &str) -> f64 {
    metric(&cache.render_metrics(), name, "")
}

fn with_budget(bytes: u64) -> MemCache {
    Cache::memory().with_ram_ack_bytes(bytes)
}

fn digest(blob: &VerifiedBlob) -> Digest {
    blob.digest()
}

/// Catches a budget that takes more than it has (mutant: `try_acquire` ignores the cap):
/// with room for two blobs of 100 bytes the third is `Budget`, not held, not readable as
/// present, counted as `writethrough{budget}` once, and the bytes held are exactly 200
/// (invariant I4). The caller then writes the third through (`store_blobs`) and it is
/// present and durable: a full budget pushes back, it does not lose the write.
#[tokio::test]
async fn a_full_budget_refuses_and_the_write_goes_through() {
    let cache = with_budget(200);
    let (a, b, c) = (blob(1, 100), blob(2, 100), blob(3, 100));
    assert_eq!(cache.hold(&a), Hold::Held);
    assert_eq!(cache.hold(&b), Hold::Held);
    assert_eq!(cache.hold(&c), Hold::Budget, "the third does not fit");
    assert_eq!(
        cache.find_missing(&[digest(&c)]).await.expect("found"),
        [digest(&c)],
        "a refused blob is not held, so it is not present"
    );
    assert_eq!(cache.unwritten().bytes, 200, "never over the budget");
    let text = cache.render_metrics();
    assert_eq!(
        metric(
            &text,
            "kbf_storage_ram_writethrough_total",
            "{reason=\"budget\"}"
        ),
        1.0
    );

    cache
        .store_blobs(vec![c.clone()])
        .await
        .expect("written through");
    assert!(
        cache
            .durable(&[digest(&c)])
            .await
            .expect("asked")
            .is_empty(),
        "the refused write is on disk"
    );
}

/// Catches a default that holds blobs (the flag is 0, off, until the producer ships), and a
/// refusal that is not counted: every blob but the empty one (never held, always present)
/// is refused and counted.
#[tokio::test]
async fn a_budget_of_zero_holds_nothing() {
    let cache = Cache::memory();
    assert_eq!(cache.hold(&blob(1, 10)), Hold::Budget);
    assert_eq!(
        cache.hold(&VerifiedBlob::hashed(Bytes::new())),
        Hold::Duplicate
    );
    assert_eq!(cache.unwritten().blobs, 0);
    let text = cache.render_metrics();
    assert_eq!(
        metric(
            &text,
            "kbf_storage_ram_writethrough_total",
            "{reason=\"budget\"}"
        ),
        1.0
    );
    assert_eq!(metric(&text, "kbf_storage_ram_budget_bytes", ""), 0.0);
}

/// A blob larger than the whole budget is refused at once, and a blob of exactly the
/// budget fits (the bound is `<=`). Catches an off-by-one in the cap comparison.
#[tokio::test]
async fn the_bound_is_the_whole_budget_inclusive() {
    let cache = with_budget(100);
    assert_eq!(cache.hold(&blob(1, 101)), Hold::Budget);
    assert_eq!(cache.hold(&blob(2, 100)), Hold::Held);
    assert_eq!(cache.hold(&blob(3, 1)), Hold::Budget, "nothing is left");
}

/// Catches a permit that is not returned when the blob is drained (mutant: `Permit::drop`
/// does nothing) and one that is returned twice or early: with room for one blob, a second
/// writer of the same digest is a dedup answer and takes no second permit, the drain frees
/// the one permit, and then a different blob fits.
#[tokio::test]
async fn a_drain_returns_the_budget_once() {
    let cache = with_budget(100);
    let (a, b) = (blob(1, 100), blob(2, 100));
    assert_eq!(cache.hold(&a), Hold::Held);
    assert_eq!(
        cache.hold(&a),
        Hold::Duplicate,
        "a held digest needs no permit, even with the budget full"
    );
    assert_eq!(cache.hold(&b), Hold::Budget, "the budget is full");

    let report = cache.drain_held(Instant::now()).await;
    assert_eq!((report.claimed, report.written, report.failed), (1, 1, 0));
    assert_eq!(cache.unwritten().bytes, 0);
    assert_eq!(cache.hold(&b), Hold::Held, "the drain gave the budget back");
    assert_eq!(
        cache.hold(&a),
        Hold::Budget,
        "and only once: b holds all of it"
    );
}

/// With 16 writers racing for room for 5 blobs, exactly 5 are held, 11 are refused and
/// counted, and the bytes held are exactly the budget whatever the interleaving (I4).
/// Catches a check-then-add that two writers can both pass (mutant: the cap is not part
/// of the atomic update).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_writers_never_exceed_the_budget() {
    let cache = Arc::new(with_budget(500));
    let tasks: Vec<_> = (0..16u8)
        .map(|seed| {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move { cache.hold(&blob(seed, 100)) })
        })
        .collect();
    let mut outcomes = Vec::new();
    for task in tasks {
        outcomes.push(task.await.expect("join"));
    }
    let held = outcomes.iter().filter(|o| **o == Hold::Held).count();
    assert_eq!(held, 5);
    assert_eq!(outcomes.iter().filter(|o| **o == Hold::Budget).count(), 11);
    assert_eq!(cache.unwritten().bytes, 500);
    let text = cache.render_metrics();
    assert_eq!(
        metric(
            &text,
            "kbf_storage_ram_writethrough_total",
            "{reason=\"budget\"}"
        ),
        11.0
    );
}

/// Each gauge's value at each step (the doc's names): empty, two blobs held at different
/// times, the oldest's age, the oldest leaving, and the drain's end (`ram_drain.rs` reads
/// the failure counter). Ages are read on the paused clock, so they are exact. Catches a
/// gauge that is not updated when the set changes, an oldest age taken from the youngest
/// blob (mutant: `min` for `max`), `writethrough` reasons that are not all printed, and a
/// budget gauge that reads something else.
#[tokio::test(start_paused = true)]
async fn the_metrics_read_the_set_at_every_step() {
    let cache = with_budget(1000);
    let (a, b) = (blob(1, 100), blob(2, 250));
    let bytes = "kbf_storage_ram_unwritten_bytes";
    let blobs = "kbf_storage_ram_unwritten_blobs";
    let oldest = "kbf_storage_ram_unwritten_oldest_seconds";
    let failures = "kbf_storage_ram_drain_failures_total";

    assert_eq!(gauge(&cache, "kbf_storage_ram_budget_bytes"), 1000.0);
    for name in [bytes, blobs, oldest, failures] {
        assert_eq!(gauge(&cache, name), 0.0, "{name} starts at 0");
    }
    let text = cache.render_metrics();
    for reason in [
        WriteThrough::ReplicaLost,
        WriteThrough::SingleOwner,
        WriteThrough::Large,
        WriteThrough::QueueFull,
        WriteThrough::Budget,
        WriteThrough::StorePath,
    ] {
        let labels = format!("{{reason=\"{}\"}}", reason.label());
        assert_eq!(
            metric(&text, "kbf_storage_ram_writethrough_total", &labels),
            0.0
        );
    }

    assert_eq!(cache.hold(&a), Hold::Held);
    tokio::time::advance(Duration::from_secs(3)).await;
    assert_eq!(cache.hold(&b), Hold::Held);
    tokio::time::advance(Duration::from_secs(7)).await;
    assert_eq!(gauge(&cache, bytes), 350.0);
    assert_eq!(gauge(&cache, blobs), 2.0);
    assert_eq!(gauge(&cache, oldest), 10.0, "a was held 10 s ago");

    assert!(cache.release_for_test(&digest(&a)));
    assert_eq!(gauge(&cache, bytes), 250.0);
    assert_eq!(gauge(&cache, blobs), 1.0);
    assert_eq!(gauge(&cache, oldest), 7.0, "now b is the oldest");

    // The drain's end: the blob is in the index and every gauge is back to 0.
    let report = cache.drain_held(Instant::now()).await;
    assert_eq!(
        (report.written, report.failed),
        (1, 0),
        "the memory store takes it"
    );
    assert_eq!(gauge(&cache, bytes), 0.0);
    assert_eq!(gauge(&cache, blobs), 0.0);
    assert_eq!(gauge(&cache, oldest), 0.0);
    assert_eq!(gauge(&cache, failures), 0.0);
}
