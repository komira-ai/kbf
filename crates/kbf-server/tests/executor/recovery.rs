//! What the executor does when the world breaks: the job service stops answering, a submit is
//! refused, an answer is lost, the frontend's store is slow.

use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_front::Finished;
use kbf_job_client::fake::Fault;
use tonic::Code;

use crate::rig::{HOST, Rig, submits};

/// The job service says `UNAVAILABLE` to the next `n` calls of `method`.
fn unavailable(rig: &Rig, method: &str, n: usize) {
    for _ in 0..n {
        rig.fake.inject_on(
            method,
            Fault::Status {
                code: Code::Unavailable,
                message: "down".to_owned(),
            },
        );
    }
}

/// Catches a submit whose answer was lost being given up on, or making a second job: the job
/// service made the job, and the executor cannot know, so it sends the submit again with the
/// same job id; the service answers from the job it has, there is one job of the action, and
/// it is counted as submitted once and sent again once.
#[tokio::test]
async fn a_post_whose_answer_was_lost_is_submitted_again_with_the_same_id_and_makes_one_job() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    rig.fake.inject_on("SubmitJob", Fault::LoseAnswer);
    let mut caller = rig.submit(1);
    rig.counter_reaches("kbf_executor_jobs_submitted_total", 1)
        .await;
    let sent = submits(&rig.fake.calls());
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[0].job_id, sent[1].job_id);
    let jobs = rig.jobs();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].id(), sent[0].job_id);
    assert_eq!(rig.counter("kbf_executor_jobs_submitted_total"), 1);
    assert!(caller.finished().is_none());
    rig.host_finishes(jobs[0].id(), HOST, 0);
    assert!(matches!(caller.done().await, Finished::Ran(_)));
    assert_eq!(rig.ingest.keys_read().len(), 1);
}

/// Catches a submit the job service refuses for its own sake (it did not take the job, and
/// says nothing about the job being wrong) being the action's failure or being sent again
/// unchanged: it is infrastructure, so the operation runs again as its next attempt, a new
/// job id, and the refused submit made no job.
#[tokio::test]
async fn a_refused_submit_is_run_again_as_the_next_attempt() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    rig.fake.inject_on(
        "SubmitJob",
        Fault::Status {
            code: Code::ResourceExhausted,
            message: "busy".to_owned(),
        },
    );
    let mut caller = rig.submit(1);
    let jobs = rig.until_jobs(1, 1).await;
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert!(jobs[0].id().ends_with("-1"), "{}", jobs[0].id());
    assert!(caller.finished().is_none());
    assert_eq!(rig.counter("kbf_executor_runs_total{outcome=\"infra\"}"), 1);
    rig.host_finishes(jobs[0].id(), HOST, 0);
    assert!(matches!(caller.done().await, Finished::Ran(_)));
}

/// Catches a job service that stops answering for a while ending the operation, or making a
/// second job for it: its reads of the job fail twenty times, the operation is neither answered
/// nor submitted again, and once the service answers the job's end is the caller's answer.
#[tokio::test]
async fn a_job_service_that_is_down_for_a_while_loses_nothing() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut caller = rig.submit(1);
    let job = rig.first_job(1).await;
    rig.host_runs(job.id(), HOST);
    unavailable(&rig, "GetJob", 20);
    assert!(rig.fake.finish(job.id(), 0));
    assert!(matches!(caller.done().await, Finished::Ran(_)));
    assert_eq!(rig.jobs_of(1).len(), 1, "a second job was made");
    assert_eq!(rig.ingest.keys_read().len(), 1);
}

/// Catches a job service that accepts a request and never answers holding the operation for
/// the client's own timeout a call, or giving it up: the read gives up after the call timeout,
/// the next read is made, and the job goes on being tracked to its end.
#[tokio::test]
async fn a_job_service_that_never_answers_a_read_does_not_hold_the_operation() {
    let rig = Rig::start(&[HOST], |cfg| cfg.call_timeout = Duration::from_millis(100)).await;
    let mut caller = rig.submit(1);
    let job = rig.first_job(1).await;
    rig.host_runs(job.id(), HOST);
    rig.fake.inject_on("GetJob", Fault::Hang);
    rig.fake.hung().await;
    assert!(rig.fake.finish(job.id(), 0));
    assert!(matches!(caller.done().await, Finished::Ran(_)));
    assert_eq!(rig.jobs_of(1).len(), 1);
}

/// Catches an ingest that takes long (a slow store) holding up the executor: the pack of one
/// operation is being read, and another operation is submitted, runs and is answered meanwhile;
/// the first is answered when its read ends, from the one job, with no second made.
#[tokio::test]
async fn a_slow_ingest_holds_only_its_own_operation() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    rig.ingest.hold.store(true, Ordering::SeqCst);
    let mut slow = rig.submit(1);
    let job = rig.first_job(1).await;
    rig.host_finishes(job.id(), HOST, 0);
    while rig.ingest.keys_read().is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    rig.ingest.hold.store(false, Ordering::SeqCst);
    let mut other = rig.submit(2);
    let second = rig.first_job(2).await;
    rig.host_finishes(second.id(), HOST, 0);
    assert!(matches!(other.done().await, Finished::Ran(_)));
    assert!(slow.finished().is_none(), "the held read answered");
    rig.ingest.release.notify_one();
    assert!(matches!(slow.done().await, Finished::Ran(_)));
    assert_eq!(rig.jobs_of(1).len(), 1, "a second job was made");
}
