//! The line each job the service takes writes: one per job, with its ids and the times of its
//! start as the frontend saw them; none for a submit the service refused or a run cancelled
//! before its job was taken.

use std::sync::atomic::Ordering;

use kbf_front::{Dispatch, Finished};
use kbf_job_client::fake::Fault;
use tonic::Code;

use super::*;
use crate::executor::job_id;
use crate::executor::tests::Rig;
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
