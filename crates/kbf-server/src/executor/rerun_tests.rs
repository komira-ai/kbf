//! The wait before an operation's next attempt: how it grows and is jittered, and, through the
//! executor and a fake job service that ends every job failed to start, that the next job is
//! submitted only after it, within the action's deadline, and never after a cancel. The clock is
//! tokio's paused one, moved by the rig, which moves the fake's with it.

use std::time::Duration;

use js_proto::v1::StartFailureCode;
use kbf_front::{Dispatch, Finished};
use kbf_job_client::fake::Fault;
use kbf_proto::google::rpc;
use tokio::time::Instant;
use tonic::Code;

use super::*;
use crate::executor::outcome::RESUBMITS;
use crate::executor::tests::{Caller, Rig};

/// The rig's poll, the executor's default.
const POLL: Duration = Duration::from_millis(250);

/// The wait before a first rerun at [`POLL`], without jitter.
const FIRST_WAIT: Duration = Duration::from_secs(2);

/// The longest wait at [`POLL`], without jitter.
const WAIT_CAP: Duration = Duration::from_secs(20);

/// How far the clock moves between two looks at the fake: finer than any wait asserted.
const STEP: Duration = Duration::from_millis(10);

fn status_of(finished: Finished) -> rpc::Status {
    match finished {
        Finished::Failed(status) => status,
        other => panic!("not a failure: {other:?}"),
    }
}

/// Moves the clock [`STEP`] at a time until the job of `attempt` of `caller`'s operation is at
/// the job service, and says when the fake first saw it, in its milliseconds: no later than a
/// step after the submit.
async fn submitted_at(rig: &Rig, caller: &Caller, attempt: u32) -> (String, u64) {
    let job = rig.job_of(caller, attempt);
    for _ in 0..10_000 {
        if rig.fake.submitted(&job).is_some() {
            return (job, rig.fake.now_ms());
        }
        rig.advance(STEP).await;
    }
    panic!("never submitted: {job}");
}

fn millis(wait: Duration) -> u64 {
    u64::try_from(wait.as_millis()).expect("a short wait")
}

/// Asserts the waits without jitter: [`FIRST_WAIT`] before the first rerun, doubling before each
/// one after, held at [`WAIT_CAP`] from the fifth on and however many reruns came before.
#[test]
fn the_wait_doubles_from_the_first_up_to_the_cap() {
    let waits: Vec<Duration> = (0..7).map(|reruns| wait(POLL, reruns, 0.0)).collect();
    let secs = Duration::from_secs;
    assert_eq!(
        waits,
        [
            secs(2),
            secs(4),
            secs(8),
            secs(16),
            secs(20),
            secs(20),
            secs(20)
        ]
    );
    assert_eq!(wait(POLL, 31, 0.0), WAIT_CAP);
    assert_eq!(wait(POLL, 32, 0.0), WAIT_CAP);
    assert_eq!(wait(POLL, u32::MAX, 0.0), WAIT_CAP);
}

/// Asserts the waits are counted in polls: at a poll of 5 ms the first is 40 ms and the cap
/// 400 ms; and a poll so long its waits do not fit a `Duration` gives the longest one, and no
/// deadline is met by it.
#[test]
fn the_waits_are_counted_in_polls() {
    let fast = Duration::from_millis(5);
    assert_eq!(wait(fast, 0, 0.0), Duration::from_millis(40));
    assert_eq!(wait(fast, 1, 0.0), Duration::from_millis(80));
    assert_eq!(wait(fast, 9, 0.0), Duration::from_millis(400));
    assert_eq!(wait(Duration::MAX, 0, 0.5), Duration::MAX);
    let now = Instant::now();
    let far = now + Duration::from_secs(3600);
    assert_eq!(before((Duration::MAX, 0, 0.0), now, far), None);
}

/// Asserts the jitter: a unit of `u` lengthens a wait by `u / 2` of it, so a wait is never
/// shorter than its base and shorter than half again of it; at the cap too. A unit outside
/// `[0, 1)` (1, a negative, NaN, infinity) is read as 0 and gives the base.
#[test]
fn the_jitter_lengthens_a_wait_by_up_to_half() {
    assert_eq!(wait(POLL, 0, 0.5), Duration::from_millis(2500));
    assert_eq!(wait(POLL, 1, 0.25), Duration::from_millis(4500));
    assert_eq!(wait(POLL, 9, 0.5), Duration::from_secs(25));
    let almost = wait(POLL, 0, 0.999_999);
    assert!(
        almost >= FIRST_WAIT && almost < FIRST_WAIT.mul_f64(1.5),
        "{almost:?}"
    );
    for unit in [1.0, -0.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(wait(POLL, 0, unit), FIRST_WAIT, "{unit}");
    }
}

/// Asserts the deadline: the wait is given when it ends before the deadline, and not when it
/// would end at it or after it.
#[test]
fn a_wait_that_would_end_at_or_after_the_deadline_is_not_given() {
    let now = Instant::now();
    let first = wait(POLL, 0, 0.0);
    let just_after = now + first + Duration::from_millis(1);
    assert_eq!(before((POLL, 0, 0.0), now, just_after), Some(first));
    assert_eq!(before((POLL, 0, 0.0), now, now + first), None);
    assert_eq!(before((POLL, 0, 0.0), now, now), None);
    assert_eq!(before((POLL, 1, 0.0), now, now + first + first), None);
}

/// Asserts the message of a run the deadline left no rerun after: the run's own message, then
/// the deadline, in full; and the code and details as they were.
#[test]
fn a_status_past_the_deadline_says_so_after_its_own_message() {
    let mut status = rpc::Status {
        code: Code::Unavailable as i32,
        message: "the job was lost with its host".to_owned(),
        details: Vec::new(),
    };
    past_deadline(&mut status, Duration::from_secs(60));
    assert_eq!(
        status,
        rpc::Status {
            code: Code::Unavailable as i32,
            message: "the job was lost with its host; the action's deadline (60s from when it \
                      was asked for) left no time for another attempt"
                .to_owned(),
            details: Vec::new(),
        }
    );
}

/// Asserts, against a job service that ends every job failed to start (`START_TIMEOUT`, an
/// `infra` ending), that the next job is submitted no sooner than the wait after the last ended:
/// the first gap is at least [`FIRST_WAIT`], the second at least twice it and longer than the
/// first; that the operation has its three attempts and no more; and that the callers are
/// answered the last run's status in full.
#[tokio::test(start_paused = true)]
async fn the_gaps_between_attempts_are_the_waits_and_grow() {
    assert_eq!(RESUBMITS, 2);
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let mut seen = Vec::new();
    for attempt in 0..=RESUBMITS {
        let (job, at) = submitted_at(&rig, &caller, attempt).await;
        assert!(rig.fake.fail_to_start(&job, StartFailureCode::StartTimeout));
        seen.push(at);
    }
    let status = status_of(rig.until_done(&caller).await);

    let gaps: Vec<u64> = seen.windows(2).map(|pair| pair[1] - pair[0]).collect();
    assert!(gaps[0] >= millis(FIRST_WAIT), "{gaps:?}");
    assert!(gaps[1] >= millis(FIRST_WAIT * 2), "{gaps:?}");
    assert!(gaps[1] > gaps[0], "{gaps:?}");
    assert_eq!(status.code, Code::Unavailable as i32);
    assert_eq!(status.message, "the job failed to start: start-timeout");
    rig.advance(Duration::from_secs(60)).await;
    assert_eq!(rig.fake.jobs().len(), 3);
    assert_eq!(rig.counted("kbf_executor_attempts_resubmitted_total "), 2);
}

/// Asserts that three attempts do not end within one poll when every job fails to start as soon
/// as it is submitted: after a poll the operation has one job and no answer, and its answer
/// comes no sooner than the two waits after the submit.
#[tokio::test(start_paused = true)]
async fn three_attempts_no_longer_end_within_one_poll() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let (_, first) = submitted_at(&rig, &caller, 0).await;
    let fail_all = || {
        for job in rig.fake.jobs() {
            rig.fake
                .fail_to_start(&job.id, StartFailureCode::StartTimeout);
        }
    };
    let mut moved = Duration::ZERO;
    while moved < rig.cfg.poll {
        fail_all();
        rig.advance(STEP).await;
        moved += STEP;
    }
    assert_eq!(rig.fake.jobs().len(), 1, "one job within a poll");
    assert!(caller.finished().is_none(), "no answer within a poll");

    for _ in 0..10_000 {
        if caller.finished().is_some() {
            break;
        }
        fail_all();
        rig.advance(STEP).await;
    }
    assert!(caller.finished().is_some(), "answered");
    let took = rig.fake.now_ms() - first;
    assert!(took >= millis(FIRST_WAIT * 3), "{took} ms");
    assert_eq!(rig.fake.jobs().len(), 3);
}

/// Asserts that the job service's own `NO_CAPACITY` (no host had room within the pool's queue
/// wait) is a refused start: placed again, three times, more than the operation's
/// [`RESUBMITS`], and the fourth job runs the action, counted as refused runs and not as `infra`.
#[tokio::test(start_paused = true)]
async fn the_job_services_no_capacity_is_placed_again_spending_no_attempt() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let refusals = RESUBMITS + 1;
    for attempt in 0..refusals {
        let job = rig.submitted(&caller, attempt).await;
        assert!(rig.fake.fail_to_start(&job, StartFailureCode::NoCapacity));
    }
    let last = rig.submitted(&caller, refusals).await;
    rig.host_finishes(&last, 0);

    let finished = rig.until_done(&caller).await;
    assert!(matches!(finished, Finished::Ran(ref result) if result.exit_code == 0));
    assert_eq!(rig.fake.jobs().len(), 4);
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"refused\"} "),
        3
    );
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"infra\"} "),
        0
    );
}

/// Asserts that a run ended for a reason of the farm's when the wait would end past the
/// action's deadline (its timeout of 60 s; the job ran 59 s and was lost) is not run again: the
/// callers are answered at once with its status and the deadline, in full, and no second job is
/// submitted.
#[tokio::test(start_paused = true)]
async fn a_rerun_whose_wait_passes_the_deadline_is_answered_instead() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    rig.host_runs(&job);
    rig.advance(Duration::from_secs(59)).await;
    assert!(rig.fake.lose(&job));

    let status = status_of(rig.until_done(&caller).await);
    assert_eq!(status.code, Code::Unavailable as i32);
    assert_eq!(
        status.message,
        "the job was lost with its host; the action's deadline (60s from when it was asked \
         for) left no time for another attempt"
    );
    rig.advance(Duration::from_secs(300)).await;
    assert_eq!(rig.fake.jobs().len(), 1);
    assert_eq!(rig.counted("kbf_executor_attempts_resubmitted_total "), 0);
}

/// Asserts that a caller's cancel is heard while the operation waits to run again: it is
/// answered `CANCELLED` before the wait ends, and no further job is submitted.
#[tokio::test(start_paused = true)]
async fn a_cancel_during_the_wait_ends_the_operation() {
    let rig = Rig::start().await;
    let caller = rig.submit(1);
    let (job, _) = submitted_at(&rig, &caller, 0).await;
    assert!(rig.fake.fail_to_start(&job, StartFailureCode::StartTimeout));
    rig.until("the run ended", || {
        rig.counted("kbf_executor_attempts_resubmitted_total ") == 1
    })
    .await;
    assert!(rig.exec.cancel(&caller.name));

    let finished = rig.until_done(&caller).await;
    assert!(
        matches!(finished, Finished::Failed(ref status) if status.code == Code::Cancelled as i32),
        "{finished:?}"
    );
    rig.advance(Duration::from_secs(60)).await;
    assert_eq!(rig.fake.jobs().len(), 1);
}

/// Asserts that a submit answered `UNAUTHENTICATED`, `PERMISSION_DENIED` or `UNIMPLEMENTED` (a
/// service that will not serve this frontend now) spends no attempt: the same job, of the first
/// attempt, is sent again until it is taken, and it runs the action.
#[tokio::test(start_paused = true)]
async fn a_submit_the_service_will_not_serve_now_is_sent_again_under_its_id() {
    let rig = Rig::start().await;
    let codes = [
        Code::Unauthenticated,
        Code::PermissionDenied,
        Code::Unimplemented,
    ];
    for code in codes {
        let message = "not now".to_owned();
        rig.fake
            .inject_on("SubmitJob", Fault::Status { code, message });
    }
    let caller = rig.submit(1);
    let job = rig.submitted(&caller, 0).await;
    rig.host_finishes(&job, 0);

    let finished = rig.until_done(&caller).await;
    assert!(matches!(finished, Finished::Ran(ref result) if result.exit_code == 0));
    assert_eq!(rig.calls_of("SubmitJob"), codes.len() + 1);
    assert_eq!(rig.fake.jobs().len(), 1);
    assert_eq!(rig.counted("kbf_executor_attempts_resubmitted_total "), 0);
}
