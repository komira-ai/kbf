//! What the server logs when a job ends for the farm's reason (the task tool exited nonzero):
//! the client is told only that the job did not run to a result, so the job's id and the reason
//! must reach the server's log, or nobody can tell which job to ask the job service about. A
//! module of the `execute` test binary, which installs one log capture for all its tests
//! (`log.rs`).

use crate::log::{capture, lines_with};
use crate::support::{Cell, Job, done, response};
use tonic::Code;

/// Asserts that a job that ends in an infrastructure failure at every attempt logs, once per
/// attempt, the reason and the id of the job at WARN; and that the client is answered with the
/// reason's own words, UNAVAILABLE, and no job id.
#[tokio::test]
async fn an_infrastructure_failure_logs_the_job_and_its_reason() {
    capture();
    let cell = Cell::start().await;
    let job = Job::new("needs an image", &[]);
    cell.upload(&job.blobs()).await;
    let mut ops = cell.execute(&job.action).await;
    let mut ids = Vec::new();
    for attempt in 0..3 {
        let id = cell.direct.job(&job.action, attempt).await;
        cell.direct.run_job(&id);
        assert!(cell.direct.fake.finish(&id, 7));
        ids.push(id);
    }

    let status = response(&done(&mut ops).await).status.expect("a status");
    assert_eq!(status.code, Code::Unavailable as i32, "{status:?}");
    assert_eq!(
        status.message,
        "the task tool exited nonzero (exit code Some(7))"
    );
    for id in &ids {
        assert!(!status.message.contains(id.as_str()), "the job id leaked");
        let logged = lines_with(&[id, "the task tool exited nonzero", "WARN"]);
        assert_eq!(logged.len(), 1, "{id}: {logged:?}");
    }
}
