//! The line each job the service takes writes: one per job, with its ids and the times of its
//! start as the frontend saw them, from the Execute's arrival at the front; none for a submit the
//! service refused or a run cancelled before its job was taken.

use std::sync::atomic::Ordering;

use kbf_front::{Arrival, Dispatch, Finished, Submission};
use kbf_job_client::fake::Fault;
use tonic::Code;

use super::*;
use crate::executor::job_id;
use crate::executor::tests::{Caller, Rig};
use crate::test_log;

/// The message of the line.
const LINE: &str = "the job service took the job";

/// The lines of `text` that are the submitted line.
fn lines(text: &str) -> Vec<&str> {
    text.lines().filter(|line| line.contains(LINE)).collect()
}

/// The value of `name=` in `line`.
fn field<'a>(line: &'a str, name: &str) -> &'a str {
    let key = format!(" {name}=");
    let (_, rest) = line
        .split_once(&key)
        .unwrap_or_else(|| panic!("no {name} in {line}"));
    rest.split(' ').next().unwrap_or_default()
}

/// The value of `name=` in `line`, a number.
fn number(line: &str, name: &str) -> u128 {
    field(line, name)
        .parse()
        .unwrap_or_else(|error| panic!("{name} in {line}: {error}"))
}

/// The milliseconds of tokio's paused clock since `t0`: the test's own clock, moved only by
/// the rig, so the values compared with it are exact, not bounds.
fn ms_since(t0: Instant) -> u128 {
    t0.elapsed().as_millis()
}

/// Lets the tasks run, with no time passing once it is there, until `log` holds `n` lines.
async fn until_lines(rig: &Rig, log: &test_log::Capture, n: usize) {
    rig.until("the lines are written", || lines(&log.text()).len() >= n)
        .await;
}

/// Checks the fields every line has: its ids, and a submit time that is the Execute's plus
/// `since_execute_ms`.
fn ids_and_times(line: &str, rig: &Rig, run: RunId) {
    assert_eq!(
        field(line, "operation"),
        run.operation.to_string(),
        "{line}"
    );
    assert_eq!(field(line, "run"), run.to_string(), "{line}");
    assert_eq!(field(line, "job"), job_id(&rig.cfg.frontend, run), "{line}");
    assert_eq!(number(line, "attempt"), u128::from(run.attempt), "{line}");
    assert_eq!(
        number(line, "submitted_unix_ms") - number(line, "execute_unix_ms"),
        number(line, "since_execute_ms"),
        "{line}"
    );
}

/// Catches a job whose start is not written per job, or written with the wrong times: the
/// first attempt, whose tree writes are held four polls, writes one line with `prepare_ms` 0 and
/// `start_ms` the four polls, and its Execute to submit time is the time the paused clock moved;
/// the next attempt, after the first job is lost, writes its own line with its own run and job,
/// the same Execute time, and the time since the Execute counting the first attempt.
#[tokio::test(start_paused = true)]
async fn each_job_the_service_takes_writes_one_line_with_its_start_times() {
    let log = test_log::capture();
    let rig = Rig::start().await;
    rig.ingest.hold_stored.store(true, Ordering::SeqCst);
    let t0 = Instant::now();
    let caller = rig.submit(1);
    let (run0, run1) = (
        RunId::new(caller.operation(), 0),
        RunId::new(caller.operation(), 1),
    );
    rig.until("the first run waits for its tree", || {
        rig.ingest.stored_asked.lock().unwrap().len() == 1
    })
    .await;
    assert_eq!(ms_since(t0), 0, "prepared at once");
    rig.advance(rig.cfg.poll * 4).await;
    assert_eq!(
        lines(&log.text()),
        Vec::<&str>::new(),
        "no line before the submit"
    );
    rig.ingest.release.notify_one();
    let first = rig.submitted(&caller, 0).await;
    until_lines(&rig, &log, 1).await;
    let waited = ms_since(t0);
    assert_eq!(waited, (rig.cfg.poll * 4).as_millis());
    let text = log.text();
    let written = lines(&text);
    assert_eq!(written.len(), 1, "{text}");
    ids_and_times(written[0], &rig, run0);
    assert_eq!(number(written[0], "prepare_ms"), 0);
    assert_eq!(number(written[0], "start_ms"), waited);
    assert_eq!(number(written[0], "since_execute_ms"), waited);

    rig.host_runs(&first);
    assert!(rig.fake.lose(&first));
    rig.ingest.release.notify_one();
    let second = rig.submitted(&caller, 1).await;
    until_lines(&rig, &log, 2).await;
    let text = log.text();
    let written = lines(&text);
    assert_eq!(written.len(), 2, "{text}");
    ids_and_times(written[1], &rig, run1);
    assert_eq!(
        field(written[1], "execute_unix_ms"),
        field(written[0], "execute_unix_ms"),
        "one Execute"
    );
    assert_eq!(number(written[1], "since_execute_ms"), ms_since(t0));
    rig.host_finishes(&second, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(lines(&log.text()).len(), 2, "no line past the submits");
}

/// Catches a line per submit call instead of per job: a submit whose answer was lost is sent
/// again, and the job it makes writes one line, whose `start_ms` holds the wait for the resend.
#[tokio::test(start_paused = true)]
async fn a_submit_sent_again_after_a_lost_answer_writes_one_line() {
    let log = test_log::capture();
    let rig = Rig::start().await;
    rig.fake.lose_next_answer();
    let t0 = Instant::now();
    let caller = rig.submit(1);
    rig.until("the submit is sent again", || {
        rig.calls_of("SubmitJob") >= 2
    })
    .await;
    until_lines(&rig, &log, 1).await;
    let waited = ms_since(t0);
    assert_eq!(rig.calls_of("SubmitJob"), 2);
    let text = log.text();
    let written = lines(&text);
    assert_eq!(written.len(), 1, "{text}");
    ids_and_times(written[0], &rig, RunId::new(caller.operation(), 0));
    assert_eq!(number(written[0], "start_ms"), waited);
    assert_eq!(number(written[0], "since_execute_ms"), waited);
}

/// Catches a line for a job the service did not take: a submit refused `PERMISSION_DENIED`
/// writes no line, and the next attempt's job, which the service takes, writes the only one.
#[tokio::test(start_paused = true)]
async fn a_refused_submit_writes_no_line() {
    let log = test_log::capture();
    let rig = Rig::start().await;
    rig.fake.inject_on(
        "SubmitJob",
        Fault::Status {
            code: Code::PermissionDenied,
            message: "no".to_owned(),
        },
    );
    let caller = rig.submit(1);
    rig.submitted(&caller, 1).await;
    until_lines(&rig, &log, 1).await;
    let text = log.text();
    let written = lines(&text);
    assert_eq!(written.len(), 1, "{text}");
    ids_and_times(written[0], &rig, RunId::new(caller.operation(), 1));
}

/// Catches a line for a run cancelled while its submit was unanswered: the job may have been
/// taken, but its taking was never answered, so no line is written.
#[tokio::test(start_paused = true)]
async fn a_run_cancelled_during_its_submit_writes_no_line() {
    let log = test_log::capture();
    let rig = Rig::start().await;
    rig.fake.inject_on("SubmitJob", Fault::Hang);
    let caller = rig.submit(1);
    rig.until("the submit is hung", || rig.calls_of("SubmitJob") == 1)
        .await;
    assert!(rig.exec.cancel(&caller.name));
    let finished = rig.until_done(&caller).await;
    assert!(
        matches!(finished, Finished::Failed(ref status) if status.code == Code::Cancelled as i32),
        "{finished:?}"
    );
    assert_eq!(lines(&log.text()), Vec::<&str>::new());
}

/// The front's wall-clock arrival the tests hand the executor: a fixed time, so its Unix
/// milliseconds in the line are exact.
const REAPI_UNIX_MS: u64 = 1_800_000_000_123;

/// An Execute of the action of `tag` that reached the front at `at`, at [`REAPI_UNIX_MS`] plus
/// `later_ms` on the wall clock.
fn arrived(tag: u8, at: Instant, later_ms: u64) -> Submission {
    Submission {
        arrival: Arrival {
            at,
            unix: UNIX_EPOCH + Duration::from_millis(REAPI_UNIX_MS + later_ms),
        },
        ..crate::executor::tests::submission_of(crate::executor::tests::digest_of(tag))
    }
}

/// Catches a line whose start figures begin at the executor, not at the front: an Execute that
/// spent 80 ms at the front before the executor took it writes `presubmit_ms` 80, its front
/// arrival as `reapi_unix_ms`, and `since_reapi_ms` 80 above `since_execute_ms`; the next
/// attempt's line keeps both, its `since_execute_ms` counting the first attempt too.
#[tokio::test(start_paused = true)]
async fn the_line_counts_the_time_before_the_executor_from_the_fronts_arrival() {
    let log = test_log::capture();
    let rig = Rig::start().await;
    rig.ingest.hold_stored.store(true, Ordering::SeqCst);
    let front = Instant::now();
    rig.advance(Duration::from_millis(80)).await;
    let t0 = Instant::now();
    let caller = Caller::of(rig.exec.submit(arrived(1, front, 0)).expect("taken"));
    rig.until("the first run waits for its tree", || {
        rig.ingest.stored_asked.lock().unwrap().len() == 1
    })
    .await;
    rig.advance(rig.cfg.poll * 2).await;
    rig.ingest.release.notify_one();
    let first = rig.submitted(&caller, 0).await;
    until_lines(&rig, &log, 1).await;
    let since = ms_since(t0);
    assert_eq!(since, (rig.cfg.poll * 2).as_millis());
    let text = log.text();
    let written = lines(&text);
    assert_eq!(written.len(), 1, "{text}");
    ids_and_times(written[0], &rig, RunId::new(caller.operation(), 0));
    assert_eq!(number(written[0], "since_execute_ms"), since);
    assert_eq!(number(written[0], "presubmit_ms"), 80);
    assert_eq!(number(written[0], "since_reapi_ms"), since + 80);
    assert_eq!(
        number(written[0], "reapi_unix_ms"),
        u128::from(REAPI_UNIX_MS)
    );

    rig.host_runs(&first);
    assert!(rig.fake.lose(&first));
    rig.ingest.release.notify_one();
    let second = rig.submitted(&caller, 1).await;
    until_lines(&rig, &log, 2).await;
    let text = log.text();
    let written = lines(&text);
    ids_and_times(written[1], &rig, RunId::new(caller.operation(), 1));
    assert_eq!(number(written[1], "since_execute_ms"), ms_since(t0));
    assert_eq!(number(written[1], "presubmit_ms"), 80, "one Execute");
    assert_eq!(number(written[1], "since_reapi_ms"), ms_since(t0) + 80);
    assert_eq!(
        number(written[1], "reapi_unix_ms"),
        u128::from(REAPI_UNIX_MS)
    );
    rig.host_finishes(&second, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
}

/// Catches a line that takes the arrival of an Execute joined to a running operation: the
/// second Execute of the action, which reached the front later, joins before the job is
/// submitted, and the one line keeps the first Execute's arrival; and an Execute whose front
/// stamp is the executor's own moment writes `presubmit_ms` 0, the boundary.
#[tokio::test(start_paused = true)]
async fn a_joined_execute_leaves_the_line_with_the_first_arrival() {
    let log = test_log::capture();
    let rig = Rig::start().await;
    rig.ingest.hold_stored.store(true, Ordering::SeqCst);
    let caller = Caller::of(
        rig.exec
            .submit(arrived(1, Instant::now(), 0))
            .expect("taken"),
    );
    rig.until("the run waits for its tree", || {
        rig.ingest.stored_asked.lock().unwrap().len() == 1
    })
    .await;
    rig.advance(rig.cfg.poll).await;
    let joined = rig
        .exec
        .submit(arrived(1, Instant::now(), 5_000))
        .expect("joined");
    assert_eq!(joined.name, caller.name, "the same operation");
    rig.ingest.release.notify_one();
    rig.submitted(&caller, 0).await;
    until_lines(&rig, &log, 1).await;
    let text = log.text();
    let written = lines(&text);
    assert_eq!(written.len(), 1, "{text}");
    assert_eq!(number(written[0], "presubmit_ms"), 0);
    assert_eq!(
        number(written[0], "reapi_unix_ms"),
        u128::from(REAPI_UNIX_MS)
    );
    assert_eq!(
        number(written[0], "since_reapi_ms"),
        number(written[0], "since_execute_ms")
    );
}

/// Catches a line that invents a front arrival for an operation no front handed this process:
/// one taken up from the state store, whose row names no put directory, runs again here, and
/// its job's line writes `reapi_unix_ms` 0 and `presubmit_ms` 0, so `since_reapi_ms` is
/// `since_execute_ms`.
#[tokio::test(start_paused = true)]
async fn a_taken_up_operation_writes_no_front_arrival() {
    let log = test_log::capture();
    let rig = Rig::stored().await;
    let action = crate::executor::tests::digest_of(1);
    let old = job_id(&rig.cfg.frontend, RunId::new(OperationId(60), 0));
    rig.fake
        .record(js_proto::v1::JobSpec {
            job_id: old.clone(),
            image: crate::executor::tests::IMAGE.to_owned(),
            wall_limit_seconds: Some(360),
            network: js_proto::v1::NetworkLevel::None.into(),
            constraints: Some(rig.cfg.constraints()),
            ..js_proto::v1::JobSpec::default()
        })
        .expect("a job");
    let row = OpRow {
        owner: String::new(),
        instance: "main".to_owned(),
        hash: action.hash_hex(),
        size: action.size_bytes,
        attempt: 0,
        job: old,
        key: None,
        whole_machine: false,
        tree: None,
    };
    rig.rows("gone").put_op(OperationId(60), &row).expect("row");
    let running = crate::executor::Running::spawn(&rig.exec);
    running.kept(1).await;
    let run = RunId::new(OperationId(60), 1);
    let next = job_id(&rig.cfg.frontend, run);
    rig.until("the next attempt is submitted", || {
        rig.fake.submitted(&next).is_some()
    })
    .await;
    until_lines(&rig, &log, 1).await;
    let text = log.text();
    let written: Vec<_> = lines(&text)
        .into_iter()
        .filter(|line| line.contains(&format!(" job={next} ")))
        .collect();
    assert_eq!(written.len(), 1, "{text}");
    ids_and_times(written[0], &rig, run);
    assert_eq!(number(written[0], "reapi_unix_ms"), 0);
    assert_eq!(number(written[0], "presubmit_ms"), 0);
    assert_eq!(
        number(written[0], "since_reapi_ms"),
        number(written[0], "since_execute_ms")
    );
    running.stop().await;
}
