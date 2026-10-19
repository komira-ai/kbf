//! What the task of a flight ([`Inner::fly`]) does when its last waiter has left, polled
//! by hand: the test is the executor, so each answer follows from the poll that gave it
//! and no timing is involved. The task's read waits for the byte budget, which the test
//! holds, so no source is asked and the blob's `arrived` bytes are what the test sets.
//! The same cases against a source, with the bytes landing, are in
//! `tests/engine/deserted.rs`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::poll;
use kbf_proto::reapi::Digest;
use tokio::sync::SemaphorePermit;
use tonic::transport::Endpoint;

use super::Transfers;
use crate::config::TransferConfig;
use crate::flights::{Flight, Waiter};
use crate::stats::Counts;

type Task = Pin<Box<dyn Future<Output = ()> + Send>>;

/// An engine over a channel that is never connected: nothing here reaches a source.
fn engine() -> Transfers {
    let channel = Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
    Transfers::over(channel, TransferConfig::default())
}

/// A blob of 3,000 bytes.
fn digest() -> Digest {
    Digest {
        hash: "cd".repeat(32),
        size_bytes: 3000,
    }
}

/// The whole byte budget, so the read of any flight waits for it.
async fn hold_budget(engine: &Transfers) -> SemaphorePermit<'_> {
    let all = u32::try_from(engine.inner.bytes_total).expect("a budget that fits a u32");
    engine
        .inner
        .bytes
        .acquire_many(all)
        .await
        .expect("the semaphore is never closed")
}

/// Starts the flight of [`digest`] and returns its first waiter, the flight, and its
/// task, which the caller polls and which has not run yet.
fn start(engine: &Transfers) -> (Waiter<'_>, Arc<Flight>, Task) {
    let inner = &engine.inner;
    let digest = digest();
    let mut started = None;
    let (waiter, merged) = inner.flights.join(&digest, |publish| {
        let flight = Arc::clone(&publish.flight);
        let counts = Arc::new(Counts::new(None));
        let task: Task = Box::pin(Arc::clone(inner).fly(digest.clone(), counts, publish));
        started = Some((flight, task));
        // What is spawned is nothing: the test polls the task itself.
        std::future::ready(())
    });
    assert!(!merged, "the first caller of a digest starts the flight");
    let (flight, task) = started.expect("the flight was started with a task");
    (waiter, flight, task)
}

/// Another caller of [`digest`]: whether it joined the flight in progress.
fn join(engine: &Transfers) -> (Waiter<'_>, bool) {
    engine
        .inner
        .flights
        .join(&digest(), |_| std::future::ready(()))
}

/// Catches a task that goes on with a read nobody wants: the only waiter leaves with
/// exactly half of the blob in (1,500 of 3,000: not more than half), the task returns
/// in its next turn, and the flight is out of the map, so the next caller starts its
/// own.
#[tokio::test]
async fn a_flight_with_no_waiter_and_half_its_blob_in_is_given_up() {
    let engine = engine();
    let _budget = hold_budget(&engine).await;

    let (waiter, flight, mut task) = start(&engine);
    assert!(
        poll!(task.as_mut()).is_pending(),
        "the read waits for the budget"
    );
    flight.arrived.store(1500, Ordering::SeqCst);
    drop(waiter);
    assert!(
        poll!(task.as_mut()).is_ready(),
        "the task went on instead of giving up"
    );
    assert_eq!(engine.inner.flights.len(), 0, "flights left");
    let (_next, merged) = join(&engine);
    assert!(!merged, "the next caller starts a flight of its own");
}

/// Catches a task that is only told by a waiter who leaves while it is looking: the
/// only waiter leaves before the task's first poll, and that poll gives up (the task
/// finds the leaving kept, as `Flight::deserted` promises).
#[tokio::test]
async fn a_waiter_who_leaves_before_the_task_first_runs_still_tells_it() {
    let engine = engine();
    let _budget = hold_budget(&engine).await;

    let (waiter, _flight, mut task) = start(&engine);
    drop(waiter);
    assert!(poll!(task.as_mut()).is_ready(), "the task did not give up");
    assert_eq!(engine.inner.flights.len(), 0, "flights left");
}

/// Catches a task that drops a read more than half done (the next caller would read it
/// all again): the only waiter leaves with 1,501 of 3,000 bytes in, the task asks
/// `give_up`, is refused, and goes on; the flight stays in the map, a later caller
/// joins it, and when that caller leaves the task is told again, and again goes on.
#[tokio::test]
async fn a_flight_with_no_waiter_and_more_than_half_its_blob_in_goes_on() {
    let engine = engine();
    let _budget = hold_budget(&engine).await;

    let (waiter, flight, mut task) = start(&engine);
    assert!(
        poll!(task.as_mut()).is_pending(),
        "the read waits for the budget"
    );
    flight.arrived.store(1501, Ordering::SeqCst);
    drop(waiter);
    assert!(poll!(task.as_mut()).is_pending(), "the task gave up");
    assert_eq!(engine.inner.flights.len(), 1, "flights left");
    let (later, merged) = join(&engine);
    assert!(merged, "the later caller joins the flight");
    drop(later);
    assert!(
        poll!(task.as_mut()).is_pending(),
        "the task gave up when the later caller left"
    );
    assert_eq!(engine.inner.flights.len(), 1, "flights left");
}

/// Catches a task that gives up a flight a caller joined after the last one left but
/// before the task looked (that caller would be told "the fetch ended without an
/// answer"): 1,000 of 3,000 bytes are in, so with nobody waiting the task would give up
/// (as `a_flight_with_no_waiter_and_half_its_blob_in_is_given_up`), but the caller
/// joined first, so it goes on. When that caller leaves, the task is told again and
/// gives up: refusing once does not leave the task deaf.
#[tokio::test]
async fn a_caller_who_joins_before_the_task_looks_keeps_the_flight() {
    let engine = engine();
    let _budget = hold_budget(&engine).await;

    let (waiter, flight, mut task) = start(&engine);
    assert!(
        poll!(task.as_mut()).is_pending(),
        "the read waits for the budget"
    );
    flight.arrived.store(1000, Ordering::SeqCst);
    drop(waiter);
    let (late, merged) = join(&engine);
    assert!(merged, "the caller joins the flight");
    assert!(
        poll!(task.as_mut()).is_pending(),
        "the task gave up a waited flight"
    );
    assert_eq!(engine.inner.flights.len(), 1, "flights left");
    drop(late);
    assert!(
        poll!(task.as_mut()).is_ready(),
        "the task did not give up when the late caller left"
    );
    assert_eq!(engine.inner.flights.len(), 0, "flights left");
}
