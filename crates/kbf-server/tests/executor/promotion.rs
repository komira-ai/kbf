//! The promotion rule: the frontend reads a job's put directory, and promotes its outputs, only
//! from a clean result (the job and its result succeeded, for the job's exit, with exit code 0).
//! A job that committed its outputs and then ended any other way has its directory left unread.
//!
//! The fake job service ends a job by exit code only, so every other ending is scripted: the
//! answers to the executor's next read of the job and of its result are replaced by ones that
//! say how it ended.

use js_proto::v1::{JobState, ResultReason, mount};
use kbf_front::Finished;
use tonic::Code;

use crate::rig::{HOST, Posted, Rig};

/// How the first job of the action ends, after it committed its outputs.
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

/// The key of the put directory `job` commits into, from the host path it mounts at `/out`.
fn dir_key(job: &Posted) -> String {
    let source = job.spec.mounts.first().and_then(|m| m.source.clone());
    let Some(mount::Source::HostPath(path)) = source else {
        panic!("no host directory mounted: {:?}", job.spec.mounts);
    };
    path.strip_prefix("/mnt/ts/put/")
        .expect("a directory under the put root")
        .to_owned()
}

/// Catches a directory the job already committed into being read, and its outputs promoted, from a
/// result that is not clean: a nonzero exit of the task tool, a cancel by someone else, the
/// wall limit, the memory limit, and a job that says it succeeded with a result that has no
/// exit code, a nonzero one, a reason other than its exit, or a state other than succeeded.
/// For each, the first job's directory is never read; the action runs again (or, for
/// the wall limit, is answered `DEADLINE_EXCEEDED`), and only the rerun's clean end is read.
#[tokio::test]
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
        let rig = Rig::start(&[HOST], |_| {}).await;
        let mut caller = rig.submit(1);
        let first = rig.first_job(1).await;
        let written = dir_key(&first);
        match &ending {
            Ending::Exits(code) => rig.host_finishes(first.id(), HOST, *code),
            Ending::Cancelled => {
                rig.api.cancel_job(first.id()).await.expect("cancelled");
            }
            Ending::Scripted(job, result, reason, exit_code) => {
                rig.host_runs(first.id(), HOST);
                rig.script_ending(first.id(), (*job, *result, *reason, *exit_code));
            }
        }
        if then == Then::TimedOut {
            match caller.done().await {
                Finished::TimedOut { status, .. } | Finished::Failed(status) => {
                    assert_eq!(Code::from_i32(status.code), Code::DeadlineExceeded);
                }
                other => panic!("{ending:?}: not a timeout: {other:?}"),
            }
            assert_eq!(rig.jobs_of(1).len(), 1, "{ending:?} was run again");
            assert_eq!(rig.ingest.keys_read(), Vec::<String>::new(), "{ending:?}");
            continue;
        }
        let jobs = rig.until_jobs(1, 2).await;
        assert!(
            caller.finished().is_none(),
            "{ending:?} answered the caller"
        );
        assert_eq!(rig.ingest.keys_read(), Vec::<String>::new(), "{ending:?}");
        let rerun = dir_key(&jobs[1]);
        assert_ne!(rerun, written, "{ending:?}");
        rig.host_finishes(jobs[1].id(), HOST, 0);
        match caller.done().await {
            Finished::Ran(result) => assert_eq!(result.exit_code, 0, "{ending:?}"),
            other => panic!("{ending:?}: did not run: {other:?}"),
        }
        assert_eq!(rig.ingest.keys_read(), vec![rerun], "{ending:?}");
    }
}

/// Catches the clean result not being promoted: a job that committed its outputs and ended with the
/// job and its result succeeded, for its exit, with exit code 0, has exactly that key read,
/// once, and the caller is answered from it without a rerun.
#[tokio::test]
async fn the_clean_result_promotes_the_directory_it_committed() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut caller = rig.submit(1);
    let job = rig.first_job(1).await;
    let written = dir_key(&job);
    rig.host_runs(job.id(), HOST);
    rig.script_ending(
        job.id(),
        (
            JobState::Succeeded,
            JobState::Succeeded,
            ResultReason::Exit,
            Some(0),
        ),
    );
    match caller.done().await {
        Finished::Ran(result) => assert_eq!(result.exit_code, 0),
        other => panic!("did not run: {other:?}"),
    }
    assert_eq!(rig.ingest.keys_read(), vec![written]);
    assert_eq!(rig.jobs_of(1).len(), 1);
}
