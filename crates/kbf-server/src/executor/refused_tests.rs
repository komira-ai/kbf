//! The bound of the refused starts an operation places again, and what its callers are told
//! past it. The clock is the caller's: each case passes `now` and the deadline.

use std::time::Duration;

use tokio::time::Instant;
use tonic::Code;

use super::{REFUSED_STARTS, REFUSED_WAIT_CAP, Refusal, Refusals, Stop};

const POLL: Duration = Duration::from_millis(250);

/// Asserts the wait before each placement: the poll first, doubling after each refusal, never
/// past the cap, and the refusals placed again counted.
#[test]
fn the_wait_doubles_from_the_poll_up_to_the_cap() {
    let now = Instant::now();
    let far = now + Duration::from_secs(24 * 3600);
    let mut refusals = Refusals::default();
    let waits: Vec<Duration> = (0..10)
        .map(|_| {
            refusals
                .add(Refusal::NoCapacity, POLL, now, far)
                .expect("placed again")
        })
        .collect();
    let ms = |ms: u64| Duration::from_millis(ms);
    assert_eq!(
        waits,
        [
            ms(250),
            ms(500),
            ms(1000),
            ms(2000),
            ms(4000),
            ms(8000),
            ms(16000),
            REFUSED_WAIT_CAP,
            REFUSED_WAIT_CAP,
            REFUSED_WAIT_CAP
        ]
    );
    assert_eq!(REFUSED_WAIT_CAP, Duration::from_secs(30));
    assert_eq!(refusals.placed_again(), 10);
}

/// Asserts the deadline: a refusal whose wait would end at the deadline stops the operation,
/// one whose wait ends just before it does not, and the stop places nothing more.
#[test]
fn a_wait_that_reaches_the_deadline_stops_the_placements() {
    let now = Instant::now();
    let mut before = Refusals::default();
    let just_after = now + POLL + Duration::from_millis(1);
    assert_eq!(
        before.add(Refusal::HostHeld, POLL, now, just_after),
        Ok(POLL)
    );
    let mut at = Refusals::default();
    assert_eq!(
        at.add(Refusal::HostHeld, POLL, now, now + POLL),
        Err(Stop::Deadline)
    );
    assert_eq!(at.placed_again(), 0);
    let past = Refusals::default().add(Refusal::HostHeld, POLL, now + POLL, now);
    assert_eq!(past, Err(Stop::Deadline));
}

/// Asserts the count bound: the first [`REFUSED_STARTS`] refusals are placed again, however far
/// the deadline, and the next stops the operation.
#[test]
fn the_refusal_past_the_bound_stops_the_placements() {
    let now = Instant::now();
    let far = now + Duration::from_secs(365 * 24 * 3600);
    let mut refusals = Refusals::default();
    for _ in 0..REFUSED_STARTS {
        assert!(refusals.add(Refusal::NoCapacity, POLL, now, far).is_ok());
    }
    assert_eq!(
        refusals.add(Refusal::NoCapacity, POLL, now, far),
        Err(Stop::Bound)
    );
    assert_eq!(refusals.placed_again(), REFUSED_STARTS);
    assert_eq!(REFUSED_STARTS, 100);
    let status = refusals.status(Stop::Bound, Duration::from_secs(600));
    assert_eq!(status.code, Code::Unavailable as i32);
    assert_eq!(
        status.message,
        "the farm's hosts refused to start the job 101 times and the job was placed again 100 \
         times, the most an operation places it: no-capacity 101 (not the action's own failure)"
    );
}

/// Asserts the callers' message at the deadline names every refusal it was given, the last
/// among them, how many times each, and the action's timeout, in full.
#[test]
fn the_deadline_answer_names_every_refusal_and_its_count() {
    let now = Instant::now();
    let deadline = now + Duration::from_secs(1);
    let mut refusals = Refusals::default();
    assert!(refusals.add(Refusal::HostHeld, POLL, now, deadline).is_ok());
    assert!(
        refusals
            .add(Refusal::NoCapacity, POLL, now, deadline)
            .is_ok()
    );
    assert!(
        refusals
            .add(Refusal::HostTainted, POLL, now, deadline)
            .is_err()
    );
    let status = refusals.status(Stop::Deadline, Duration::from_secs(90));
    assert_eq!(status.code, Code::Unavailable as i32);
    assert!(status.details.is_empty());
    assert_eq!(
        status.message,
        "the farm's hosts refused to start the job 3 times and the next placement would come \
         after the action's deadline (90s from when it was asked for): no-capacity 1, \
         host-held 1, host-tainted 1 (not the action's own failure)"
    );
}

/// Asserts each refusal by the code the job service or the agent gives it.
#[test]
fn each_refusal_is_named_by_its_code() {
    assert_eq!(Refusal::NoCapacity.code(), "no-capacity");
    assert_eq!(Refusal::HostHeld.code(), "host-held");
    assert_eq!(Refusal::HostTainted.code(), "host-tainted");
}
