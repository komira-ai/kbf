//! The executor's whole traffic with the job service, over a cycle that uses all of it.

use kbf_front::{Dispatch as _, Finished};
use kbf_job_client::fake::Fault;
use tonic::Code;

use crate::rig::{HOST, Rig, cancels, submits};

/// Catches an executor that does not use the job service the way its design says: over actions
/// that run, a whole-machine action refused, a submit whose answer is lost, a caller that cancels and a
/// job that did not end well, every call it makes is one of `SubmitJob`, `GetJob`, `CancelJob`,
/// `GetResult` and `GetLogs`, and each of them is made (so the check is not about an empty
/// list). The lost answer makes one job, sent twice; the whole-machine action makes none; and the
/// cancel is made for the one job of the cancelled operation.
#[tokio::test]
async fn the_traffic_with_the_job_service_is_the_five_calls_of_its_design() {
    let rig = Rig::start(&[HOST], |_| {}).await;

    // Two actions run and exit 0, and are ingested.
    let (mut first, mut second) = (rig.submit(1), rig.submit(2));
    for (tag, caller) in [(1, &mut first), (2, &mut second)] {
        let job = rig.first_job(tag).await;
        rig.host_finishes(job.id(), HOST, 0);
        assert!(matches!(caller.done().await, Finished::Ran(_)));
    }
    assert_eq!(rig.ingest.keys_read().len(), 2);

    // A whole-machine action cannot commit into a put directory: it is refused as the caller's,
    // and no job is submitted for it.
    let mut whole = rig.submit_whole(3);
    match whole.done().await {
        Finished::Failed(status) => {
            assert_eq!(Code::from_i32(status.code), Code::InvalidArgument);
            assert!(
                status.message.contains("whole_machine"),
                "{}",
                status.message
            );
        }
        other => panic!("not refused: {other:?}"),
    }
    assert!(rig.jobs_of(3).is_empty());

    // A submit whose answer is lost makes one job, which is sent again under its id.
    rig.fake.inject_on("SubmitJob", Fault::LoseAnswer);
    let mut lost = rig.submit(4);
    let job = rig.first_job(4).await;
    rig.counter_reaches("kbf_executor_jobs_submitted_total", 3)
        .await;
    assert_eq!(rig.jobs_of(4).len(), 1, "one job, whatever was sent");
    let sent: Vec<_> = submits(&rig.fake.calls())
        .into_iter()
        .filter(|spec| spec.job_id == job.id())
        .collect();
    assert_eq!(sent.len(), 2, "sent once, then again after the lost answer");
    rig.host_finishes(job.id(), HOST, 0);
    assert!(matches!(lost.done().await, Finished::Ran(_)));

    // A caller cancels an operation whose job runs: its job is cancelled, and it is answered
    // CANCELLED.
    let mut cancelled = rig.submit(5);
    let running = rig.first_job(5).await;
    rig.host_runs(running.id(), HOST);
    assert!(rig.exec.cancel(&cancelled.name));
    match cancelled.done().await {
        Finished::Failed(status) => assert_eq!(Code::from_i32(status.code), Code::Cancelled),
        other => panic!("not cancelled: {other:?}"),
    }
    assert_eq!(cancels(&rig.fake.calls()), [running.id().to_owned()]);
    assert_eq!(
        rig.jobs_of(5).len(),
        1,
        "a cancelled operation is not run again"
    );

    // A job that did not end well has its output read.
    let mut failing = rig.submit(6);
    let job = rig.first_job(6).await;
    rig.fake.write_logs(job.id(), b"", b"it broke\n");
    rig.host_finishes(job.id(), HOST, 12);
    let retry = rig.until_jobs(6, 2).await.remove(1);
    rig.host_finishes(retry.id(), HOST, 0);
    assert!(matches!(failing.done().await, Finished::Ran(_)));

    let methods = rig.methods();
    let known = ["SubmitJob", "GetJob", "CancelJob", "GetResult", "GetLogs"];
    for method in &methods {
        assert!(known.contains(method), "{method} in {methods:?}");
    }
    for method in known {
        assert!(methods.contains(&method), "no {method} in {methods:?}");
    }
}
