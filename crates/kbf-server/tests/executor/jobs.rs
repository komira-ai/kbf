//! The executor's whole traffic with the job service, over a cycle that uses all of it.

use std::time::Duration;

use js_proto::v1::JobState;
use kbf_front::Finished;
use kbf_job_client::fake::Fault;
use kbf_types::LeaseKind;

use crate::rig::{Posted, Rig, cancels, submits};

const HOST: &str = "host-1";

/// The most rounds of 5 s a wait takes before the test fails, rather than waiting for ever.
const ROUNDS: usize = 60;

/// The queued job of the action of `tag`, once the executor has submitted one that is not in
/// `seen`.
async fn queued(rig: &mut Rig, tag: u8, seen: &[String]) -> Posted {
    for _ in 0..ROUNDS {
        let found = rig
            .jobs_of(tag)
            .into_iter()
            .find(|p| p.state() == JobState::Queued && !seen.iter().any(|id| id == p.id()));
        if let Some(job) = found {
            return job;
        }
        rig.advance(Duration::from_secs(5)).await;
        rig.pump().await;
    }
    panic!(
        "no new job of action {tag} after {ROUNDS} rounds: {:?}",
        rig.methods()
    );
}

/// Catches an executor that does not use the job service the way its design says (the lease
/// client itself is kept out of the executor by the layering test): over actions that run, a
/// whole-machine action, a submit whose answer is lost, a lease the farm gives up and a restart
/// of the executor, every call it makes is one of `SubmitJob`, `GetJob`, `ListJobs`,
/// `CancelJob` and `GetResult`, and each of the five is made (so the check is not about an
/// empty list). The lost answer makes one job, sent twice; the whole-machine action is one job
/// too, through the same calls.
#[tokio::test(start_paused = true)]
async fn no_lease_call_remains() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;

    // Two actions run and exit 0, and are ingested.
    let (first, second) = (rig.submit(1), rig.submit(2));
    for (tag, caller) in [(1, &first), (2, &second)] {
        let job = queued(&mut rig, tag, &[]).await;
        rig.host_finishes(job.id(), HOST, 0);
        assert!(matches!(rig.until_done(caller, 8).await, Finished::Ran(_)));
    }
    assert_eq!(rig.ingest.calls.lock().expect("calls").len(), 2);

    // A whole-machine action is one job, of the whole host, submitted and read as any other.
    let whole = rig.submit_kind(3, LeaseKind::WholeMachine);
    let job = queued(&mut rig, 3, &[]).await;
    assert!(
        job.spec
            .needs
            .as_ref()
            .is_some_and(|needs| needs.whole_host)
    );
    rig.host_finishes(job.id(), HOST, 0);
    assert!(matches!(rig.until_done(&whole, 8).await, Finished::Ran(_)));
    assert_eq!(rig.jobs_of(3).len(), 1);

    // A submit whose answer is lost makes one job, which is sent again under its id.
    let lost = rig.submit(4);
    rig.fake.inject_on("SubmitJob", Fault::LoseAnswer);
    let job = queued(&mut rig, 4, &[]).await;
    assert_eq!(rig.jobs_of(4).len(), 1, "one job, whatever was sent");
    let sent: Vec<_> = submits(&rig.fake.calls())
        .into_iter()
        .filter(|spec| spec.job_id == job.id())
        .collect();
    assert_eq!(sent.len(), 2, "sent once, then again after the lost answer");
    assert_eq!(
        rig.counter("kbf_executor_jobs_resubmitted_after_lost_answer_total"),
        1
    );
    rig.host_finishes(job.id(), HOST, 0);
    assert!(matches!(rig.until_done(&lost, 8).await, Finished::Ran(_)));

    // The farm gives up a lease whose worker was not heard for a minute: the job is cancelled,
    // the host stops it, and the action is submitted again as a new job.
    let given_up = rig.submit(5);
    let running = queued(&mut rig, 5, &[]).await;
    rig.host_runs(running.id(), HOST);
    rig.advance(Duration::from_secs(65)).await;
    rig.farm.tick().await;
    rig.pump().await;
    assert!(
        cancels(&rig.fake.calls()).contains(&running.id().to_owned()),
        "the job of the lease the farm gave up was not cancelled: {:?}",
        rig.methods()
    );
    assert!(rig.fake.host_stopped(running.id()));
    let again = queued(&mut rig, 5, &[running.id().to_owned()]).await;
    rig.host_finishes(again.id(), HOST, 0);
    assert!(matches!(
        rig.until_done(&given_up, 8).await,
        Finished::Ran(_)
    ));

    // The executor restarts while a job runs: its shutdown cancels the job, and the executor
    // that takes its place runs the action as a new job.
    let restarted = rig.submit(6);
    let interrupted = queued(&mut rig, 6, &[]).await;
    rig.host_runs(interrupted.id(), HOST);
    rig.exec.shutdown().await;
    assert!(
        cancels(&rig.fake.calls()).contains(&interrupted.id().to_owned()),
        "the shutdown left the job running: {:?}",
        rig.methods()
    );
    assert!(rig.fake.host_stopped(interrupted.id()));
    rig.exec = rig.restarted_executor();
    let rerun = queued(&mut rig, 6, &[interrupted.id().to_owned()]).await;
    rig.host_finishes(rerun.id(), HOST, 0);
    assert!(matches!(
        rig.until_done(&restarted, 8).await,
        Finished::Ran(_)
    ));

    // Every call is the job service's, and each of its calls was made.
    let methods = rig.methods();
    let known = ["SubmitJob", "GetJob", "ListJobs", "CancelJob", "GetResult"];
    for method in &methods {
        assert!(known.contains(method), "{method} in {methods:?}");
    }
    for method in known {
        assert!(methods.contains(&method), "no {method} in {methods:?}");
    }
}
