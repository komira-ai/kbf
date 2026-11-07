//! The arms of a task's submit, each taken by a test here: the window judged after the
//! prepare, a submit whose answer is lost again after it was already sent, and a refusal that
//! is the caller's and no twin's. The executor is built twice for coverage (in these unit tests
//! and in the farm-level tests of `tests/executor`), and a branch counts as covered only when
//! one build takes both its sides, so these take them here.

use std::sync::Arc;
use std::time::Duration;

use js_proto::v1::JobState;
use kbf_proto::reapi::Digest;
use kbf_proto::worker::Start;
use kbf_types::LeaseId;
use tokio::sync::Notify;
use tokio::time::Instant;
use tonic::Code;

use super::{Stage, counted, reported, status_of, step, submitted_specs, tell_start};
use crate::executor::Secret;
use crate::executor::test_support::{GIB, Harness, Log, start};
use crate::executor::traits::{ActionFacts, Inputs, InputsError, ReadGrant};

/// An action read that ends at `until`, on the test's held clock: the prepare of a task
/// takes exactly as long as the test says. It says when the read has begun (`begun`), so the
/// test moves the clock only then.
struct ReadUntil {
    until: Instant,
    begun: Arc<Notify>,
}

#[tonic::async_trait]
impl Inputs for ReadUntil {
    async fn action(&self, _: &Digest) -> Result<ActionFacts, InputsError> {
        self.begun.notify_one();
        tokio::time::sleep_until(self.until).await;
        Ok(ActionFacts {
            image: format!("registry.test/app@sha256:{}", "ab".repeat(32)),
            timeout: Some(Duration::from_secs(20)),
        })
    }

    async fn read_grant(
        &self,
        _: LeaseId,
        _: &Digest,
        _: Duration,
    ) -> Result<ReadGrant, InputsError> {
        Ok(ReadGrant {
            url: "https://grant.test:9400".to_owned(),
            token: Secret::new("token"),
            input_bytes: Some(3 * GIB),
        })
    }
}

/// A `Start` of lease 1.1 whose action read ends `before` the end of its window (counted
/// from the worker's first heartbeat, at its start, for the `Start`'s `valid_for_ms`), and
/// what came of it after one step: the harness, whether the task is running, and what the step
/// logged (a subscriber of its own, so the log's macros run as they do in a server). The
/// harness holds the clock, so the test moves it: once the read has begun, to the read's end.
async fn prepared_before_the_end(before: Duration) -> (Harness, bool, String) {
    let mut harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let start = start(1, 1);
    let until = Instant::now() + Duration::from_millis(start.valid_for_ms) - before;
    let begun = Arc::new(Notify::new());
    harness.deps.inputs = Arc::new(ReadUntil {
        until,
        begun: Arc::clone(&begun),
    });
    tell_start(&messages, start);
    let read_ends = async {
        begun.notified().await;
        tokio::time::advance(until - Instant::now()).await;
    };
    let log = Log::default();
    let guard = log.install();
    tokio::join!(step(&harness, &mut worker), read_ends);
    drop(guard);
    let running = matches!(
        worker
            .tasks
            .get(&LeaseId::new(1, 1))
            .map(|task| &task.stage),
        Some(Stage::Running(_))
    );
    (harness, running, log.text())
}

/// What the worker logs when it drops a `Start` whose window ended before its job was sent.
const DROPPED: &str = "a Start ran out of its window before its job was submitted and is dropped";

/// Catches a window judged only by the timeout around the prepare: a prepare that ends at
/// the very instant the window ends finishes inside that timeout, and its job must still
/// not be submitted, as the `Start` is no longer valid. It is dropped, logged and counted
/// late, and the service is asked for nothing. Control: a prepare that ends a millisecond
/// before the end is submitted and its job runs, and nothing is dropped.
#[tokio::test(start_paused = true)]
async fn a_task_prepared_at_the_end_of_its_window_is_not_submitted() {
    let (harness, running, log) = prepared_before_the_end(Duration::ZERO).await;
    assert!(!running, "a job was submitted for a Start past its window");
    assert!(log.contains(DROPPED), "{log}");
    assert_eq!(submitted_specs(&harness).len(), 0);
    assert!(harness.fake.jobs().is_empty());
    assert_eq!(counted(&harness, "kbf_executor_late_starts_total"), 1);

    let (harness, running, log) = prepared_before_the_end(Duration::from_millis(1)).await;
    assert!(
        running,
        "control: a Start prepared inside its window is not running"
    );
    assert_eq!(submitted_specs(&harness).len(), 1);
    assert_eq!(harness.fake.jobs()[0].state(), JobState::Queued);
    assert!(!log.contains(DROPPED), "{log}");
    assert_eq!(counted(&harness, "kbf_executor_late_starts_total"), 0);
}

/// Catches a task that forgets it already sent its job when a second submit is not answered
/// either: it is still the job that may exist, so the task keeps waiting with `sent`, sends
/// the same job a third time, and runs it; the service holds one job and both resends are
/// counted.
#[tokio::test(start_paused = true)]
async fn a_submit_lost_twice_is_still_a_job_that_was_sent() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let lease = LeaseId::new(1, 1);
    tell_start(&messages, start(1, 1));
    for _ in 0..2 {
        harness.fake.lose_next_answer();
        step(&harness, &mut worker).await;
        let Some(Stage::Waiting(waiting)) = worker.tasks.get(&lease).map(|task| &task.stage) else {
            panic!("the task is not waiting");
        };
        assert!(waiting.sent);
    }
    step(&harness, &mut worker).await;
    assert!(matches!(
        worker.tasks.get(&lease).map(|task| &task.stage),
        Some(Stage::Running(_))
    ));
    let sent = submitted_specs(&harness);
    assert_eq!(sent.len(), 3);
    assert!(sent.iter().all(|spec| *spec == sent[0]));
    assert_eq!(harness.fake.jobs().len(), 1);
    assert_eq!(
        counted(
            &harness,
            "kbf_executor_jobs_resubmitted_after_lost_answer_total"
        ),
        2
    );
}

/// Catches a refusal that is the caller's taken for a twin, or retried: a `Start` no host of
/// the service could ever hold (more memory than its one host has) is refused
/// `no-capable-host` at its one submit, settled as the caller's, and the frontend is not
/// flagged.
#[tokio::test(start_paused = true)]
async fn a_start_no_host_can_hold_is_refused_once_and_flags_no_twin() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    tell_start(
        &messages,
        Start {
            memory_bytes: 128 * GIB,
            ..start(1, 1)
        },
    );
    step(&harness, &mut worker).await;
    let (code, message) = status_of(reported(&worker, LeaseId::new(1, 1)));
    assert_eq!(
        (code, message),
        (
            Code::InvalidArgument as i32,
            "the job service refused the job: no-capable-host"
        )
    );
    assert_eq!(harness.twin.take(), None);
    assert_eq!(submitted_specs(&harness).len(), 1);
    assert!(harness.fake.jobs().is_empty());
}
