//! The rules of [`Flights`] without a task or a source: when `give_up` takes a flight
//! out of the map, and when the last waiter's leaving tells the flight's task. The test
//! holds the [`Publish`] the task would, and sets `arrived` itself.

use std::sync::atomic::Ordering;

use futures::FutureExt as _;
use kbf_proto::reapi::Digest;

use super::{Flights, Publish, Waiter};

fn digest(size_bytes: i64) -> Digest {
    Digest {
        hash: "ab".repeat(32),
        size_bytes,
    }
}

/// Starts the flight of `digest`, whose task is the test: it holds the [`Publish`] and
/// the first waiter.
fn start<'a>(flights: &'a Flights, digest: &Digest) -> (Waiter<'a>, Publish) {
    let mut held = None;
    let (waiter, merged) = flights.join(digest, |publish| {
        held = Some(publish);
        std::future::ready(())
    });
    assert!(!merged, "the first caller of a digest starts the flight");
    (waiter, held.expect("the flight was started with a task"))
}

/// Another caller of `digest`: whether it joined a flight in progress.
fn join<'a>(flights: &'a Flights, digest: &Digest) -> (Waiter<'a>, bool) {
    flights.join(digest, |_| std::future::ready(()))
}

/// (blob size, bytes arrived, callers still waiting, whether `give_up` takes the flight).
type Row = (i64, u64, usize, bool);

const ROWS: [Row; 18] = [
    // Nobody waits: given up unless MORE than half of the blob has arrived.
    (3000, 0, 0, true),
    (3000, 1499, 0, true),
    (3000, 1500, 0, true),
    (3000, 1501, 0, false),
    (3000, 3000, 0, false),
    // An odd size: half is 1500.5, so 1500 is below it and 1501 above.
    (3001, 1500, 0, true),
    (3001, 1501, 0, false),
    // A blob of no bytes has nothing arrived, which is not more than half of nothing; a
    // size the digest gives below zero counts as 0 bytes.
    (0, 0, 0, true),
    (-7, 0, 0, true),
    (1, 0, 0, true),
    (1, 1, 0, false),
    // A caller still waiting keeps the flight, whatever has arrived.
    (3000, 0, 1, false),
    (3000, 1500, 1, false),
    (3000, 1501, 1, false),
    (3000, 3000, 1, false),
    (0, 0, 1, false),
    (-7, 0, 1, false),
    (1, 0, 1, false),
];

/// Catches `give_up` taking a flight a caller still waits for (that caller would be
/// told "the fetch ended without an answer"), never taking one (a read nobody wants runs
/// on), taking one with more than half of its blob in (the next caller reads it all
/// again), keeping one with exactly half (`>` read as `>=`), a size of zero or below
/// read as something else, and a flight that `give_up` reports taken but leaves in the
/// map (the next caller would join a task that has returned). Each row also checks what
/// the next caller finds: its own new flight after a give-up, the flight in progress
/// otherwise.
#[tokio::test]
async fn give_up_takes_a_flight_only_when_nobody_waits_and_at_most_half_has_arrived() {
    for row @ (size, arrived, waiting, gives_up) in ROWS {
        let flights = Flights::default();
        let digest = digest(size);
        let (first, publish) = start(&flights, &digest);
        publish.flight.arrived.store(arrived, Ordering::SeqCst);
        // The first caller is the one that stays, if one does.
        let stays = Some(first).filter(|_| waiting > 0);
        assert_eq!(flights.give_up(&publish), gives_up, "give_up for {row:?}");
        assert_eq!(
            flights.len(),
            usize::from(!gives_up),
            "flights in the map after {row:?}"
        );
        let (_next, merged) = join(&flights, &digest);
        assert_eq!(merged, !gives_up, "the next caller joined, for {row:?}");
        drop(stays);
    }
}

/// Catches a flight's task told when any waiter leaves (it would wake at every one of
/// the flight's callers going), or never told when the last leaves (the read nobody
/// wants would run on), or only if the task is already asking (a caller that goes
/// before the task first looks must still be heard: the task finds it told).
#[tokio::test]
async fn the_task_is_told_when_the_last_waiter_leaves_and_not_before() {
    let flights = Flights::default();
    let digest = digest(100);
    let (first, publish) = start(&flights, &digest);
    let (second, merged_second) = join(&flights, &digest);
    let (third, merged_third) = join(&flights, &digest);
    assert!(
        merged_second && merged_third,
        "two callers joined the flight"
    );
    let flight = &publish.flight;
    assert!(
        flight.deserted().now_or_never().is_none(),
        "told with three waiting"
    );
    drop(first);
    assert!(
        flight.deserted().now_or_never().is_none(),
        "told with two waiting"
    );
    drop(second);
    assert!(
        flight.deserted().now_or_never().is_none(),
        "told with one waiting"
    );
    // Nobody is asking yet: the leaving is kept for the task's first look.
    drop(third);
    assert!(
        flight.deserted().now_or_never().is_some(),
        "not told when the last waiter left"
    );
    assert!(
        flight.deserted().now_or_never().is_none(),
        "told twice for one leaving"
    );
}
