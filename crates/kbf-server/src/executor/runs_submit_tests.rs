//! The arms of a task's submit, each taken by a test here: the window judged after the
//! prepare, a submit whose answer is lost again after it was already sent, and a refusal that
//! is the caller's and no twin's. The executor is built twice for coverage (in these unit tests
//! and in the farm-level tests of `tests/executor`), and a branch counts as covered only when
//! one build takes both its sides, so these take them here.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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
use crate::executor::host::HostWorker;
use crate::executor::test_support::{Harness, Log, built_class, start};
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
            system: crate::executor::System::Linux,
            size_class: built_class(),
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
    // The pass begins 200 ms before the window ends, so the preparation ends inside the one
    // poll the pass waits for it, on either side of the window's end.
    tokio::time::advance(until + before - Instant::now() - Duration::from_millis(200)).await;
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

/// What the worker logs of a `Start` whose preparation ended at the very end of its window: the
/// stage, and the window as the age of the heartbeat it names.
const PREPARED_LATE: &str = "stage=prepared_late beat_age_ms=14000";

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
    assert!(log.contains(PREPARED_LATE), "{log}");
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
/// the service could ever hold (an architecture its one host is not) is refused
/// `no-capable-host` at its one submit, settled as the caller's, and the frontend is not
/// flagged.
#[tokio::test(start_paused = true)]
async fn a_start_no_host_can_hold_is_refused_once_and_flags_no_twin() {
    let mut harness = Harness::new().await;
    harness.cfg.arch = "aarch64".to_owned();
    let (mut worker, messages) = harness.worker();
    tell_start(&messages, start(1, 1));
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

/// One step of the worker, with the held clock moved on in small ticks for as long as it runs:
/// a pass that waits on the clock for something that never comes ends when the ticks reach its
/// bound, as it does on a real clock, and is not a hang.
async fn step_ticking(harness: &Harness, worker: &mut HostWorker) {
    let ticking = async {
        loop {
            tokio::time::advance(Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        () = step(harness, worker) => {}
        () = ticking => unreachable!("the ticks end only with the step"),
    }
}

/// An action read that ends when the test lets it (`release`), or panics when it does: the
/// preparation of a task takes as long as the test says, on no clock. `finished` counts the
/// reads that ran to their end, and `done` says when one has.
struct Gate {
    release: Notify,
    done: Notify,
    finished: AtomicUsize,
    /// The reads that began, ended or not.
    begun: AtomicUsize,
    panics: bool,
}

impl Gate {
    fn new(panics: bool) -> Arc<Self> {
        Arc::new(Self {
            release: Notify::new(),
            done: Notify::new(),
            finished: AtomicUsize::new(0),
            begun: AtomicUsize::new(0),
            panics,
        })
    }
}

#[tonic::async_trait]
impl Inputs for Arc<Gate> {
    async fn action(&self, _: &Digest) -> Result<ActionFacts, InputsError> {
        self.begun.fetch_add(1, Ordering::SeqCst);
        self.release.notified().await;
        assert!(!self.panics, "the read of the action failed hard");
        self.finished.fetch_add(1, Ordering::SeqCst);
        self.done.notify_one();
        Ok(ActionFacts {
            image: format!("registry.test/app@sha256:{}", "ab".repeat(32)),
            timeout: Some(Duration::from_secs(20)),
            system: crate::executor::System::Linux,
            size_class: built_class(),
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
        })
    }
}

/// Asserts that a preparation that is not done does not hold the pass: the step returns with
/// the task still waiting, and when the preparation is done a later step submits its job inside
/// the window. A worker's pass is joined with the other workers' (`Executor::step`), so a
/// pass held by one slow preparation held every worker's heartbeat and every other `Start` for
/// the whole window. Catches a preparation awaited inside the pass until the window ends.
#[tokio::test(start_paused = true)]
async fn a_slow_preparation_does_not_hold_the_pass() {
    let mut harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let gate = Gate::new(false);
    harness.deps.inputs = Arc::new(Arc::clone(&gate));
    let lease = LeaseId::new(1, 1);
    tell_start(&messages, start(1, 1));
    step_ticking(&harness, &mut worker).await;
    assert!(
        matches!(
            worker.tasks.get(&lease).map(|task| &task.stage),
            Some(Stage::Waiting(_))
        ),
        "the task is not still waiting for its preparation"
    );
    assert_eq!(submitted_specs(&harness).len(), 0);
    gate.release.notify_one();
    step_ticking(&harness, &mut worker).await;
    assert!(matches!(
        worker.tasks.get(&lease).map(|task| &task.stage),
        Some(Stage::Running(_))
    ));
    assert_eq!(submitted_specs(&harness).len(), 1);
    assert_eq!(counted(&harness, "kbf_executor_late_starts_total"), 0);
}

/// Asserts that a preparation that outlasts its `Start`'s window is not stopped with it: the
/// `Start` is dropped late, and the preparation still runs to its end, so what it wrote (the
/// input tree, kept by the writer's memo) is there when the farm runs the action again.
/// Catches a preparation cancelled at the window, which redid the same work for every lease of
/// an action whose tree took longer than the window, and never finished it.
#[tokio::test(start_paused = true)]
async fn a_preparation_outlasting_the_window_runs_to_its_end() {
    let mut harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let gate = Gate::new(false);
    harness.deps.inputs = Arc::new(Arc::clone(&gate));
    let lease = LeaseId::new(1, 1);
    let window = Duration::from_millis(start(1, 1).valid_for_ms);
    tell_start(&messages, start(1, 1));
    step_ticking(&harness, &mut worker).await;
    tokio::time::advance(window + Duration::from_secs(1)).await;
    let log = Log::default();
    let guard = log.install();
    step_ticking(&harness, &mut worker).await;
    drop(guard);
    assert!(!worker.tasks.contains_key(&lease), "the late task is kept");
    let stage = log.field("stage");
    assert_eq!(stage.as_deref(), Some("preparing"), "{}", log.text());
    assert_eq!(counted(&harness, "kbf_executor_late_starts_total"), 1);
    assert_eq!(gate.finished.load(Ordering::SeqCst), 0);
    gate.release.notify_one();
    gate.done.notified().await;
    assert_eq!(gate.finished.load(Ordering::SeqCst), 1);
    assert_eq!(submitted_specs(&harness).len(), 0);
}

/// Asserts that the warning of a dropped `Start` says where the `Start` stood and how old the
/// heartbeat it names was: with room for one preparation at a time, two `Start`s whose
/// preparation never ends are dropped past the window, the one that began as `preparing` and
/// the one that never did as `not_begun`, each with the age of the registration heartbeat
/// they name (the window and the second after it). Catches a drop warning that names neither,
/// which left a farm's drops unexplained (a preparation slower than the window, or a worker
/// given more `Start`s than it prepares at once).
#[tokio::test(start_paused = true)]
async fn a_dropped_start_is_logged_with_its_stage_and_the_age_of_its_heartbeat() {
    let mut harness = Harness::new().await;
    harness.cfg.prepare_concurrency = 1;
    let (mut worker, messages) = harness.worker();
    harness.deps.inputs = Arc::new(Gate::new(false));
    let window = Duration::from_millis(start(1, 1).valid_for_ms);
    tell_start(&messages, start(1, 1));
    tell_start(&messages, start(1, 2));
    step_ticking(&harness, &mut worker).await;
    tokio::time::advance(window + Duration::from_secs(1)).await;
    let log = Log::default();
    let guard = log.install();
    step_ticking(&harness, &mut worker).await;
    drop(guard);
    let text = log.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("dropped"))
        .collect();
    assert_eq!(lines.len(), 2, "{text}");
    for (line, stage) in lines.iter().zip(["preparing", "not_begun"]) {
        assert!(line.contains(&format!("stage={stage} ")), "{text}");
        let age: u64 = line
            .split_once("beat_age_ms=")
            .and_then(|(_, tail)| tail.split_whitespace().next())
            .and_then(|age| age.parse().ok())
            .unwrap_or_else(|| panic!("no age of the heartbeat in {text}"));
        assert!((15_000..20_000).contains(&age), "{age} in {text}");
    }
    assert_eq!(counted(&harness, "kbf_executor_late_starts_total"), 2);
}

/// Asserts that a `Start` whose job was prepared and whose submit the service did not answer
/// is logged as `submit_unanswered` when its window ends. Catches a drop that blames the
/// preparation for a submit that waited.
#[tokio::test(start_paused = true)]
async fn a_start_dropped_with_its_submit_unanswered_says_so() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    harness.fake.lose_next_answer();
    tell_start(&messages, start(1, 1));
    step(&harness, &mut worker).await;
    tokio::time::advance(Duration::from_secs(15)).await;
    harness.fake.lose_next_answer();
    let log = Log::default();
    let guard = log.install();
    step(&harness, &mut worker).await;
    drop(guard);
    let stage = log.field("stage");
    let text = log.text();
    assert_eq!(stage.as_deref(), Some("submit_unanswered"), "{text}");
}

/// Asserts that a `Start` that names a heartbeat the worker never sent is logged as not begun,
/// with no age of the heartbeat (`none`): there is none to age. Catches an age of zero or a
/// panic for a heartbeat that is not there.
#[tokio::test(start_paused = true)]
async fn a_start_naming_no_heartbeat_is_logged_with_no_age() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    tell_start(
        &messages,
        Start {
            heartbeat_seq: 99,
            ..start(1, 1)
        },
    );
    let log = Log::default();
    let guard = log.install();
    step(&harness, &mut worker).await;
    drop(guard);
    let (stage, age) = (log.field("stage"), log.field("beat_age_ms"));
    assert_eq!(stage.as_deref(), Some("not_begun"), "{}", log.text());
    assert_eq!(age.as_deref(), Some("none"), "{}", log.text());
}

/// Asserts that a preparation is bounded by `prepare_limit` (not by the window): one that does
/// not end by then is an infrastructure failure the farm runs again, with the limit named.
#[tokio::test(start_paused = true)]
async fn a_preparation_that_never_ends_fails_at_the_prepare_limit() {
    let mut harness = Harness::new().await;
    harness.cfg.prepare_limit = Duration::from_secs(2);
    let (mut worker, messages) = harness.worker();
    harness.deps.inputs = Arc::new(Gate::new(false));
    tell_start(&messages, start(1, 1));
    step_ticking(&harness, &mut worker).await;
    tokio::time::advance(Duration::from_secs(3)).await;
    step_ticking(&harness, &mut worker).await;
    let (code, message) = status_of(reported(&worker, LeaseId::new(1, 1)));
    assert_eq!(
        (code, message),
        (
            Code::Unavailable as i32,
            "the inputs of the run were not ready within 2s"
        )
    );
}

/// Asserts that a preparation that panics is the farm's to run again, not a lost task: the
/// result is an infrastructure failure.
#[tokio::test(start_paused = true)]
async fn a_preparation_that_panics_is_an_infrastructure_failure() {
    let mut harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let gate = Gate::new(true);
    harness.deps.inputs = Arc::new(Arc::clone(&gate));
    tell_start(&messages, start(1, 1));
    gate.release.notify_one();
    step_ticking(&harness, &mut worker).await;
    let (code, message) = status_of(reported(&worker, LeaseId::new(1, 1)));
    assert_eq!(code, Code::Unavailable as i32);
    assert!(
        message.starts_with("the preparation of the run ended without an answer"),
        "{message}"
    );
}

/// Asserts that a worker prepares only `prepare_concurrency` of its runs at once, and that the
/// ones past it begin as the ones before them end: five `Start`s given at one instant begin two
/// preparations; when one is done its job is submitted and the next `Start` begins its own, so
/// the preparations that began are finished inside the window, in the order the `Start`s came.
/// Catches every `Start` of a full set preparing together, each slowed by the rest until none
/// is ready before the window ends (the farm's three first sets of 48 `Start`s were all dropped).
#[tokio::test(start_paused = true)]
async fn a_worker_prepares_only_so_many_runs_at_once() {
    let mut harness = Harness::new().await;
    harness.cfg.prepare_concurrency = 2;
    let (mut worker, messages) = harness.worker();
    let gate = Gate::new(false);
    harness.deps.inputs = Arc::new(Arc::clone(&gate));
    for seq in 1..=5 {
        tell_start(&messages, start(1, seq));
    }
    step_ticking(&harness, &mut worker).await;
    assert_eq!(gate.begun.load(Ordering::SeqCst), 2);
    assert_eq!(submitted_specs(&harness).len(), 0);
    gate.release.notify_one();
    gate.done.notified().await;
    tokio::task::yield_now().await;
    step_ticking(&harness, &mut worker).await;
    assert_eq!(submitted_specs(&harness).len(), 1);
    assert_eq!(gate.begun.load(Ordering::SeqCst), 3);
    assert_eq!(counted(&harness, "kbf_executor_late_starts_total"), 0);
}
