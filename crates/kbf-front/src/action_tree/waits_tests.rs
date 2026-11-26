//! A write's tally ([`tallied`]): the waits told to it are added up apart, the request is the
//! rest of its time, a wait told outside any tally reaches none, and two writes at once keep
//! their own tallies. The time is tokio's paused clock, moved only by the writes' own sleeps.

use std::time::Duration;

use super::*;

const fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A write that sleeps `slot` and tells it as a store slot's wait, sleeps `request`, then
/// sleeps `wait` and tells it as a 503 wait, twice over.
async fn write(slot: Duration, request: Duration, wait: Duration) -> &'static str {
    for _ in 0..2 {
        tokio::time::sleep(slot).await;
        waited_for_slot(slot);
        tokio::time::sleep(request).await;
        tokio::time::sleep(wait).await;
        waited_out_503(wait);
    }
    "the write's answer"
}

/// Catches a wait counted in the wrong part, a part's waits not added up over attempts, or a
/// request that keeps the waits in it: two attempts of 5 ms for a store slot, 2 ms at the store
/// and 7 ms out of a 503 are 10, 4 and 14 ms, beside the manifest slot's 3 ms as given, and the
/// write's own answer is handed back.
#[tokio::test(start_paused = true)]
async fn each_wait_is_added_up_apart_and_the_rest_is_the_request() {
    let (answer, waits) = tallied(ms(3), write(ms(5), ms(2), ms(7))).await;
    assert_eq!(answer, "the write's answer");
    let want = WriteWaits {
        manifest_slot: ms(3),
        store_slot: ms(10),
        store_503: ms(14),
        request: ms(4),
    };
    assert_eq!(waits, want);
}

/// Catches a wait told outside a write reaching a tally, or two writes at once sharing one: a
/// write under no tally tells its waits to nothing, and two tallied writes at once, interleaved
/// on one task, each get only their own waits.
#[tokio::test(start_paused = true)]
async fn waits_reach_only_the_tally_of_the_write_that_made_them() {
    assert_eq!(write(ms(1), ms(1), ms(1)).await, "the write's answer");
    let (one, two) = tokio::join!(
        tallied(Duration::ZERO, write(ms(5), ms(1), ms(0))),
        tallied(ms(1), write(ms(0), ms(2), ms(9))),
    );
    let one_want = WriteWaits {
        manifest_slot: Duration::ZERO,
        store_slot: ms(10),
        store_503: Duration::ZERO,
        request: ms(2),
    };
    let two_want = WriteWaits {
        manifest_slot: ms(1),
        store_slot: Duration::ZERO,
        store_503: ms(18),
        request: ms(4),
    };
    assert_eq!((one.1, two.1), (one_want, two_want));
}
