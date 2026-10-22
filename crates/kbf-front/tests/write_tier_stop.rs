//! The write tier's flush at a stop (`Cache::flush_for_shutdown`): what a node does with
//! the blocks it still holds once its calls have ended. Every test runs the same [`Farm`]
//! as `write_tier.rs`, and its comment names the defect it is there to catch.

mod common;
#[allow(dead_code)]
#[path = "write_tier/support.rs"]
mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::faulty::{FaultyStore, PutFault};
use kbf_front::{Cache, FLUSHES_PER_ROUND, ShutdownFlush};
use kbf_meta::StagedState;
use kbf_objstore::{Capabilities, ObjectKey};
use support::{Farm, SEGMENT, TestBlob, blobs, key, later, start, verified};
use tokio::sync::oneshot;

const GIB: u64 = 1 << 30;

/// Long enough that a flush that works ends first, on a loaded machine too.
const DEADLINE: Duration = Duration::from_secs(30);

/// What the final flush logs for a round that failed.
const FAILED_ROUND: &str = "a round of the final flush of the write tier failed";

/// Where the test's subscriber writes.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Lines {
    /// The logged lines that contain `part`.
    fn with(&self, part: &str) -> Vec<String> {
        let log = self.0.lock().expect("log");
        let text = String::from_utf8_lossy(&log).into_owned();
        let lines = text.lines().filter(|line| line.contains(part));
        lines.map(str::to_owned).collect()
    }
}

/// A subscriber that writes to the returned lines, on this thread (the test's one), until
/// the guard drops.
fn capture() -> (Lines, tracing::subscriber::DefaultGuard) {
    let lines = Lines::default();
    let writer = lines.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    (lines, tracing::subscriber::set_default(subscriber))
}

/// Catches: a stop that does something to a cache with no write tier (it has no block to
/// flush and no disk to ask), or one that waits out its deadline for nothing.
#[tokio::test]
async fn a_cache_with_no_tier_has_nothing_to_flush() {
    let ended = Cache::memory().flush_for_shutdown(DEADLINE).await;
    assert_eq!(ended, ShutdownFlush::Flushed);
}

/// Catches: a stop that leaves the open block staged. Three acknowledged blobs are in the
/// open grouped block, which the flusher seals only 30 s after it opened; at a stop the
/// block must be sealed and flushed at once: its object is in the store, the blobs are
/// no longer staged and read the same, and the tier holds no block. A final flush that
/// reads the clock as it is (the seal delay not passed) never seals it and runs to its
/// deadline; one that returns without a round leaves the store empty.
#[tokio::test]
async fn a_stop_seals_and_flushes_the_open_block() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 3, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("staged");
    let object = farm.log.object_of(&blobs[0].digest);
    assert_eq!(farm.log.staged_state(object), Some(StagedState::Open));
    assert_eq!(farm.store.put_keys(), Vec::<ObjectKey>::new());

    let ended = cache.flush_for_shutdown(DEADLINE).await;
    assert_eq!(ended, ShutdownFlush::Flushed);
    assert_eq!(farm.store.put_keys(), [key(&cache, object)]);
    for b in &blobs {
        assert_eq!(farm.log.located(&b.digest).1, None, "flushed");
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    let health = cache.tier_health().expect("a tier");
    assert_eq!(health.unflushed_blocks, 0);
}

/// Catches: a stop that runs one round and returns, when a round flushes at most
/// [`FLUSHES_PER_ROUND`] objects: six objects each in a block of its own are all in the
/// store when the stop returns.
#[tokio::test]
async fn a_stop_flushes_more_objects_than_one_round_takes() {
    assert_eq!(FLUSHES_PER_ROUND, 4);
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let len = usize::try_from(SEGMENT).expect("size") + 1;
    let mut digests = Vec::new();
    for seed in 1..=6 {
        let b = TestBlob::new(seed, len);
        cache.store_blobs(vec![b.verified()]).await.expect("staged");
        digests.push(b.digest);
    }

    let ended = cache.flush_for_shutdown(DEADLINE).await;
    assert_eq!(ended, ShutdownFlush::Flushed);
    assert_eq!(farm.store.put_keys().len(), 6);
    for digest in &digests {
        assert_eq!(farm.log.located(digest).1, None, "flushed");
    }
}

/// Catches: a stop that leaves to the live flusher's backoff an object whose flush failed
/// before the stop began. Three rounds (at times the test gives them) fail the one
/// object's PUT, so its next attempt is 4 s later; the stop, with a deadline of 3 s, must
/// try it at once and succeed. A final flush that takes the clock as it is waits out the
/// backoff and ends at its deadline with the block still staged.
#[tokio::test]
async fn a_stop_retries_at_once_what_the_flusher_was_backing_off_from() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let len = usize::try_from(SEGMENT).expect("size") + 1;
    let b = TestBlob::new(1, len);
    cache.store_blobs(vec![b.verified()]).await.expect("staged");
    let object = farm.log.object_of(&b.digest);
    let t0 = Instant::now();
    // The seconds after `t0` of each round, and the wait the failure it makes is given.
    for (at, wait) in [(0, 1), (2, 2), (5, 4)] {
        farm.store.on_put(PutFault::Fail);
        let round = cache
            .flush_tier(t0 + Duration::from_secs(at))
            .await
            .expect("round");
        assert_eq!(round.retrying, [(object, Duration::from_secs(wait))]);
    }
    assert_eq!(farm.store.put_keys().len(), 3);

    let ended = cache.flush_for_shutdown(Duration::from_secs(3)).await;
    assert_eq!(ended, ShutdownFlush::Flushed);
    assert_eq!(farm.store.put_keys().len(), 4);
    assert_eq!(farm.log.located(&b.digest).1, None, "flushed");
}

/// Catches: a stop that waits for a store that stays down for as long as it stays down
/// (this test would not end); one that gives up the block it could not flush (it must
/// stay staged, readable and unmarked, and a start after a power cut must flush it, with
/// no loss counted). The deadline of 1 s ends the stop with the one block and its bytes
/// still on the tier, as the tier counted them before.
#[tokio::test]
async fn a_stop_that_cannot_flush_ends_at_its_deadline_and_loses_nothing() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let b = TestBlob::new(1, 4096);
    cache.store_blobs(vec![b.verified()]).await.expect("staged");
    let object = farm.log.object_of(&b.digest);
    for _ in 0..3 {
        farm.store.on_put(PutFault::Fail);
    }
    let before = cache.tier_health().expect("a tier");
    assert_eq!(before.unflushed_blocks, 1);

    let ended = cache.flush_for_shutdown(Duration::from_secs(1)).await;
    let left = ShutdownFlush::TimedOut {
        blocks: before.unflushed_blocks,
        bytes: before.unflushed_bytes,
    };
    assert_eq!(ended, left);
    assert!(!farm.store.put_keys().is_empty(), "the flush was tried");
    assert_eq!(farm.log.staged_state(object), Some(StagedState::Sealed));
    assert!(farm.has_block(object));
    assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    assert_eq!(farm.log.unreachable(object), None);

    drop(cache);
    let fs = farm.fs.crash(0);
    let store = FaultyStore::new(Capabilities::default());
    let (cache, startup) = start(&fs, &farm.log, &store, GIB, None)
        .await
        .expect("start");
    assert_eq!(startup.recovery.lost, []);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, [object]);
    assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    assert_eq!(farm.log.losses(), (0, 0));
}

/// Catches: a final flush that gives up, or panics, at the first round the metadata log
/// refuses (a log can be without a leader for a moment at a stop), and one that does not
/// say so. The log refuses the open block's seal twice: each refusal is one logged line
/// with the log's reason, and the third round seals and flushes it.
#[tokio::test]
async fn a_round_the_log_refuses_is_logged_and_tried_again() {
    let (lines, _guard) = capture();
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let b = TestBlob::new(1, 4096);
    cache.store_blobs(vec![b.verified()]).await.expect("staged");
    farm.log.refuse("seal_staged", 2);

    let ended = cache.flush_for_shutdown(DEADLINE).await;
    assert_eq!(ended, ShutdownFlush::Flushed);
    assert_eq!(farm.log.located(&b.digest).1, None, "flushed");
    let said = lines.with(FAILED_ROUND);
    assert_eq!(said.len(), 2, "{said:?}");
    assert!(
        said[0].contains("seal_staged refused by the test"),
        "{said:?}"
    );
}

/// Catches: a final flush that runs beside a round of the live flusher, which is still
/// running when a stop begins. A round is held at its PUT; the stop begins meanwhile and
/// must wait for that round: after five of its pauses the store has had the one PUT, and
/// when the round ends the object is flushed once and nothing logs a flush that failed. A
/// final flush that does not wait takes the same sealed object and PUTs it a second time.
#[tokio::test]
async fn a_stop_does_not_flush_beside_a_round_of_the_flusher() {
    let (lines, _guard) = capture();
    let farm = Farm::new(Capabilities::default());
    let cache = Arc::new(farm.start(GIB).await);
    let b = TestBlob::new(1, 4096);
    cache.store_blobs(vec![b.verified()]).await.expect("staged");
    let object = farm.log.object_of(&b.digest);
    let (entered_tx, entered) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    farm.store.on_put(PutFault::Hold {
        entered: entered_tx,
        release: release_rx,
    });
    let flusher = Arc::clone(&cache);
    let round = tokio::spawn(async move { flusher.flush_tier(later()).await });
    entered.await.expect("the flush reached the store");
    let stopper = Arc::clone(&cache);
    let stop = tokio::spawn(async move { stopper.flush_for_shutdown(DEADLINE).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(farm.store.put_keys().len(), 1);

    release.send(()).expect("release");
    let round = round.await.expect("join").expect("round");
    assert_eq!(round.flushed, [object]);
    assert_eq!(stop.await.expect("join"), ShutdownFlush::Flushed);
    assert_eq!(farm.store.put_keys(), [key(&cache, object)]);
    let failed = lines.with("flush failed");
    assert!(failed.is_empty(), "{failed:?}");
    assert_eq!(farm.log.located(&b.digest).1, None, "flushed");
}
