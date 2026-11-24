//! The waits between the reads of one job.

use std::time::Duration;

use super::{FIRST_STEP, Pace};

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// Catches a job that has not started read at the poll from its first wait, or steps that never
/// reach the poll or pass it: at the default poll the waits are 10, 20, 40, 80 and 160 ms, then
/// the poll for as long as the job waits.
#[test]
fn the_steps_double_from_ten_milliseconds_to_the_poll() {
    let mut pace = Pace::new(ms(250));
    let waits: Vec<_> = (0..8).map(|_| pace.not_started()).collect();
    assert_eq!(
        waits,
        [
            ms(10),
            ms(20),
            ms(40),
            ms(80),
            ms(160),
            ms(250),
            ms(250),
            ms(250)
        ]
    );
}

/// Catches a bound on the reads of a job that waits which grows with the poll faster than its
/// logarithm: the reads before the waits are a poll apart are `1 + ceil(log2(poll / 10 ms))`,
/// at the flag's least poll (100 ms), the default (250 ms), a poll that is a power of two of
/// the step (160 ms), and the flag's most (10 s).
#[test]
fn the_reads_before_the_poll_are_bounded_by_the_log_of_the_poll() {
    for (poll, reads) in [(100, 5), (160, 5), (250, 6), (10_000, 11)] {
        let mut pace = Pace::new(ms(poll));
        // The first read goes out at once; each step below the poll is one more read.
        let mut count = 1;
        while pace.not_started() < ms(poll) {
            count += 1;
        }
        assert_eq!(count, reads, "a poll of {poll} ms");
    }
}

/// Catches a poll below the first step being waited as the step: the poll is the longest wait.
#[test]
fn a_poll_shorter_than_the_first_step_is_the_wait() {
    let short = FIRST_STEP / 2;
    let mut pace = Pace::new(short);
    assert_eq!(pace.not_started(), short);
    assert_eq!(pace.not_started(), short);
}

/// Catches a job read running whose reads speed up again when it is read queued after (a job
/// the service puts back in its queue): it is read at the poll from then on.
#[test]
fn a_job_read_running_is_read_at_the_poll_from_then_on() {
    let mut pace = Pace::new(ms(250));
    assert_eq!(pace.not_started(), ms(10));
    assert_eq!(pace.running(), ms(250));
    assert_eq!(pace.not_started(), ms(250));
    assert_eq!(pace.not_started(), ms(250));
}
