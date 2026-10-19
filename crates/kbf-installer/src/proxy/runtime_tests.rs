use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;

/// Catches a sleep that returns early or not at all, and a clock that is not wall time.
#[test]
fn a_sleep_lasts_its_duration_and_the_clock_is_the_wall_clock() {
    let clock = SystemClock::new();
    let before = clock.now();
    let start = Instant::now();
    assert_eq!(clock.sleep(Duration::from_millis(60)), Ok(()));
    assert!(start.elapsed() >= Duration::from_millis(60));
    let after = clock.now();
    assert!(after >= before + Duration::from_millis(50));
    // Seconds since the epoch of this century, not since a start of the process.
    assert!(before > Duration::from_secs(1_700_000_000));
}

/// Catches a shutdown that waits out the swap's sleep: a 60 s wait must end at the stop, and
/// every later wait ends at once.
#[test]
fn a_stop_ends_a_long_sleep_at_once_and_every_sleep_after() {
    let clock = Arc::new(SystemClock::new());
    let sleeper = {
        let clock = Arc::clone(&clock);
        std::thread::spawn(move || clock.sleep(Duration::from_secs(60)))
    };
    std::thread::sleep(Duration::from_millis(50));
    let stopped = Instant::now();
    clock.stop();
    assert_eq!(sleeper.join().expect("thread"), Err(Halt::Interrupted));
    assert!(stopped.elapsed() < Duration::from_secs(5));
    assert_eq!(clock.sleep(Duration::from_secs(60)), Err(Halt::Interrupted));
}

/// Catches a queue that reorders, repeats or loses a request.
#[test]
fn requests_come_out_once_in_the_order_they_went_in() {
    let queue = RequestQueue::new();
    assert_eq!(queue.next(), None);
    queue.push(Request::Abort);
    queue.push(Request::Rollback);
    assert_eq!(queue.next(), Some(Request::Abort));
    assert_eq!(queue.next(), Some(Request::Rollback));
    assert_eq!(queue.next(), None);
}
