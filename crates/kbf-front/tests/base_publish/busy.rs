//! The base tree published into a store that is busy or restarting: a length check or a write
//! the store answers with an error that says "not now" (a 503 `SlowDown`, a 500, a refused
//! connection) is made again after waits that double from 0.5 s, until the store takes it or
//! the start's bound passes; any other answer fails the publish at once, as before.
//!
//! Time is a virtual clock that a wait moves forward only when it is waited on, so a bound of
//! 30 s is 30 s of waits, counted, never taken.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_front::base_publish::{PublishError, publish};
use kbf_front::large_backfill::LargeStore;
use kbf_front::start_retry::{RetryClock, StartRetry};
use kbf_objstore::{BodyStream, ObjectKey, ObjectStoreError};

use super::{COMMIT, PLATFORM, install, key, scratch, standard};

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
        Box::pin(async move {
            *self.now.lock().unwrap_or_else(PoisonError::into_inner) += wait;
            self.waits
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(wait);
        })
    }
}

/// How the store answers one write before it is made.
enum Answer {
    /// The write is refused with this error, and nothing is stored.
    Refuse(ObjectStoreError),
    /// The object is stored and the answer is this error: the answer was lost.
    Lose(ObjectStoreError),
}

#[derive(Default)]
struct State {
    /// The length of each object held, by key.
    objects: BTreeMap<String, usize>,
    /// The keys whose length was asked, in order.
    lengths: Vec<String>,
    /// The keys a write was attempted at, in order.
    puts: Vec<String>,
    /// The answers to the next length checks, before the store's own.
    length_answers: VecDeque<ObjectStoreError>,
    /// The answers to the next writes, before the store's own.
    put_answers: VecDeque<Answer>,
    /// The answer to every write, for ever.
    every_put: Option<fn() -> ObjectStoreError>,
}

/// A store in memory that answers as it is told before it answers as itself.
#[derive(Default)]
struct Busy {
    state: Mutex<State>,
}

impl Busy {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl LargeStore for Busy {
    fn len<'a>(
        &'a self,
        key: &'a ObjectKey,
    ) -> BoxFuture<'a, Result<Option<u64>, ObjectStoreError>> {
        Box::pin(async move {
            let mut state = self.state();
            state.lengths.push(key.to_string());
            if let Some(error) = state.length_answers.pop_front() {
                return Err(error);
            }
            Ok(state.objects.get(key.as_str()).map(|n| *n as u64))
        })
    }

    fn put<'a>(
        &'a self,
        key: &'a ObjectKey,
        _len: u64,
        mut body: BodyStream,
    ) -> BoxFuture<'a, Result<(), ObjectStoreError>> {
        Box::pin(async move {
            let mut got = 0;
            while let Some(piece) = body.next().await {
                got += piece?.len();
            }
            let mut state = self.state();
            state.puts.push(key.to_string());
            if let Some(every) = state.every_put {
                return Err(every());
            }
            match state.put_answers.pop_front() {
                Some(Answer::Refuse(error)) => Err(error),
                Some(Answer::Lose(error)) => {
                    state.objects.insert(key.to_string(), got);
                    Err(error)
                }
                None => {
                    state.objects.insert(key.to_string(), got);
                    Ok(())
                }
            }
        })
    }
}

fn service(status: u16, code: &str, message: &str) -> ObjectStoreError {
    ObjectStoreError::Service {
        status,
        code: code.to_owned(),
        message: message.to_owned(),
    }
}

fn slow_down() -> ObjectStoreError {
    service(503, "SlowDown", "busy")
}

fn secs(seconds: &[f64]) -> Vec<Duration> {
    seconds
        .iter()
        .map(|s| Duration::from_secs_f64(*s))
        .collect()
}

/// A retry of `bound` seconds on a virtual clock, and the clock.
fn on_clock(bound: u64) -> (StartRetry, Arc<Clock>) {
    let clock = Arc::new(Clock::default());
    let retry = StartRetry::with_clock(Duration::from_secs(bound), clock.clone());
    (retry, clock)
}

/// Asserts that a write the store answers 503 `SlowDown` three times is made a fourth time,
/// after waits of 0.5, 1 and 2 s, its length asked again before each, and the publish then
/// writes every object of the bundle. Catches: a frontend whose start ends at the first
/// `SlowDown` of a store that is restarting, which is what rolled a release back.
#[tokio::test]
async fn a_write_answered_slow_down_three_times_is_made_again_and_the_publish_succeeds() {
    let dir = scratch("base-publish-busy-slow-down");
    let release = standard(b"kbf-task");
    install(&dir, &release);
    let store = Arc::new(Busy::default());
    store
        .state()
        .put_answers
        .extend((0..3).map(|_| Answer::Refuse(slow_down())));
    let (retry, clock) = on_clock(120);

    let published = publish(&dir, COMMIT, PLATFORM, store.clone(), true, &retry)
        .await
        .expect("published");

    assert_eq!(published.written, release.objects.len() as u64);
    assert_eq!(published.held, 0);
    assert_eq!(clock.waits(), secs(&[0.5, 1.0, 2.0]));
    let first = key(&release.objects[0].0);
    let state = store.state();
    let asked = state.lengths.iter().filter(|k| **k == first).count();
    let written = state.puts.iter().filter(|k| **k == first).count();
    assert_eq!((asked, written), (4, 4));
    assert_eq!(state.objects.len(), release.objects.len());
}

/// Asserts that a write the store answers 503 for ever fails the publish when the bound of 30 s
/// has passed, not before: the error names the key and the call, the store's last answer, the
/// attempts and the 30 s, and nothing after the object is written, the root least of all.
/// Catches: a start held for ever by a store that stays down, one given up before its bound,
/// and a give-up that does not say how long it tried.
#[tokio::test]
async fn a_write_answered_503_for_ever_fails_the_publish_at_the_bound() {
    let dir = scratch("base-publish-busy-for-ever");
    let release = standard(b"kbf-task");
    install(&dir, &release);
    let store = Arc::new(Busy::default());
    store.state().every_put = Some(slow_down);
    let (retry, clock) = on_clock(30);

    let error = publish(&dir, COMMIT, PLATFORM, store.clone(), true, &retry)
        .await
        .expect_err("given up");

    assert!(matches!(error, PublishError::GaveUp(_)), "{error}");
    let first = key(&release.objects[0].0);
    assert_eq!(
        error.to_string(),
        format!(
            "{first}: the store failed the write: store answered 503 SlowDown: busy (still \
             failing after 7 attempts over 30.0 s)"
        )
    );
    assert_eq!(clock.waits(), secs(&[0.5, 1.0, 2.0, 4.0, 8.0, 8.0, 6.5]));
    let state = store.state();
    assert!(state.puts.iter().all(|k| *k == first), "{:?}", state.puts);
    assert!(!state.objects.contains_key(&key(&release.built.root)));
}

/// Asserts that a 403 to a write fails the publish at once, as before the retry: one attempt,
/// no wait, the error the store's answer as it was. Catches: a refusal retried for the whole
/// bound, which would hold a start that can never succeed.
#[tokio::test]
async fn a_403_to_a_write_fails_the_publish_at_once() {
    let dir = scratch("base-publish-busy-denied");
    let release = standard(b"kbf-task");
    install(&dir, &release);
    let store = Arc::new(Busy::default());
    store
        .state()
        .put_answers
        .push_back(Answer::Refuse(service(403, "AccessDenied", "no")));
    let (retry, clock) = on_clock(120);

    let error = publish(&dir, COMMIT, PLATFORM, store.clone(), true, &retry)
        .await
        .expect_err("refused");

    let first = key(&release.objects[0].0);
    assert!(matches!(error, PublishError::Store { .. }), "{error}");
    assert_eq!(
        error.to_string(),
        format!("{first}: the store failed the write: store answered 403 AccessDenied: no")
    );
    assert!(clock.waits().is_empty(), "{:?}", clock.waits());
    assert_eq!(store.state().puts, [first]);
}

/// Asserts that a length check whose connection was refused, and then answered 504, is asked
/// again, and the object is written once it is answered. Catches: a retry of the writes alone,
/// which leaves the start ending at the first length check of a store that is restarting.
#[tokio::test]
async fn a_length_check_refused_then_answered_504_is_asked_again() {
    let dir = scratch("base-publish-busy-length");
    let release = standard(b"kbf-task");
    install(&dir, &release);
    let store = Arc::new(Busy::default());
    let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
    store.state().length_answers.extend([
        ObjectStoreError::Transport(Arc::new(refused)),
        service(504, "GatewayTimeout", "late"),
    ]);
    let (retry, clock) = on_clock(120);

    let published = publish(&dir, COMMIT, PLATFORM, store.clone(), true, &retry)
        .await
        .expect("published");

    assert_eq!(published.written, release.objects.len() as u64);
    assert_eq!(clock.waits(), secs(&[0.5, 1.0]));
    let first = key(&release.objects[0].0);
    assert_eq!(store.state().lengths[..3], vec![first; 3]);
}

/// Asserts that a write the store took but whose answer was lost (a 500 after the object was
/// stored) is found held by the next attempt's length check and is not written again. Catches:
/// a retry that writes blindly, and a publish that counts the object twice.
#[tokio::test]
async fn a_write_whose_answer_was_lost_is_found_held_by_the_next_attempt() {
    let dir = scratch("base-publish-busy-lost");
    let release = standard(b"kbf-task");
    install(&dir, &release);
    let store = Arc::new(Busy::default());
    store
        .state()
        .put_answers
        .push_back(Answer::Lose(service(500, "InternalError", "lost")));
    let (retry, clock) = on_clock(120);

    let published = publish(&dir, COMMIT, PLATFORM, store.clone(), true, &retry)
        .await
        .expect("published");

    assert_eq!(published.written, release.objects.len() as u64 - 1);
    assert_eq!(published.held, 1);
    assert_eq!(clock.waits(), secs(&[0.5]));
    let first = key(&release.objects[0].0);
    let written = store.state().puts.iter().filter(|k| **k == first).count();
    assert_eq!(written, 1);
}
