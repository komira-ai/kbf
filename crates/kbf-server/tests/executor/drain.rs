//! What the executor does with the jobs it has on the job service once a drain has closed the
//! farm's intake.

use std::time::Duration;

use js_proto::v1::JobState;
use kbf_front::Stage;
use kbf_job_client::fake::Fault;

use crate::rig::{Posted, Rig, cancels};

const HOST: &str = "host-1";

/// The most rounds of 5 s a wait takes before the test fails, rather than waiting for ever.
const ROUNDS: usize = 60;

/// The job of the action of `tag`, once the executor has submitted it.
async fn submitted(rig: &mut Rig, tag: u8) -> Posted {
    for _ in 0..ROUNDS {
        if let Some(job) = rig.jobs_of(tag).into_iter().next() {
            return job;
        }
        rig.advance(Duration::from_secs(5)).await;
        rig.pump().await;
    }
    panic!(
        "no job of action {tag} after {ROUNDS} rounds: {:?}",
        rig.methods()
    );
}

/// Lets the executor and the farm step for `rounds` rounds of 5 s.
async fn rounds(rig: &mut Rig, rounds: usize) {
    for _ in 0..rounds {
        rig.advance(Duration::from_secs(5)).await;
        rig.pump().await;
    }
}

/// Asserts that a job still queued on the job service when the drain closes the intake is
/// cancelled, its lease given up and its caller's stage one a drain ends (`Waiting`, not the
/// `Executing` it read, and not `WaitingKept`), so that a job nothing will place does not hold
/// the drain: with every host of the job service out of service, the queue never moves. A job
/// a host is running is left alone, and its caller keeps reading `Executing`.
#[tokio::test(start_paused = true)]
async fn a_drain_withdraws_the_jobs_still_queued_and_leaves_the_running_ones() {
    let mut rig = Rig::start(&[HOST], 2, |_| {}).await;
    rig.pump().await;
    let (queued, running) = (rig.submit(1), rig.submit(2));
    let queued_job = submitted(&mut rig, 1).await;
    let running_job = submitted(&mut rig, 2).await;
    rig.host_runs(running_job.id(), HOST);
    rounds(&mut rig, 2).await;
    assert_eq!(queued_job.state(), JobState::Queued);
    assert!(matches!(*queued.stage.borrow(), Stage::Executing));
    assert!(
        cancels(&rig.fake.calls()).is_empty(),
        "nothing cancelled before the drain"
    );

    rig.farm.close_intake();
    rounds(&mut rig, 2).await;

    assert_eq!(
        cancels(&rig.fake.calls()),
        [queued_job.id().to_owned()],
        "only the queued job is cancelled"
    );
    assert!(
        matches!(*queued.stage.borrow(), Stage::Waiting(_)),
        "{:?}",
        *queued.stage.borrow()
    );
    assert!(matches!(*running.stage.borrow(), Stage::Executing));
}

/// Asserts that a cancel the job service refuses to take leaves the job's lease where it was,
/// and the next pass cancels it: the lease is not given up while its job may still be placed.
#[tokio::test(start_paused = true)]
async fn a_withdrawal_the_job_service_did_not_answer_is_tried_again() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(1);
    let job = submitted(&mut rig, 1).await;
    rounds(&mut rig, 1).await;

    rig.fake.inject_on(
        "CancelJob",
        Fault::Status {
            code: tonic::Code::Unavailable,
            message: "the job service is restarting".to_owned(),
        },
    );
    rig.farm.close_intake();
    rounds(&mut rig, 3).await;

    assert!(
        cancels(&rig.fake.calls()).iter().any(|id| id == job.id()),
        "{:?}",
        rig.methods()
    );
    assert!(matches!(*caller.stage.borrow(), Stage::Waiting(_)));
}

/// Asserts that a job the job service no longer knows when the drain withdraws it is withdrawn
/// all the same: the answer is a NotFound, which is an answer, and the lease is given up.
#[tokio::test(start_paused = true)]
async fn a_queued_job_the_job_service_forgot_is_withdrawn_at_a_drain() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(1);
    submitted(&mut rig, 1).await;
    rounds(&mut rig, 1).await;

    rig.fake.inject_on(
        "CancelJob",
        Fault::Status {
            code: tonic::Code::NotFound,
            message: "no such job".to_owned(),
        },
    );
    rig.farm.close_intake();
    rounds(&mut rig, 3).await;

    assert!(matches!(*caller.stage.borrow(), Stage::Waiting(_)));
}
