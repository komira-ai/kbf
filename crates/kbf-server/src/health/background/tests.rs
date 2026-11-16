//! The background prober, against probes the test plays. No test reads a clock: the tests that
//! look at the interval run on tokio's paused one.

use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;

use futures::FutureExt as _;
use futures::future::BoxFuture;

use super::*;
use crate::health::{Checks, Readiness};

/// What one ask of a [`Script`] does.
enum Step {
    Pass,
    Fail(&'static str),
    /// Never answers.
    Hang,
}

/// A probe that does the next of its steps at each ask, and passes once it has none left.
struct Script {
    steps: Mutex<VecDeque<Step>>,
    asked: Arc<AtomicUsize>,
}

impl Script {
    /// The probe, and the count of asks it has had.
    fn of(steps: impl IntoIterator<Item = Step>) -> (Arc<dyn Probe>, Arc<AtomicUsize>) {
        let asked = Arc::new(AtomicUsize::new(0));
        let probe = Self {
            steps: Mutex::new(steps.into_iter().collect()),
            asked: Arc::clone(&asked),
        };
        (Arc::new(probe), asked)
    }
}

impl Probe for Script {
    fn probe(&self) -> BoxFuture<'_, Result<(), String>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Step::Pass);
        Box::pin(async move {
            match step {
                Step::Pass => Ok(()),
                Step::Fail(reason) => Err(reason.to_owned()),
                Step::Hang => std::future::pending().await,
            }
        })
    }
}

/// The failing checks of `prober` with their reasons.
fn reasons(prober: &Prober) -> Vec<(&'static str, String)> {
    prober
        .failing(&[])
        .into_iter()
        .map(|failing| (failing.check, failing.reason))
        .collect()
}

/// Lets the other tasks of the test's runtime run until `asked` has reached `count`.
async fn until_asked(asked: &AtomicUsize, count: usize) {
    while asked.load(Ordering::SeqCst) < count {
        tokio::task::yield_now().await;
    }
}

/// Catches: a probe that never answers delaying `/readyz`. A check is asked in the background
/// and one ask is still open: `Checks::failing` answers at once from the last answer (and
/// it does not wait on the open ask). Before the probes moved to the background `failing` asked
/// the probe itself, so with a probe that takes longer than the load balancer's timeout (a store
/// that answers in 2 s) the answer took that long.
#[tokio::test]
async fn a_probe_that_never_answers_does_not_delay_the_answer() {
    let (probe, asked) = Script::of([Step::Pass, Step::Hang]);
    let checks = Checks::new(Arc::new(Readiness::default()))
        .probing("storage", probe)
        .in_background(3);
    let prober = checks.prober().expect("the probes are in the background");
    prober.round().await;
    let open = tokio::spawn(async move { prober.round().await });
    until_asked(&asked, 2).await;
    let answer = checks
        .failing()
        .now_or_never()
        .expect("the answer waited on the open probe");
    assert!(answer.is_empty(), "the last answer was a pass");
    open.abort();
}

/// Catches: a frontend that is ready before its probes have answered, and one that fails
/// before `fail_after` probes have failed. Until a probe passes, a check says `not probed
/// yet`, through failures that are fewer than `fail_after`; the failure that makes `fail_after`
/// in a row shows its own reason.
#[tokio::test]
async fn a_check_that_has_not_passed_is_not_probed_until_it_has_failed_enough() {
    let (probe, _) = Script::of([Step::Fail("first"), Step::Fail("second"), Step::Fail("third")]);
    let prober = Prober::new(vec![("storage", probe)], 3);
    let not_probed = vec![("storage", "not probed yet".to_owned())];
    assert_eq!(reasons(&prober), not_probed);
    prober.round().await;
    prober.round().await;
    assert_eq!(reasons(&prober), not_probed);
    prober.round().await;
    assert_eq!(reasons(&prober), [("storage", "third".to_owned())]);
}

/// Catches: one failed probe taking a ready frontend out of its load balancer. After a pass,
/// one failure and then a pass leave the check passing, and so do two failures after another
/// pass (the pass starts the count again); `fail_after` failures in a row make it fail with the
/// last reason, and the next pass clears it.
#[tokio::test]
async fn one_failed_probe_does_not_flip_the_check_but_fail_after_in_a_row_do() {
    let (probe, _) = Script::of([
        Step::Pass,
        Step::Fail("blip"),
        Step::Pass,
        Step::Fail("a"),
        Step::Fail("b"),
        Step::Fail("c"),
        Step::Pass,
    ]);
    let prober = Prober::new(vec![("storage", probe)], 3);
    for round in 1..=5 {
        prober.round().await;
        assert!(reasons(&prober).is_empty(), "failing in round {round}");
    }
    prober.round().await;
    assert_eq!(reasons(&prober), [("storage", "c".to_owned())]);
    prober.round().await;
    assert!(reasons(&prober).is_empty(), "a pass clears the check");
}

/// Catches: probes asked one after the other, so that one that hangs holds the answers of the
/// others. The `storage` probe never answers; the `state_store` probe fails and its failure
/// shows while the storage one is still open.
#[tokio::test]
async fn the_probes_are_asked_at_the_same_time() {
    let (storage, _) = Script::of([Step::Hang]);
    let (state, asked) = Script::of([Step::Fail("down")]);
    let prober = Arc::new(Prober::new(
        vec![("storage", storage), ("state_store", state)],
        1,
    ));
    let round = tokio::spawn({
        let prober = Arc::clone(&prober);
        async move { prober.round().await }
    });
    until_asked(&asked, 1).await;
    assert_eq!(
        reasons(&prober),
        [
            ("storage", "not probed yet".to_owned()),
            ("state_store", "down".to_owned())
        ]
    );
    round.abort();
}

/// Catches: a probe that is asked once and never again, or asked ahead of its interval. On a
/// clock that only moves when the test moves it: nothing is asked before an interval has
/// passed, then each interval asks once more, and the answers follow.
#[tokio::test(start_paused = true)]
async fn each_probe_is_asked_again_after_the_interval() {
    let (probe, asked) = Script::of([Step::Fail("down"), Step::Pass]);
    let prober = Arc::new(Prober::new(vec![("storage", probe)], 1));
    let interval = Duration::from_secs(10);
    let run = tokio::spawn(Arc::clone(&prober).run(interval));
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(asked.load(Ordering::SeqCst), 0, "asked before an interval");
    tokio::time::advance(interval).await;
    until_asked(&asked, 1).await;
    assert_eq!(reasons(&prober), [("storage", "down".to_owned())]);
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(asked.load(Ordering::SeqCst), 1, "asked twice in an interval");
    tokio::time::advance(interval).await;
    until_asked(&asked, 2).await;
    assert!(reasons(&prober).is_empty());
    run.abort();
}

/// Catches: a `fail_after` of 0 that never lets a check fail or counts it wrong: it is 1.
#[tokio::test]
async fn a_fail_after_of_zero_is_one() {
    let (probe, _) = Script::of([Step::Fail("down")]);
    let prober = Prober::new(vec![("storage", probe)], 0);
    prober.round().await;
    assert_eq!(reasons(&prober), [("storage", "down".to_owned())]);
}

/// Catches: `READY=1`'s skipped checks being reported anyway, and the probes added after
/// `in_background` not being asked at each answer: `failing_except` leaves a skipped check out
/// of the background answers, and a probe added after is asked every time, listed after.
#[tokio::test]
async fn skipped_checks_are_left_out_and_later_probes_are_asked_each_time() {
    let (storage, _) = Script::of([Step::Fail("down")]);
    let (direct, asked) = Script::of([Step::Fail("one"), Step::Fail("two")]);
    let checks = Checks::new(Arc::new(Readiness::default()))
        .probing("storage", storage)
        .in_background(1)
        .probing("deny_rows", direct);
    checks.prober().expect("background").round().await;
    let names = |failing: Vec<Failing>| {
        failing
            .into_iter()
            .map(|failing| (failing.check, failing.reason))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(checks.failing().await),
        [("storage", "down".to_owned()), ("deny_rows", "one".to_owned())]
    );
    assert_eq!(
        names(checks.failing_except(&["storage"]).await),
        [("deny_rows", "two".to_owned())]
    );
    assert_eq!(asked.load(Ordering::SeqCst), 2);
}

/// Catches: a handed-over frontend reporting its probes (it stood its roles down, and its
/// probes may fail while it still answers calls): with the probes in the background it still
/// lists only `stopping` and `leader`.
#[tokio::test]
async fn a_handed_over_frontend_reports_no_background_probe() {
    let (storage, _) = Script::of([Step::Fail("down")]);
    let readiness = Arc::new(Readiness::default());
    let checks = Checks::new(Arc::clone(&readiness))
        .probing("storage", storage)
        .in_background(1);
    assert_eq!(checks.failing().await.len(), 1);
    readiness.set_handed_over();
    assert!(checks.failing().await.is_empty());
}

/// Catches: transitions that are not counted by check and direction, or not at all. A check
/// that fails at its start (`not probed yet` to a failure), passes, and fails again is counted
/// once failing and once ready; a failure under the limit counts nothing.
#[tokio::test]
async fn transitions_are_counted_by_check_and_direction() {
    let (storage, _) = Script::of([
        Step::Fail("down"),
        Step::Pass,
        Step::Fail("blip"),
        Step::Fail("down again"),
    ]);
    let (map, _) = Script::of([]);
    let prober = Prober::new(vec![("storage", storage), ("map", map)], 2);
    let mut text = String::new();
    prober.render(&mut text);
    let header = "# HELP kbf_readyz_transitions_total Times a readiness check went from passing to failing (to=\"failing\") or from failing to passing (to=\"ready\"; the first pass counts)\n# TYPE kbf_readyz_transitions_total counter\n";
    let counts = |storage: [u64; 2], map: [u64; 2]| {
        format!(
            "{header}kbf_readyz_transitions_total{{check=\"storage\",to=\"failing\"}} {}\nkbf_readyz_transitions_total{{check=\"storage\",to=\"ready\"}} {}\nkbf_readyz_transitions_total{{check=\"map\",to=\"failing\"}} {}\nkbf_readyz_transitions_total{{check=\"map\",to=\"ready\"}} {}\n",
            storage[0], storage[1], map[0], map[1]
        )
    };
    assert_eq!(text, counts([0, 0], [0, 0]));
    // The storage check fails once (under the limit of 2): nothing changes; the map passes.
    prober.round().await;
    let mut text = String::new();
    prober.render(&mut text);
    assert_eq!(text, counts([0, 0], [0, 1]));
    // Storage passes: its first pass.
    prober.round().await;
    // One failure: still passing. The second makes the limit: it began to fail.
    prober.round().await;
    prober.round().await;
    let mut text = String::new();
    prober.render(&mut text);
    assert_eq!(text, counts([1, 1], [0, 1]));
}
