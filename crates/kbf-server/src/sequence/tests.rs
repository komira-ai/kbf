//! The drain sequence over a farm on a fake state store: the order of its steps, what each
//! one does and when it ends. Time is paused, so a wait of minutes costs nothing.

use futures::FutureExt as _;
use futures::future::{AbortHandle, AbortRegistration};
use kbf_front::{Backend, Cache};
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::*;
use crate::drain::Phase;
use crate::health::Readiness;

/// The two waits, in seconds.
fn timing(lb_wait: u64, deadline: u64) -> DrainTiming {
    DrainTiming {
        lb_wait: Duration::from_secs(lb_wait),
        deadline: Some(Duration::from_secs(deadline)),
    }
}

/// The load balancer wait in seconds, and no deadline: the default.
fn no_deadline(lb_wait: u64) -> DrainTiming {
    DrainTiming {
        lb_wait: Duration::from_secs(lb_wait),
        deadline: None,
    }
}

/// A serving frontend's parts, as the drain stands them down: a drain with the listeners'
/// stops, and the sequence over them.
struct Rig {
    backend: Backend,
    drain: Arc<Drain>,
    readiness: Arc<Readiness>,
    closing: kbf_front::Closing,
    /// Completes when the drain sends the REAPI listener GOAWAY.
    goaway: oneshot::Receiver<()>,
    worker: Stop,
    api: Stop,
    stand: Stand,
    sequence: Sequence,
    /// Kept so the sweeper is not aborted before it is told to be.
    _sweeping: AbortRegistration,
}

impl Rig {
    fn new(timing: DrainTiming) -> Self {
        let backend = Backend::from(Arc::new(Cache::memory()));
        let (closer, closing) = kbf_front::closing();
        let readiness = Arc::new(Readiness::default());
        let (drain, goaway) = Drain::new(closer, Arc::clone(&readiness));
        let drain = Arc::new(drain);
        let (sweeper, sweeping) = AbortHandle::new_pair();
        let (worker, api) = (Stop::new(), Stop::new());
        let stand = Stand {
            drain: Arc::clone(&drain),
            backend: backend.clone(),
            worker: worker.clone(),
            sweeper,
        };
        let sequence = Sequence::new(stand.clone(), api.clone(), timing);
        Self {
            backend,
            drain,
            readiness,
            closing,
            goaway,
            worker,
            api,
            stand,
            sequence,
            _sweeping: sweeping,
        }
    }

    /// The retire mode of this frontend, which holds the sequence, as the frontend keeps it.
    fn retire(&self) -> OnceLock<Retire> {
        let retire = OnceLock::new();
        let _ = retire.set(Retire::new(self.stand.clone(), self.sequence.clone()));
        retire
    }
}

/// Asserts that `begin` says it was the first (a second call is not) and answers not ready
/// before it returns; that in the wait the frontend refuses nothing and sends no GOAWAY; that
/// when the wait is over new Execute calls are refused and the REAPI, worker and operator
/// listeners are sent GOAWAY; and that an open execution keeps the drain going until it is gone, when the drain
/// ends, closing.
#[tokio::test(start_paused = true)]
async fn the_wait_serves_everything_and_the_end_of_it_refuses_and_sends_goaway() {
    let mut rig = Rig::new(timing(10, 900));
    let open = rig.backend.metrics().execute.open_executions.raise();

    assert!(rig.sequence.begin());
    assert!(!rig.sequence.begin(), "a second begin was a first");
    assert_eq!(rig.drain.phase(), Phase::LbWait);
    assert!(rig.readiness.is_stopping(), "still ready after begin");
    tokio::time::sleep(Duration::from_millis(9_999)).await;
    assert_eq!(rig.drain.phase(), Phase::LbWait);
    assert!(!rig.closing.draining(), "refused calls in the wait");
    assert!(rig.goaway.try_recv().is_err(), "GOAWAY in the wait");
    assert!(rig.worker.fired().now_or_never().is_none());
    assert!(rig.api.fired().now_or_never().is_none());

    tokio::time::sleep(Duration::from_millis(2)).await;
    assert_eq!(rig.drain.phase(), Phase::Draining);
    assert!(rig.closing.draining(), "new calls taken after the wait");
    assert_eq!(rig.goaway.try_recv(), Ok(()));
    assert!(rig.worker.fired().now_or_never().is_some());
    assert!(rig.api.fired().now_or_never().is_some());

    assert!(rig.drain.sequence_finished().now_or_never().is_none());
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(rig.drain.phase(), Phase::Draining, "ended while open");
    drop(open);
    let ended = tokio::time::timeout(Duration::from_secs(10), rig.drain.sequence_finished());
    ended
        .await
        .expect("the drain ends once the execution is gone");
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Catches a drain that stands down while a call other than an open execution is still
/// being answered, which the exit then cuts: a ByteStream write in flight (any call in
/// `open_calls`, with no execution open) keeps the drain in its wait, past the end of the
/// load balancer wait and long after it, until it is answered; then the drain ends.
#[tokio::test(start_paused = true)]
async fn a_call_in_flight_keeps_the_drain_going_until_it_is_answered() {
    let rig = Rig::new(no_deadline(10));
    let write = rig.backend.metrics().execute.open_calls.raise();

    assert!(rig.sequence.begin());
    tokio::time::sleep(Duration::from_secs(11)).await;
    assert_eq!(rig.drain.phase(), Phase::Draining);
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(rig.drain.phase(), Phase::Draining, "stood down over a call");
    assert!(rig.drain.sequence_finished().now_or_never().is_none());
    assert!(!rig.readiness.is_handed_over(), "handed over over a call");

    drop(write);
    let ended = tokio::time::timeout(Duration::from_secs(10), rig.drain.sequence_finished());
    ended
        .await
        .expect("the drain ends once the call is answered");
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Asserts that a second stop signal (`Drain::close`) skips the waits: with a wait of ten
/// minutes and a deadline of fifteen, and an execution that never ends, the drain is over at
/// once.
#[tokio::test(start_paused = true)]
async fn a_second_signal_skips_both_waits() {
    let rig = Rig::new(timing(600, 900));
    let _open = rig.backend.metrics().execute.open_executions.raise();
    let began = Instant::now();

    assert!(rig.sequence.begin());
    rig.drain.close();
    rig.drain.sequence_finished().await;

    // Only the clients' grace, for the execution that was open, is waited.
    let most = CLOSE_GRACE + Duration::from_secs(1);
    assert!(began.elapsed() < most, "{:?}", began.elapsed());
}

/// Asserts that a drain with no execution open goes straight through: with a wait of zero
/// and no execution, the drain is over without waiting for a deadline of fifteen minutes.
#[tokio::test(start_paused = true)]
async fn a_drain_with_nothing_open_does_not_wait_for_the_deadline() {
    let rig = Rig::new(timing(0, 900));
    let began = Instant::now();

    assert!(rig.sequence.begin());
    rig.drain.sequence_finished().await;

    assert!(
        began.elapsed() < Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Asserts that at a stop signal a frontend with no state store does the plain drain, at
/// once, with no sequence.
#[tokio::test(start_paused = true)]
async fn a_stop_without_a_state_store_drains_at_once() {
    let rig = Rig::new(timing(10, 900));
    let none = OnceLock::new();

    stopped(&rig.drain, &none, Duration::from_secs(1)).await;

    assert!(!rig.drain.is_sequenced());
    assert_eq!(rig.drain.phase(), Phase::Closing);
    assert!(rig.closing.draining());
}

/// Asserts that at a stop signal a frontend with a state store begins the sequence (a second
/// beginning, as the endpoint's, is not a first), waits out the sequence and returns when it
/// is over.
#[tokio::test(start_paused = true)]
async fn a_stop_over_a_state_store_runs_the_sequence() {
    let rig = Rig::new(timing(3, 900));
    let retire = rig.retire();
    let began = Instant::now();

    stopped(&rig.drain, &retire, Duration::MAX).await;

    assert!(rig.drain.is_sequenced());
    assert!(
        began.elapsed() >= Duration::from_secs(3),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(rig.drain.phase(), Phase::Closing);
    assert!(rig.readiness.is_stopping());
    let retire = retire.get().expect("set");
    assert!(!retire.drain(), "the endpoint began it a second time");
}

/// Asserts that the stop timeout (`--shutdown-timeout-secs`) bounds a sequence that a stop
/// signal began, with no drain deadline: an execution that never ends is closed when it
/// runs out.
#[tokio::test(start_paused = true)]
async fn the_stop_timeout_closes_the_streams_of_a_sequence() {
    let rig = Rig::new(no_deadline(0));
    let retire = rig.retire();
    let _open = rig.backend.metrics().execute.open_executions.raise();
    let began = Instant::now();

    stopped(&rig.drain, &retire, Duration::from_secs(5)).await;

    let waited = began.elapsed();
    assert!(waited >= Duration::from_secs(5), "ended early: {waited:?}");
    assert!(waited < Duration::from_secs(60), "ended late: {waited:?}");
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Asserts that a task of the sequence that panics still ends it: the drain is not over
/// while the task holds its guard, and is over once the task is gone.
#[tokio::test(start_paused = true)]
async fn a_sequence_task_that_panics_still_ends_the_sequence() {
    let rig = Rig::new(timing(0, 900));
    let ends = Ends(Arc::clone(&rig.drain));
    assert!(rig.drain.sequence_finished().now_or_never().is_none());

    let task = tokio::spawn(async move {
        let _ends = ends;
        panic!("a step of the sequence failed");
    });

    assert!(task.await.is_err(), "the task did not panic");
    rig.drain.sequence_finished().await;
}

/// Asserts that the defaults are the ones the flags document: ten seconds of wait and no
/// deadline.
#[test]
fn the_defaults_are_ten_seconds_and_no_deadline() {
    let timing = DrainTiming::default();
    assert_eq!(timing.lb_wait, Duration::from_secs(10));
    assert_eq!(timing.deadline, None);
}

/// What begins the drain in the tests below: a stop signal, or `POST /v1/frontend/drain`.
/// Both end in the same sequence ([`Retire::drain`]), and these tests run each.
#[derive(Clone, Copy, Debug)]
enum Begin {
    Signal,
    Endpoint,
}

/// Runs the stop (or the endpoint's drain) in a task of its own, which ends with it.
fn begin(rig: &Rig, how: Begin, timeout: Duration) -> tokio::task::JoinHandle<()> {
    let retire = Arc::new(rig.retire());
    let drain = Arc::clone(&rig.drain);
    tokio::spawn(async move {
        match how {
            Begin::Signal => stopped(&drain, &retire, timeout).await,
            Begin::Endpoint => {
                retire.get().expect("set").drain();
                drain.sequence_finished().await;
            }
        }
    })
}

/// Asserts that an execution open on the front's store holds the drain, however it began and
/// however long the deadline (if any) has passed: ten minutes after the wait the stop is
/// still going on and the status reports the deadline passed; and that once the execution is
/// gone the drain ends.
async fn an_open_execution_holds_the_drain(how: Begin, timing: DrainTiming) {
    let rig = Rig::new(timing);
    let open = rig.backend.metrics().execute.open_executions.raise();
    let stop = begin(&rig, how, Duration::MAX);

    tokio::time::sleep(Duration::from_secs(600)).await;
    assert!(
        !stop.is_finished(),
        "the drain ended over an open execution"
    );
    assert_eq!(rig.drain.phase(), Phase::Draining);
    assert_eq!(rig.stand.open_executions(), 1);

    drop(open);
    tokio::time::timeout(Duration::from_secs(60), stop)
        .await
        .expect("the drain ends once the execution is gone")
        .expect("task");
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

#[tokio::test(start_paused = true)]
async fn a_stop_waits_for_an_open_execution_however_long() {
    an_open_execution_holds_the_drain(Begin::Signal, no_deadline(1)).await;
}

#[tokio::test(start_paused = true)]
async fn the_endpoints_drain_waits_for_an_open_execution_past_its_deadline() {
    an_open_execution_holds_the_drain(Begin::Endpoint, timing(1, 30)).await;
}

/// Asserts that a drain deadline that passes with an execution open is reported in the status
/// and in nothing else: the drain still waits, and ends when the execution does.
#[tokio::test(start_paused = true)]
async fn a_deadline_passed_with_an_execution_open_is_reported_and_the_drain_goes_on() {
    let rig = Rig::new(timing(0, 30));
    let open = rig.backend.metrics().execute.open_executions.raise();
    assert!(rig.sequence.begin());

    tokio::time::sleep(Duration::from_secs(29)).await;
    assert!(!rig.drain.deadline_passed());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(rig.drain.deadline_passed());
    assert!(rig.drain.sequence_finished().now_or_never().is_none());

    drop(open);
    rig.drain.sequence_finished().await;
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Catches a deadline that does not see a drain held by a call alone: a ByteStream write open
/// past the deadline, with no execution open, is reported (`deadline_passed`) and still waited
/// for.
#[tokio::test(start_paused = true)]
async fn a_deadline_passed_with_only_a_call_open_is_reported_and_the_drain_goes_on() {
    let rig = Rig::new(timing(0, 30));
    let write = rig.backend.metrics().execute.open_calls.raise();
    assert!(rig.sequence.begin());

    tokio::time::sleep(Duration::from_secs(31)).await;
    assert!(
        rig.drain.deadline_passed(),
        "a call held the drain unreported"
    );
    assert!(rig.drain.sequence_finished().now_or_never().is_none());

    drop(write);
    rig.drain.sequence_finished().await;
    assert_eq!(rig.drain.phase(), Phase::Closing);
}

/// Catches a deadline that is reported with nothing open: a call that ended just before the
/// deadline passed (by hand, before the wait looks again) leaves `deadline_passed` unset, and
/// the drain ends.
#[tokio::test(start_paused = true)]
async fn a_deadline_passed_once_nothing_is_open_reports_nothing() {
    let rig = Rig::new(timing(0, 86_400));
    let write = rig.backend.metrics().execute.open_calls.raise();
    assert!(rig.sequence.begin());
    // Between two looks of the wait (every 50 ms), so the deadline is seen before the next one.
    tokio::time::sleep(Duration::from_millis(1_025)).await;
    assert_eq!(rig.drain.phase(), Phase::Draining);

    drop(write);
    rig.drain.pass_deadline();
    rig.drain.sequence_finished().await;
    assert!(!rig.drain.deadline_passed(), "reported with nothing open");
    assert_eq!(rig.drain.phase(), Phase::Closing);
}
