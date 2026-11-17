//! An Execute end to end over the executor and a fake job service: a miss runs as one job and
//! its result is cached; a server with no executor refuses it.

mod support;

use support::{Cell, Job, Ran, done, response};
use tonic::Code;

#[tokio::test]
async fn an_execute_that_misses_the_cache_runs_as_one_job_and_its_result_is_cached() {
    let cell = Cell::start().await;
    let job = Job::new("/bin/run", &[]);
    cell.upload(&job.blobs()).await;
    cell.direct
        .will_run(&job.action, Ran::writes("out", "hello").stdout("log"));
    assert!(cell.cached(&job.action).await.is_err(), "a miss");

    let mut ops = cell.execute(&job.action).await;
    let id = cell.direct.complete(&job.action).await;
    let result = response(&done(&mut ops).await).result.expect("a result");

    assert_eq!(result.exit_code, 0);
    assert_eq!(result.output_files[0].path, "out");
    let out = support::Blob::new("hello");
    assert_eq!(result.output_files[0].digest, Some(out.proto.clone()));
    assert!(cell.holds(&out).await, "the output is in the CAS");
    assert_eq!(cell.direct.jobs_of(&job.action).len(), 1, "one job: {id}");
    assert_eq!(cell.cached(&job.action).await.expect("cached"), result);

    // The second Execute is a cache hit: nothing is submitted.
    let mut again = cell.execute(&job.action).await;
    let hit = response(&done(&mut again).await);
    assert!(hit.cached_result, "answered from the action cache");
    assert_eq!(hit.result.map(support::unstamped), Some(result));
    assert_eq!(cell.direct.submitted_jobs(), 1);
}

#[tokio::test]
async fn a_nonzero_exit_of_the_action_is_answered_and_not_cached() {
    let cell = Cell::start().await;
    let job = Job::new("/bin/fail", &[]);
    cell.upload(&job.blobs()).await;
    cell.direct.will_run(&job.action, Ran::exits(3));

    let mut ops = cell.execute(&job.action).await;
    cell.direct.complete(&job.action).await;
    let result = response(&done(&mut ops).await).result.expect("a result");

    assert_eq!(result.exit_code, 3);
    assert_eq!(cell.cached(&job.action).await, Err(Code::NotFound));
}

#[tokio::test]
async fn a_lost_job_runs_again_as_the_next_attempt_and_the_action_is_answered() {
    let cell = Cell::start().await;
    let job = Job::new("/bin/run", &[]);
    cell.upload(&job.blobs()).await;
    cell.direct
        .will_run(&job.action, Ran::writes("out", "second"));

    let mut ops = cell.execute(&job.action).await;
    let first = cell.direct.lose(&job.action, 0).await;
    let second = cell.direct.job(&job.action, 1).await;
    cell.direct.run_job(&second);
    assert!(cell.direct.fake.finish(&second, 0));
    let result = response(&done(&mut ops).await).result.expect("a result");

    assert_ne!(first, second, "the next attempt is a new job");
    assert_eq!(result.output_files[0].path, "out");
    assert_eq!(cell.direct.jobs_of(&job.action).len(), 2);
}

#[tokio::test]
async fn a_job_that_hits_its_wall_limit_answers_deadline_exceeded_and_runs_once() {
    let cell = Cell::start().await;
    let job = Job::new("/bin/slow", &[]);
    cell.upload(&job.blobs()).await;

    let mut ops = cell.execute(&job.action).await;
    cell.direct.time_out(&job.action).await;
    let status = response(&done(&mut ops).await).status.expect("a status");

    assert_eq!(status.code, Code::DeadlineExceeded as i32);
    assert_eq!(cell.direct.jobs_of(&job.action).len(), 1);
    assert!(cell.direct.ingestions().is_empty(), "no pack is read");
}

#[tokio::test]
async fn a_server_with_no_executor_refuses_an_execute_the_cache_cannot_answer() {
    let cell = Cell::start_without_executor().await;
    let job = Job::new("/bin/run", &[]);
    cell.upload(&job.blobs()).await;

    let refused = match cell.execute_with(&job.action, false).await {
        Ok(mut ops) => done_error(&mut ops).await,
        Err(status) => status,
    };

    assert_eq!(refused.code(), Code::FailedPrecondition);
    assert_eq!(cell.direct.submitted_jobs(), 0);
}

/// The error a stream ends with.
async fn done_error(
    ops: &mut tonic::Streaming<kbf_proto::google::longrunning::Operation>,
) -> tonic::Status {
    loop {
        match ops.message().await {
            Err(status) => return status,
            Ok(Some(_)) => {}
            Ok(None) => panic!("the stream ended without an error"),
        }
    }
}
