//! An update of a frontend over a state store while a build runs across it, as a rolling update
//! with a load balancer in front runs it: the new frontend starts beside the old one over the
//! same database, job service and pack store; the old one is given its stop signal; and the
//! client, which plays Buck2, sends its later calls to the new one on a connection of its own,
//! as the load balancer gives a new connection to the frontend that is ready. Buck2 keeps an
//! Execute stream open for the whole of its action and never re-attaches one that breaks
//! (`docs/api.md`, the drain), so every action of the build must end on the stream it
//! began on, whichever frontend that is, and the old frontend must stand down only once the
//! last of its streams has ended.
//!
//! Each frontend is started as a start is (`start_frontend`), so it runs the real drain sequence
//! and the real executor; the waits of the drain are ended by hand and followed by its phases
//! ([`kbf_server::Drain::phases`]), never by the clock. The tests need a Postgres server and fail
//! without one: `KBF_TEST_POSTGRES_URL` names a server they may create and drop databases on.

mod support;

#[path = "frontend_update/rig.rs"]
mod rig;

use kbf_proto::google::longrunning::Operation;
use kbf_proto::reapi::execution_stage::Value as ExecStage;
use kbf_server::drain::Phase;
use rig::{Farm, Front};
use support::{Job, done, response, stage};
use tonic::Streaming;

/// Reads `ops` until the update that says the action is executing.
async fn executing(ops: &mut Streaming<Operation>) {
    loop {
        let op = tokio::time::timeout(support::PROMPT, ops.message())
            .await
            .expect("an update in time")
            .expect("a healthy stream")
            .expect("an update before the end");
        if stage(&op) == ExecStage::Executing as i32 {
            return;
        }
    }
}

/// Asserts that `ops` ends with the done operation of an action that ran and exited 0, on the
/// stream it began on.
async fn succeeds(ops: &mut Streaming<Operation>, what: &str) {
    let op = done(ops).await;
    let answer = response(&op);
    let status = answer.status.unwrap_or_default();
    assert_eq!(status.code, 0, "{what}: {status:?}");
    assert_eq!(
        answer.result.expect("a result").exit_code,
        0,
        "{what}: the action's exit code"
    );
}

/// Gives the old frontend its stop signal and ends its load balancer wait: from then on it
/// refuses new Execute calls and its listeners are sent GOAWAY, and it waits for its streams.
async fn update(old: &mut Front) {
    old.signal();
    old.reaches(Phase::LbWait).await;
    old.drain.end_lb_wait();
    old.reaches(Phase::Draining).await;
}

/// Asserts that a build whose actions run across an update all succeed, each on the stream it
/// began on: three actions running as jobs on the old frontend when it is stopped, and one the
/// build sends after, to the new frontend. The old frontend stays up, `draining`, while any of
/// its streams is open (checked after each but the last of its jobs ends), and stands down and
/// stops serving only once the last has ended. No action is run twice.
///
/// Catches an update that fails a running build: a drain that stands down, or ends streams,
/// while an action of the old frontend still runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_action_of_a_build_running_across_an_update_succeeds() {
    let farm = Farm::start("build").await;
    let mut old = farm.front("front-old");
    let client = old.client().await;
    let before: Vec<Job> = (0..3)
        .map(|i| Job::new(&format!("run before the update {i}"), &[]))
        .collect();
    let mut streams = Vec::new();
    for job in &before {
        client.upload(&job.blobs()).await;
        let mut ops = client.execute(&job.action).await;
        let id = farm.job_of(&job.action).await;
        farm.runs(&id);
        executing(&mut ops).await;
        streams.push((job, id, ops));
    }

    let new = farm.front("front-new");
    update(&mut old).await;
    // The build's next action goes to the new frontend, on a connection of its own.
    let after = Job::new("run after the update", &[]);
    let moved = new.client().await;
    moved.upload(&after.blobs()).await;
    let mut late = moved.execute(&after.action).await;
    let late_id = farm.job_of(&after.action).await;
    farm.runs(&late_id);
    executing(&mut late).await;

    let last = streams.len() - 1;
    for (n, (job, id, ops)) in streams.iter_mut().enumerate() {
        assert_eq!(
            old.drain.phase(),
            Phase::Draining,
            "{} streams open",
            last + 1 - n
        );
        assert!(
            !old.has_ended(),
            "the old frontend stopped with a stream open"
        );
        farm.finishes(id, &job.action).await;
        succeeds(ops, &format!("action {n} of the old frontend")).await;
    }
    old.reaches(Phase::Closing).await;
    old.exits()
        .await
        .expect("the old frontend ends without an error");

    farm.finishes(&late_id, &after.action).await;
    succeeds(&mut late, "the action sent after the update").await;
    assert_eq!(farm.jobs.jobs().len(), 4, "an action was run twice");
}

/// Asserts that an action the old frontend took before the update, whose job had not yet
/// started when the update came, starts and ends after it and succeeds on its stream, while the
/// new frontend serves the build's other actions; and that the old frontend stands down once
/// that stream has ended.
///
/// Catches a drain that ends, or stands down over, an execution that is still queued: one whose
/// stream has not yet reported EXECUTING.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_action_queued_before_an_update_runs_after_it_and_succeeds() {
    let farm = Farm::start("queued").await;
    let mut old = farm.front("front-old");
    let client = old.client().await;
    let queued = Job::new("queued across the update", &[]);
    client.upload(&queued.blobs()).await;
    let mut ops = client.execute(&queued.action).await;
    let id = farm.job_of(&queued.action).await;

    let new = farm.front("front-new");
    update(&mut old).await;
    let other = Job::new("served by the new frontend", &[]);
    let moved = new.client().await;
    moved.upload(&other.blobs()).await;
    let mut other_ops = moved.execute(&other.action).await;
    let other_id = farm.job_of(&other.action).await;
    farm.runs(&other_id);
    farm.finishes(&other_id, &other.action).await;
    succeeds(&mut other_ops, "the new frontend's action").await;
    assert_eq!(old.drain.phase(), Phase::Draining);
    assert!(
        !old.has_ended(),
        "the old frontend stopped with a stream open"
    );

    farm.runs(&id);
    executing(&mut ops).await;
    assert_eq!(old.drain.phase(), Phase::Draining);
    farm.finishes(&id, &queued.action).await;
    succeeds(&mut ops, "the action queued across the update").await;
    old.reaches(Phase::Closing).await;
    old.exits()
        .await
        .expect("the old frontend ends without an error");
    assert_eq!(farm.jobs.jobs().len(), 2, "an action was run twice");
}
