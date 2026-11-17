//! The arms of a task's submit, each taken by a test here: a `Start` taken up and prepared past
//! its window, one first seen past it, a submit whose answer is lost again after it was already
//! sent, and a refusal that is the caller's and no twin's. The executor is built twice for coverage (in these unit tests
//! and in the farm-level tests of `tests/executor`), and a branch counts as covered only when
//! one build takes both its sides, so these take them here.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use kbf_proto::reapi::Digest;
use kbf_proto::worker::Start;
use kbf_types::LeaseId;
use tokio::sync::Notify;
use tonic::Code;

use super::{Stage, counted, reported, status_of, step, submitted_specs, tell_start};
use crate::executor::Secret;
use crate::executor::host::HostWorker;
use crate::executor::test_support::{Harness, Log, built_class, start};
use crate::executor::traits::{ActionFacts, Inputs, InputsError, ReadGrant};

/// Catches a `Start` dropped because its preparation was slower than its window: the farm then
/// gave the lease up only after its start grace, and the action paid a whole second attempt.
/// Taken up inside its window, the `Start` is listed from that pass on; its preparation ends 10 s
/// past the window and its job is submitted then, once, on its own lease, with nothing dropped.
#[tokio::test(start_paused = true)]
async fn a_start_prepared_past_its_window_is_submitted_on_its_lease() {
    let mut harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let gate = Gate::new(false);
    harness.deps.inputs = Arc::new(Arc::clone(&gate));
    let lease = LeaseId::new(1, 1);
    let window = Duration::from_millis(start(1, 1).valid_for_ms);
    tell_start(&messages, start(1, 1));
    step_ticking(&harness, &mut worker).await;
    assert_eq!(worker.listed(), [lease]);
    tokio::time::advance(window + Duration::from_secs(10)).await;
    step_ticking(&harness, &mut worker).await;
    assert_eq!(
        worker.listed(),
        [lease],
        "the Start was dropped at its window"
    );
    assert_eq!(submitted_specs(&harness).len(), 0);
    gate.release.notify_one();
    gate.done.notified().await;
    step_ticking(&harness, &mut worker).await;
    assert!(matches!(
        worker.tasks.get(&lease).map(|task| &task.stage),
        Some(Stage::Running(_))
    ));
    assert_eq!(worker.listed(), [lease]);
    assert_eq!(submitted_specs(&harness).len(), 1);
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

/// Asserts that a preparation whose `Start` the farm cancelled is not stopped with it: the task
/// is dropped and no longer listed, and the preparation still runs to its end, so what it wrote
/// (the input tree, kept by the writer's memo) is there when the farm runs the action again, and
/// nothing is submitted for the cancelled lease. Catches a preparation stopped with its task,
/// which redid the same work for every lease of the action.
#[tokio::test(start_paused = true)]
async fn a_cancelled_start_drops_its_task_and_its_preparation_runs_to_its_end() {
    let mut harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let gate = Gate::new(false);
    harness.deps.inputs = Arc::new(Arc::clone(&gate));
    let lease = LeaseId::new(1, 1);
    tell_start(&messages, start(1, 1));
    step_ticking(&harness, &mut worker).await;
    super::tell_cancel(&messages, 1, 1);
    step_ticking(&harness, &mut worker).await;
    assert!(
        !worker.tasks.contains_key(&lease),
        "the cancelled task is kept"
    );
    assert!(worker.listed().is_empty());
    assert_eq!(gate.finished.load(Ordering::SeqCst), 0);
    gate.release.notify_one();
    gate.done.notified().await;
    assert_eq!(gate.finished.load(Ordering::SeqCst), 1);
    step_ticking(&harness, &mut worker).await;
    assert_eq!(submitted_specs(&harness).len(), 0);
}

/// Catches a `Start` dropped at its window because it waited for a place under
/// `prepare_concurrency` (`not_begun`, the drops of a worker given more `Start`s than it prepares
/// at once): with room for one preparation, two `Start`s are both taken up and listed, still
/// listed past their window, and the second begins its preparation when the first is done; both
/// jobs are submitted on their leases, and nothing is dropped.
#[tokio::test(start_paused = true)]
async fn a_start_waiting_for_a_place_to_prepare_is_listed_and_prepared_in_turn() {
    let mut harness = Harness::new().await;
    harness.cfg.prepare_concurrency = 1;
    let (mut worker, messages) = harness.worker();
    let gate = Gate::new(false);
    harness.deps.inputs = Arc::new(Arc::clone(&gate));
    let window = Duration::from_millis(start(1, 1).valid_for_ms);
    let leases = [LeaseId::new(1, 1), LeaseId::new(1, 2)];
    tell_start(&messages, start(1, 1));
    tell_start(&messages, start(1, 2));
    step_ticking(&harness, &mut worker).await;
    assert_eq!(worker.listed(), leases);
    assert_eq!(gate.begun.load(Ordering::SeqCst), 1);
    tokio::time::advance(window + Duration::from_secs(1)).await;
    let log = Log::default();
    let guard = log.install();
    step_ticking(&harness, &mut worker).await;
    drop(guard);
    assert!(!log.text().contains("dropped"), "{}", log.text());
    assert_eq!(worker.listed(), leases);
    for prepared in 1..=2 {
        gate.release.notify_one();
        gate.done.notified().await;
        tokio::task::yield_now().await;
        step_ticking(&harness, &mut worker).await;
        assert_eq!(submitted_specs(&harness).len(), prepared);
    }
    assert_eq!(gate.begun.load(Ordering::SeqCst), 2);
    assert_eq!(worker.listed(), leases);
    assert_eq!(counted(&harness, "kbf_executor_late_starts_total"), 0);
}

/// Catches a `Start` dropped past its window and left to the farm's start grace, and a drop
/// warning that does not say why: a `Start` the worker first looks at 15 s after the heartbeat it
/// names is never taken up, nothing of it is submitted, and its lease is reported at once as
/// infrastructure (listed until the farm acknowledges it), with the warning naming the stage
/// `not_begun` and the age of the heartbeat, and the run counted as an infrastructure one.
#[tokio::test(start_paused = true)]
async fn a_start_first_seen_past_its_window_is_reported_at_once() {
    let harness = Harness::new().await;
    let (mut worker, messages) = harness.worker();
    let lease = LeaseId::new(1, 1);
    tell_start(&messages, start(1, 1));
    tokio::time::advance(Duration::from_secs(15)).await;
    let log = Log::default();
    let guard = log.install();
    step(&harness, &mut worker).await;
    drop(guard);
    let (code, message) = status_of(reported(&worker, lease));
    assert_eq!(
        (code, message),
        (
            Code::Unavailable as i32,
            "the Start's window ended before its job was submitted"
        )
    );
    assert_eq!(worker.listed(), [lease]);
    let (stage, age) = (log.field("stage"), log.field("beat_age_ms"));
    assert_eq!(stage.as_deref(), Some("not_begun"), "{}", log.text());
    assert_eq!(age.as_deref(), Some("15000"), "{}", log.text());
    assert!(harness.fake.calls().is_empty());
    assert_eq!(counted(&harness, "kbf_executor_late_starts_total"), 1);
    assert_eq!(
        counted(&harness, "kbf_executor_runs_total{outcome=\"infra\"}"),
        1
    );
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

/// Asserts what a heartbeat lists of a `Start`: nothing before a pass has looked at it, and its
/// lease from the pass that takes it up, while its preparation runs. Catches a `Start` listed
/// before the worker has judged its window, and one left out while it prepares (which the farm
/// gives up after its start grace).
#[tokio::test(start_paused = true)]
async fn a_start_is_listed_from_the_pass_that_takes_it_up() {
    let mut harness = Harness::new().await;
    let (mut worker, _messages) = harness.worker();
    harness.deps.inputs = Arc::new(Gate::new(false));
    worker.accept(&start(1, 1));
    assert!(worker.listed().is_empty(), "listed before it was taken up");
    step_ticking(&harness, &mut worker).await;
    assert_eq!(worker.listed(), [LeaseId::new(1, 1)]);
    assert_eq!(submitted_specs(&harness).len(), 0);
}
