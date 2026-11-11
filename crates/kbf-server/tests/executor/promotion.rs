//! The promotion rule: the frontend reads a job's pack, and promotes its outputs, only from a
//! clean result (the job and its result succeeded, for the job's exit, with exit code 0). A
//! job that wrote its whole pack and then ended any other way has its pack left unread.
//!
//! The fake job service ends a job by exit code only, so every other ending is scripted: the
//! answers to the executor's next read of the job and of its result are replaced by ones that
//! say how it ended.

use bytes::Bytes;
use js_proto::v1::{GetJobResponse, GetResultResponse, Job, JobResult, JobState, ResultReason};
use kbf_front::Finished;
use kbf_job_client::fake::Fault;
use prost::Message as _;
use tonic::Code;

use crate::rig::{Posted, Rig};

const HOST: &str = "host-1";

/// How the first job of the action ends, after it wrote its whole pack.
#[derive(Debug)]
enum Ending {
    /// The task tool exits nonzero, as the fake ends a job.
    Exits(i32),
    /// Someone else cancels the job.
    Cancelled,
    /// The job's state is this and its result is this, whatever the service holds.
    Scripted(JobState, JobState, ResultReason, Option<i32>),
}

/// What the caller is answered after the first job's ending.
#[derive(Debug, PartialEq)]
enum Then {
    /// The action runs again, and the rerun's clean end is the caller's answer.
    Rerun,
    /// The caller is answered `DEADLINE_EXCEEDED`, and the action is not run again.
    TimedOut,
}

/// The key of the pack `job` may write, from the url it was handed.
fn pack_key(job: &Posted) -> String {
    let url = job
        .file("pack-url")
        .or_else(|| job.arg("--pack-url"))
        .expect("a pack url");
    let key = url
        .strip_prefix("https://store.test/")
        .and_then(|rest| rest.split('?').next())
        .expect("a url of the fake store");
    key.to_owned()
}

/// The keys the frontend was asked to read, in order.
fn read_keys(rig: &Rig) -> Vec<String> {
    let calls = rig.ingest.calls.lock().expect("calls");
    calls.iter().map(|call| call.1.clone()).collect()
}

/// The next read of job `id` answers it in `job_state`, and the next read of its result
/// answers `result_state`, `reason` and `exit_code`.
fn script(
    rig: &Rig,
    id: &str,
    (job_state, result_state, reason, exit_code): (JobState, JobState, ResultReason, Option<i32>),
) {
    let job = GetJobResponse {
        job: Some(Job {
            id: id.to_owned(),
            state: job_state.into(),
            ..Job::default()
        }),
    };
    let result = GetResultResponse {
        result: Some(JobResult {
            state: result_state.into(),
            exit_code,
            reason: reason.into(),
            ..JobResult::default()
        }),
    };
    rig.fake
        .inject_on("GetJob", Fault::Answer(Bytes::from(job.encode_to_vec())));
    rig.fake.inject_on(
        "GetResult",
        Fault::Answer(Bytes::from(result.encode_to_vec())),
    );
}

/// Catches a pack the job already wrote in full being read, and its outputs promoted, from a
/// result that is not clean: a nonzero exit of the task tool, a cancel by someone else, the
/// wall limit, the memory limit, and a job that says it succeeded with a result that has no
/// exit code, a nonzero one, a reason other than its exit, or a state other than succeeded.
/// For each, the first job's key is written and never read; the action runs again (or, for
/// the wall limit, is answered `DEADLINE_EXCEEDED`), and only the rerun's clean end is read.
#[tokio::test(start_paused = true)]
async fn outputs_are_promoted_only_from_a_clean_result() {
    use JobState::{Failed, Succeeded};
    use ResultReason::{Exit, Memory, Unspecified, WallLimit};
    let endings = [
        (Ending::Exits(12), Then::Rerun),
        (Ending::Cancelled, Then::Rerun),
        (
            Ending::Scripted(Failed, Failed, WallLimit, Some(137)),
            Then::TimedOut,
        ),
        (
            Ending::Scripted(Failed, Failed, Memory, Some(137)),
            Then::Rerun,
        ),
        (
            Ending::Scripted(Succeeded, Succeeded, Exit, None),
            Then::Rerun,
        ),
        (
            Ending::Scripted(Succeeded, Succeeded, Exit, Some(3)),
            Then::Rerun,
        ),
        (
            Ending::Scripted(Succeeded, Succeeded, Unspecified, Some(0)),
            Then::Rerun,
        ),
        (
            Ending::Scripted(Succeeded, Succeeded, WallLimit, Some(0)),
            Then::Rerun,
        ),
        (
            Ending::Scripted(Succeeded, Failed, Exit, Some(0)),
            Then::Rerun,
        ),
    ];
    for (ending, then) in endings {
        let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
        rig.pump().await;
        let caller = rig.submit(1);
        rig.pump().await;
        let first = rig.jobs_of(1).remove(0);
        let written = pack_key(&first);
        let url = format!("https://store.test/{written}");
        assert_eq!(
            rig.presign.put(&url),
            Ok(()),
            "{ending:?}: the pack is written"
        );
        match &ending {
            Ending::Exits(code) => rig.host_finishes(first.id(), HOST, *code),
            Ending::Cancelled => {
                rig.api.cancel_job(first.id()).await.expect("cancelled");
            }
            Ending::Scripted(job, result, reason, exit_code) => {
                rig.host_runs(first.id(), HOST);
                script(&rig, first.id(), (*job, *result, *reason, *exit_code));
            }
        }
        rig.pump().await;
        assert_eq!(read_keys(&rig), Vec::<String>::new(), "{ending:?}");
        if then == Then::TimedOut {
            match rig.until_done(&caller, 4).await {
                Finished::TimedOut { status, .. } | Finished::Failed(status) => {
                    assert_eq!(Code::from_i32(status.code), Code::DeadlineExceeded);
                }
                other => panic!("{ending:?}: not a timeout: {other:?}"),
            }
            assert_eq!(rig.jobs_of(1).len(), 1, "{ending:?} was run again");
            assert_eq!(read_keys(&rig), Vec::<String>::new(), "{ending:?}");
            continue;
        }
        assert!(
            caller.finished().is_none(),
            "{ending:?} answered the caller"
        );
        let jobs = rig.jobs_of(1);
        assert_eq!(jobs.len(), 2, "{ending:?} was not run again: {jobs:?}");
        let rerun = pack_key(&jobs[1]);
        assert_ne!(rerun, written, "{ending:?}");
        rig.host_finishes(jobs[1].id(), HOST, 0);
        match rig.until_done(&caller, 4).await {
            Finished::Ran(result) => assert_eq!(result.exit_code, 0, "{ending:?}"),
            other => panic!("{ending:?}: did not run: {other:?}"),
        }
        assert_eq!(read_keys(&rig), vec![rerun], "{ending:?}");
    }
}

/// Catches the clean result not being promoted: a job that wrote its pack and ended with the
/// job and its result succeeded, for its exit, with exit code 0, has exactly that key read,
/// once, and the caller is answered from it without a rerun.
#[tokio::test(start_paused = true)]
async fn the_clean_result_promotes_the_pack_it_wrote() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(1);
    rig.pump().await;
    let job = rig.jobs_of(1).remove(0);
    let written = pack_key(&job);
    let url = format!("https://store.test/{written}");
    assert_eq!(rig.presign.put(&url), Ok(()));
    rig.host_runs(job.id(), HOST);
    script(
        &rig,
        job.id(),
        (
            JobState::Succeeded,
            JobState::Succeeded,
            ResultReason::Exit,
            Some(0),
        ),
    );
    match rig.until_done(&caller, 4).await {
        Finished::Ran(result) => assert_eq!(result.exit_code, 0),
        other => panic!("did not run: {other:?}"),
    }
    assert_eq!(read_keys(&rig), vec![written]);
    assert_eq!(rig.jobs_of(1).len(), 1);
}
