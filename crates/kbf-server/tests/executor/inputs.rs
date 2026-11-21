//! A job whose inputs its host could not provide or read is the farm's failure: the action runs
//! again as the operation's next attempt, and only when the attempts run out is the caller
//! answered, `UNAVAILABLE` with a message that names the input and the host, never the action's
//! exit code. A compile that really failed is still answered with its own exit code at once.
//!
//! The host's input failure comes two ways: the job service's result says so
//! (`FakeJobServer::fail_inputs`, as the agent reports it), or the job succeeded with the
//! action's nonzero exit and the frontend's ingest says the action could not read a declared
//! input (`FakeIngest::unreadable`, scripted here; the frontend's reading of the action's
//! standard error has its own unit tests).

use kbf_front::Finished;
use tonic::Code;

use crate::rig::{FakeIngest, HOST, Rig, locked};

/// A second host, where the next attempt runs.
const OTHER: &str = "host-2";

/// The path the action's compiler could not open.
const HEADER: &str = "buck-out/v2/art/toolchains/zig/lib/libcxx/include/__assert";

fn exit_code(finished: &Finished) -> i32 {
    match finished {
        Finished::Ran(result) => result.exit_code,
        other => panic!("did not run: {other:?}"),
    }
}

/// The code and message of a failed answer.
fn failed(finished: &Finished) -> (Code, String) {
    match finished {
        Finished::Failed(status) => (Code::from_i32(status.code), status.message.clone()),
        other => panic!("not a failure: {other:?}"),
    }
}

/// Catches a job whose host reported its input failed being answered as the action's failure,
/// or its pack read: the action runs again as another job, on whichever host takes it, and the
/// caller is answered with the second job's result alone.
#[tokio::test]
async fn a_reported_input_failure_is_run_again_and_the_second_result_is_the_answer() {
    let rig = Rig::start(&[HOST, OTHER], |_| {}).await;
    let mut caller = rig.submit(1);
    let first = rig.first_job(1).await;
    rig.host_runs(first.id(), HOST);
    assert!(rig.fake.fail_inputs(first.id(), 1, "input-unreadable"));
    let second = rig.until_jobs(1, 2).await.remove(1);
    assert_ne!(second.id(), first.id());
    assert!(caller.finished().is_none(), "answered at the input failure");
    assert!(
        rig.ingest.keys_read().is_empty(),
        "the failed job's pack was read"
    );
    rig.host_finishes(second.id(), OTHER, 0);
    assert_eq!(exit_code(&caller.done().await), 0);
    assert_eq!(rig.ingest.keys_read().len(), 1);
    for row in [
        "kbf_executor_runs_total{outcome=\"input_failed\"} 1\n",
        "kbf_executor_runs_total{outcome=\"ran\"} 1\n",
        "kbf_executor_attempts_resubmitted_total 1\n",
    ] {
        let counted = rig.counted();
        assert!(counted.contains(row), "{row} in {counted}");
    }
}

/// Catches an input failure at every attempt being answered as the action's exit, or with a
/// message that does not say whose failure it is: after three jobs the caller is answered
/// `UNAVAILABLE`, naming the last job's host and the agent's code, and no pack is read. The
/// job's own exit code (0 here: the agent reports the input failure whatever the exit) is not
/// the answer.
#[tokio::test]
async fn a_reported_input_failure_at_every_attempt_is_the_farms_naming_host_and_code() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut caller = rig.submit(1);
    for attempt in 0..3 {
        let job = rig.until_jobs(1, attempt + 1).await.remove(attempt);
        rig.host_runs(job.id(), HOST);
        assert!(rig.fake.fail_inputs(job.id(), 0, "input-unreadable"));
    }
    let (code, message) = failed(&caller.done().await);
    assert_eq!(code, Code::Unavailable);
    assert_eq!(
        message,
        "the farm could not provide the job's inputs on host host-1: the job service reported \
         input-unreadable (not the action's own failure)"
    );
    assert_eq!(rig.jobs_of(1).len(), 3, "a fourth job was made");
    assert!(rig.ingest.keys_read().is_empty());
    let counted = rig.counted();
    let row = "kbf_executor_runs_total{outcome=\"input_failed\"} 3\n";
    assert!(counted.contains(row), "{row} in {counted}");
}

/// Catches an action that exited 1 because its host could not read a declared input being
/// answered with that exit code: the frontend's ingest names the input, the action runs again,
/// and the caller gets the second run's result.
#[tokio::test]
async fn an_action_that_could_not_read_a_declared_input_is_run_again() {
    let rig = Rig::start(&[HOST, OTHER], |_| {}).await;
    locked(&rig.ingest.script).extend([FakeIngest::ran(1), FakeIngest::ran(0)]);
    locked(&rig.ingest.unreadable).push_back(HEADER.to_owned());
    let mut caller = rig.submit(1);
    let first = rig.first_job(1).await;
    rig.host_finishes(first.id(), HOST, 0);
    let second = rig.until_jobs(1, 2).await.remove(1);
    assert!(caller.finished().is_none(), "answered with the exit code 1");
    rig.host_finishes(second.id(), OTHER, 0);
    assert_eq!(exit_code(&caller.done().await), 0);
    assert_eq!(*locked(&rig.ingest.asked_unreadable), vec![1]);
    let counted = rig.counted();
    for row in [
        "kbf_executor_runs_total{outcome=\"input_failed\"} 1\n",
        "kbf_executor_runs_total{outcome=\"ran\"} 1\n",
    ] {
        assert!(counted.contains(row), "{row} in {counted}");
    }
}

/// Catches an unreadable input at every attempt being answered as the action's exit 1: after
/// three jobs the caller is answered `UNAVAILABLE`, naming the input and the host.
#[tokio::test]
async fn an_unreadable_input_at_every_attempt_is_the_farms_naming_input_and_host() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    locked(&rig.ingest.script).extend((0..3).map(|_| FakeIngest::ran(1)));
    locked(&rig.ingest.unreadable).extend((0..3).map(|_| HEADER.to_owned()));
    let mut caller = rig.submit(1);
    for attempt in 0..3 {
        let job = rig.until_jobs(1, attempt + 1).await.remove(attempt);
        rig.host_finishes(job.id(), HOST, 0);
    }
    let (code, message) = failed(&caller.done().await);
    assert_eq!(code, Code::Unavailable);
    assert_eq!(
        message,
        format!(
            "the farm could not read the action's input \"{HEADER}\" on host host-1: opening \
             it answered an I/O error (not the action's own failure)"
        )
    );
    assert_eq!(rig.jobs_of(1).len(), 3, "a fourth job was made");
    assert_eq!(*locked(&rig.ingest.asked_unreadable), vec![1, 1, 1]);
}

/// Catches a compile that really failed being run again: the ingest names no unreadable input,
/// so the caller is answered with the action's exit code 1 after one job.
#[tokio::test]
async fn a_compile_that_failed_is_answered_with_its_exit_code_at_once() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    locked(&rig.ingest.script).push_back(FakeIngest::ran(1));
    let mut caller = rig.submit(1);
    let job = rig.first_job(1).await;
    rig.host_finishes(job.id(), HOST, 0);
    assert_eq!(exit_code(&caller.done().await), 1);
    assert_eq!(rig.jobs_of(1).len(), 1);
    assert_eq!(*locked(&rig.ingest.asked_unreadable), vec![1]);
}

/// Catches the frontend being asked about a clean action: an action that exited 0 is answered
/// without the question, even when an answer would name an input.
#[tokio::test]
async fn an_action_that_exited_zero_is_never_asked_about() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    locked(&rig.ingest.unreadable).push_back(HEADER.to_owned());
    let mut caller = rig.submit(1);
    let job = rig.first_job(1).await;
    rig.host_finishes(job.id(), HOST, 0);
    assert_eq!(exit_code(&caller.done().await), 0);
    assert_eq!(rig.jobs_of(1).len(), 1);
    assert!(locked(&rig.ingest.asked_unreadable).is_empty());
}
