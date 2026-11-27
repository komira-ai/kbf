//! [`StartRetry`]: a start-time call to the pack store is made again on the answers that say
//! "not now", after waits that double from 0.5 s up to 8 s, until it succeeds or its bound
//! passes; any other answer is the call's at once. Time is a virtual clock that a wait moves
//! forward only when it is waited on, so a bound of 30 s is 30 s of waits, counted, never
//! taken.

use std::cell::Cell;
use std::sync::{Mutex, PoisonError};

use super::*;

/// The virtual clock: the time is the waits waited on so far, added up.
#[derive(Default)]
struct Clock {
    now: Mutex<Duration>,
    waits: Mutex<Vec<Duration>>,
}

impl Clock {
    fn waits(&self) -> Vec<Duration> {
        self.waits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl RetryClock for Clock {
    fn now(&self) -> Duration {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn sleep(&self, wait: Duration) -> BoxFuture<'_, ()> {
        // Moved when waited on, not when made: the wait raced against an attempt that answers
        // at once is never waited on and takes no time.
        Box::pin(async move {
            *self.now.lock().unwrap_or_else(PoisonError::into_inner) += wait;
            self.waits
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(wait);
        })
    }
}

fn slow_down() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 503,
        code: "SlowDown".to_owned(),
        message: "busy".to_owned(),
    }
}

fn denied() -> ObjectStoreError {
    ObjectStoreError::Service {
        status: 403,
        code: "AccessDenied".to_owned(),
        message: "no".to_owned(),
    }
}

fn secs(seconds: &[f64]) -> Vec<Duration> {
    seconds
        .iter()
        .map(|s| Duration::from_secs_f64(*s))
        .collect()
}

fn retry_within(bound: u64) -> (StartRetry, Arc<Clock>) {
    let clock = Arc::new(Clock::default());
    let retry = StartRetry::with_clock(Duration::from_secs(bound), clock.clone());
    (retry, clock)
}

/// Asserts that a call the store answers 503 `SlowDown` three times and then takes succeeds on
/// its fourth attempt, after waits of 0.5, 1 and 2 s. Catches: a start that ends at the first
/// `SlowDown`, and waits that do not double.
#[tokio::test]
async fn a_call_answered_slow_down_three_times_succeeds_on_the_fourth() {
    let (retry, clock) = retry_within(120);
    let made = Cell::new(0);

    let answer = retry
        .run("cas/l/a", retriable, |attempt| {
            made.set(attempt);
            async move {
                match attempt {
                    1..=3 => Err(slow_down()),
                    _ => Ok("written"),
                }
            }
        })
        .await;

    assert_eq!(answer.expect("succeeds"), "written");
    assert_eq!(made.get(), 4);
    assert_eq!(clock.waits(), secs(&[0.5, 1.0, 2.0]));
}

/// Asserts that a call the store answers 503 for ever is given up at its bound of 30 s, not
/// before and not after: waits of 0.5 to 8 s that double, then the wait to the bound, and the
/// error says the last answer, the attempts and the 30 s. Catches: a retry with no end, one
/// that ends before the bound, and a give-up that does not say how long it tried.
#[tokio::test]
async fn a_call_answered_503_for_ever_is_given_up_at_the_bound() {
    let (retry, clock) = retry_within(30);

    let answer: Result<(), _> = retry
        .run("cas/l/a", retriable, |_| async { Err(slow_down()) })
        .await;

    let Err(Ended::GaveUp(gave_up)) = answer else {
        panic!("not given up: {answer:?}");
    };
    assert_eq!(clock.waits(), secs(&[0.5, 1.0, 2.0, 4.0, 8.0, 8.0, 6.5]));
    assert_eq!(gave_up.attempts, 7);
    assert_eq!(gave_up.tried, Duration::from_secs(30));
    assert_eq!(
        Ended::GaveUp(gave_up).to_string(),
        "store answered 503 SlowDown: busy (still failing after 7 attempts over 30.0 s)"
    );
}

/// Asserts that an answer that is not retried (a 403) ends the call at once: one attempt, no
/// wait, the answer as it was. Catches: a retry of a refusal, which would hold a start that can
/// never succeed for the whole bound.
#[tokio::test]
async fn a_403_ends_the_call_at_once() {
    let (retry, clock) = retry_within(120);
    let made = Cell::new(0);

    let answer: Result<(), _> = retry
        .run("cas/l/a", retriable, |attempt| {
            made.set(attempt);
            async { Err(denied()) }
        })
        .await;

    let Err(Ended::Refused(error)) = answer else {
        panic!("not refused: {answer:?}");
    };
    assert_eq!(
        Ended::Refused(error).to_string(),
        "store answered 403 AccessDenied: no"
    );
    assert_eq!(made.get(), 1);
    assert!(clock.waits().is_empty(), "{:?}", clock.waits());
}

/// Asserts that a bound of zero retries nothing: a 503 is the call's answer at once, refused as
/// before the bound. Catches: a zero that waits, or that is taken for a give-up.
#[tokio::test]
async fn a_bound_of_zero_retries_nothing() {
    let (retry, clock) = retry_within(0);
    let made = Cell::new(0);

    let answer: Result<(), _> = retry
        .run("cas/l/a", retriable, |attempt| {
            made.set(attempt);
            async { Err(slow_down()) }
        })
        .await;

    assert!(matches!(answer, Err(Ended::Refused(_))), "{answer:?}");
    assert_eq!(made.get(), 1);
    assert!(clock.waits().is_empty(), "{:?}", clock.waits());
    assert_eq!(retry.bound(), Duration::ZERO);
}

/// Asserts that an attempt the store never answers is dropped at the bound: a first attempt
/// with no answer is given up saying the store did not answer within the bound, and a retry
/// with no answer is given up with the answer before it. Catches: a start held for ever by a
/// store that takes the request and never answers.
#[tokio::test]
async fn an_attempt_never_answered_is_dropped_at_the_bound() {
    let (retry, _) = retry_within(20);

    let answer: Result<(), Ended<ObjectStoreError>> = retry
        .run("cas/l/a", retriable, |_| std::future::pending())
        .await;

    let Err(Ended::GaveUp(gave_up)) = answer else {
        panic!("not given up: {answer:?}");
    };
    assert_eq!(gave_up.attempts, 1);
    assert!(gave_up.last.is_none());
    assert_eq!(
        gave_up.to_string(),
        "cas/l/a: the pack store did not answer within 20.0 s"
    );

    let (retry, clock) = retry_within(20);
    let answer: Result<(), Ended<ObjectStoreError>> = retry
        .run("cas/l/a", retriable, |attempt| async move {
            match attempt {
                1 => Err(slow_down()),
                _ => std::future::pending().await,
            }
        })
        .await;

    let Err(Ended::GaveUp(gave_up)) = answer else {
        panic!("not given up: {answer:?}");
    };
    assert_eq!(clock.waits(), secs(&[0.5, 19.5]));
    assert_eq!(
        gave_up.to_string(),
        "store answered 503 SlowDown: busy (still failing after 2 attempts over 20.0 s, the \
         last of them unanswered)"
    );
}

/// Asserts which answers are retried: a 500, 502, 503 or 504, a 400 `RequestTimeout` and a
/// request with no answer; and which are not: a 501, a 400 of another code, a 403, a missing
/// key and a key that exists. Catches: a refusal taken for a store that is busy, and a busy
/// store taken for a refusal.
#[test]
fn the_answers_retried_are_the_ones_that_say_not_now() {
    let service = |status: u16, code: &str| ObjectStoreError::Service {
        status,
        code: code.to_owned(),
        message: String::new(),
    };
    let key = kbf_objstore::ObjectKey::new("cas/l/a".to_owned()).expect("a key");
    let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
    for status in [500, 502, 503, 504] {
        assert!(retriable(&service(status, "")), "{status}");
    }
    assert!(retriable(&service(400, "RequestTimeout")));
    assert!(retriable(&ObjectStoreError::Transport(Arc::new(refused))));
    for (status, code) in [(501, ""), (400, "BadDigest"), (403, "AccessDenied")] {
        assert!(!retriable(&service(status, code)), "{status} {code}");
    }
    assert!(!retriable(&ObjectStoreError::NotFound(key.clone())));
    assert!(!retriable(&ObjectStoreError::AlreadyExists(key)));
}

/// Asserts that the retry of a running frontend waits on the tokio timer: a call answered 503
/// once succeeds on its second attempt (on a paused clock, which runs the wait at once), and
/// the retry shows its bound. Catches: a frontend whose waits are not waited.
#[tokio::test(start_paused = true)]
async fn the_frontend_retry_waits_on_the_tokio_timer() {
    let retry = StartRetry::new(Duration::from_secs(120));

    let answer = retry
        .run("cas/l/a", retriable, |attempt| async move {
            match attempt {
                1 => Err(slow_down()),
                _ => Ok(attempt),
            }
        })
        .await;

    assert_eq!(answer.expect("succeeds"), 2);
    assert_eq!(format!("{retry:?}"), "StartRetry { bound: 120s, .. }");
}
