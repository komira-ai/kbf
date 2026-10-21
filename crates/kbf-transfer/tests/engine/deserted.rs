//! What a fetch does when the last caller waiting for it leaves
//! (`docs/design/worker-cache.md#44-limits-retries-cancellation`), against the fake
//! source: the order of events is fixed by the test, not by timing.
//!
//! Every test holds the source's read after its first message, so the fetch cannot end
//! on its own: only the caller leaving, and the task's answer to it, decide what
//! happens. The caller's wait is a future the test polls in its own task, so dropping
//! it is the caller leaving, at a point of the test's choosing. The runtime is one
//! thread (`#[tokio::test]` without a flavor), so the engine's task runs only when the
//! test awaits: `let_the_engine_run` is that await. It relies on `yield_now` waking the
//! test only after the tasks already woken have run, which tokio's scheduler does and
//! its documentation does not promise; the first test would fail (the fetch still in
//! the map) if it stopped. `src/fly_tests.rs` has the same cases with the task polled by
//! hand, which depend on no scheduler.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::poll;
use kbf_proto::reapi::Digest;
use kbf_transfer::{Cas, TransferConfig, Transfers};
use kbf_types::LeaseId;

use super::fake::{Fake, until, within};
use super::{engine, patterned};

/// The size of the blob `H` of every test here.
const SIZE: usize = 3000;

/// A server holding `H` (3,000 bytes) which sends its first `first` bytes, then holds
/// the rest until `open()`; and an engine over it.
async fn serve_h(first: usize) -> (Arc<Fake>, Arc<Transfers>, Digest, Vec<u8>) {
    let fake = Arc::new(Fake::default());
    let bytes = patterned(21, SIZE);
    let h = fake.insert(bytes.clone());
    fake.message_bytes.store(first, Ordering::SeqCst);
    fake.pause.store(true, Ordering::SeqCst);
    fake.close();
    let engine = engine(fake.serve().await, TransferConfig::default());
    (fake, engine, h, bytes)
}

/// Returns after every task that is ready has run, among them the task of a fetch
/// that a caller leaving has just told.
async fn let_the_engine_run() {
    tokio::task::yield_now().await;
}

/// Catches a fetch nobody waits for that goes on reading: the only caller leaves when
/// the first 1,500 of the 3,000 bytes have arrived, which is half and not more than
/// half. In the task's next turn the fetch is out of the map, its read is dropped (the
/// request permit and the byte budget are free, and the source finds its response
/// dropped while it is still held), and the next caller starts a read of its own from
/// byte 0, which it alone counts. Mutants seen red (the flight and its permits still
/// held after A left): a `give_up` that never takes the flight; the deserted arm
/// removed from the task's `select!`; `>` read as `>=` in `half_done` (exactly half
/// kept); a `give_up` that says it took the flight and leaves it in the map (the
/// permits were free, the flight was not).
#[tokio::test]
async fn a_fetch_nobody_waits_for_with_half_its_blob_in_is_dropped_and_read_afresh() {
    let (fake, engine, h, bytes) = serve_h(SIZE / 2).await;
    let a = engine.scope(&LeaseId::new(11, 1));
    let b = engine.scope(&LeaseId::new(11, 2));

    let mut a_waits = Box::pin(a.get(&h));
    tokio::select! {
        biased;
        read = &mut a_waits => panic!("the read ended with the source holding it: {read:?}"),
        () = until("half of H arrived", || a.stats().read_bytes == 1500) => {}
    }
    assert_eq!(engine.stats().fetches, 1, "fetches while A waits");
    drop(a_waits);
    let_the_engine_run().await;
    let stats = engine.stats();
    assert_eq!(
        (
            stats.fetches,
            stats.requests_in_flight,
            stats.bytes_in_flight
        ),
        (0, 0, 0),
        "(fetches, requests, budget bytes) held after A left"
    );
    within(10, || fake.streams_dropped.load(Ordering::SeqCst) == 1).await;
    assert_eq!(
        fake.streams_dropped.load(Ordering::SeqCst),
        1,
        "responses of the source dropped while it held the read"
    );

    fake.open();
    assert_eq!(b.get(&h).await, Ok(bytes));
    assert_eq!(fake.reads_of(&h), [0, 0], "reads of H at the source");
    let b_stats = b.stats();
    assert_eq!(
        (b_stats.merged_reads, b_stats.read_calls, b_stats.read_bytes),
        (0, 1, SIZE as u64),
        "B's (merged reads, read calls, read bytes)"
    );
}

/// Catches a fetch more than half done that is dropped (the next caller would read the
/// blob all again): the only caller leaves when the first 1,501 of the 3,000 bytes
/// have arrived, one past half. In the task's next turn the fetch is still in the map
/// with its request permit and its byte budget; a later caller joins it, and once the
/// source sends the rest gets the blob, which the source served once. Mutants seen red:
/// a `give_up` that always takes an unwaited flight; `half_done` never true. The engine
/// shows nothing when `give_up` says no, so this test alone cannot tell "more than half
/// arrived" from "a caller is waiting" (it would pass the same if the task ran only
/// after B joined); the `give_up` table and `fly_tests` pin the first answer exactly,
/// and this is the end-to-end confirmation of it.
#[tokio::test]
async fn a_fetch_nobody_waits_for_with_more_than_half_its_blob_in_goes_on_for_the_next_caller() {
    let (fake, engine, h, bytes) = serve_h(SIZE / 2 + 1).await;
    let a = engine.scope(&LeaseId::new(12, 1));
    let b = engine.scope(&LeaseId::new(12, 2));

    let mut a_waits = Box::pin(a.get(&h));
    tokio::select! {
        biased;
        read = &mut a_waits => panic!("the read ended with the source holding it: {read:?}"),
        () = until("more than half of H arrived", || a.stats().read_bytes == 1501) => {}
    }
    drop(a_waits);
    let_the_engine_run().await;
    let stats = engine.stats();
    assert_eq!(
        (
            stats.fetches,
            stats.requests_in_flight,
            stats.bytes_in_flight
        ),
        (1, 1, SIZE),
        "(fetches, requests, budget bytes) held after A left"
    );

    let mut b_waits = Box::pin(b.get(&h));
    assert!(poll!(b_waits.as_mut()).is_pending(), "B waits for the read");
    assert_eq!(b.stats().merged_reads, 1, "B joined the read that went on");
    fake.open();
    assert_eq!(b_waits.await, Ok(bytes));
    assert_eq!(fake.reads_of(&h), [0], "reads of H at the source");
    assert_eq!(
        (a.stats().read_bytes, b.stats().read_bytes),
        (SIZE as u64, 0),
        "the bytes are counted for the lease that started the read"
    );
    assert_eq!(engine.stats().fetches, 0);
}

/// Catches a task that gives up a fetch a caller joined between the last one leaving
/// and the task looking (that caller is told the fetch ended without an answer): 1,000
/// of the 3,000 bytes have arrived, which alone would be given up. The caller leaves
/// and B joins in one poll of this task, so the engine's task cannot run between them;
/// it then runs, finds B waiting, and goes on, and B gets the blob, which the source
/// served once. Mutants seen red: a `give_up` that ignores the waiters; one that
/// always takes the flight.
#[tokio::test]
async fn a_caller_who_joins_before_the_task_looks_gets_the_blob_from_the_same_read() {
    let (fake, engine, h, bytes) = serve_h(1000).await;
    let a = engine.scope(&LeaseId::new(13, 1));
    let b = engine.scope(&LeaseId::new(13, 2));

    let mut a_waits = Box::pin(a.get(&h));
    tokio::select! {
        biased;
        read = &mut a_waits => panic!("the read ended with the source holding it: {read:?}"),
        () = until("a third of H arrived", || a.stats().read_bytes == 1000) => {}
    }
    drop(a_waits);
    let mut b_waits = Box::pin(b.get(&h));
    assert!(poll!(b_waits.as_mut()).is_pending(), "B waits for the read");
    assert_eq!(b.stats().merged_reads, 1, "B joined the read");
    let_the_engine_run().await;
    let stats = engine.stats();
    assert_eq!(
        (
            stats.fetches,
            stats.requests_in_flight,
            stats.bytes_in_flight
        ),
        (1, 1, SIZE),
        "(fetches, requests, budget bytes) held after the task looked"
    );

    fake.open();
    assert_eq!(b_waits.await, Ok(bytes));
    assert_eq!(fake.reads_of(&h), [0], "reads of H at the source");
    assert_eq!(engine.stats().fetches, 0);
}
