//! What a task is given to write to (its put directory), and what bounds it.

use std::time::Duration;

use js_proto::v1::mount;
use kbf_front::Finished;

use crate::rig::{HOST, Posted, Rig};

/// The host directory a job commits into: its one read-write mount's host path.
fn out_dir(posted: &Posted) -> String {
    match posted.spec.mounts.first().and_then(|m| m.source.clone()) {
        Some(mount::Source::HostPath(path)) => path,
        other => panic!("no host directory mounted: {other:?}"),
    }
}

/// Catches a put directory that two jobs can share (a counter, the attempt number, a value kept
/// per process): a zombie of the first job, which still holds its mount, would write into the
/// directory the second job commits into. Each submit draws a directory of its own under its
/// run's build id, and the directory the frontend reads is the rerun's, not the zombie's.
#[tokio::test]
async fn a_zombie_write_cannot_land_in_the_directory_of_its_rerun() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let mut caller = rig.submit(1);
    let first = rig.first_job(1).await;
    // The task tool fails (infrastructure): the operation runs again.
    rig.host_finishes(first.id(), HOST, 12);
    let second = rig.until_jobs(1, 2).await.remove(1);
    let (zombie, dir) = (out_dir(&first), out_dir(&second));
    assert_ne!(zombie, dir);
    for (path, posted) in [&zombie, &dir].into_iter().zip([&first, &second]) {
        let run = posted.arg("--lease").expect("a run");
        assert!(
            path.starts_with(&format!("/mnt/ts/put/cas/b/{run}/")) && path.ends_with("-0"),
            "{path} for run {run}"
        );
    }
    rig.host_finishes(second.id(), HOST, 0);
    assert!(matches!(caller.done().await, Finished::Ran(_)));
    let read = rig.ingest.keys_read();
    assert_eq!(
        format!("/mnt/ts/put/{}", read[0]),
        dir,
        "the rerun's directory is read, not the zombie's"
    );
}

/// Catches a put directory that outlives the job it was minted for by more than the margin, or
/// one that dies before the job does (the whole task holds it): the mint is asked for the job's
/// wall limit, the action's timeout plus the margin, and the job's wall limit is the same number.
#[tokio::test]
async fn the_directory_and_the_wall_time_are_the_actions_timeout_plus_the_margin() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let timeout = Duration::from_secs(1_800);
    rig.set_timeout(timeout);
    let _caller = rig.submit(1);
    let job = rig.first_job(1).await;
    let wall = timeout + rig.cfg.run_margin;
    assert_eq!(*rig.ingest.walls.lock().expect("walls"), vec![wall]);
    assert_eq!(job.spec.wall_limit_seconds, Some(wall.as_secs()));
    assert_eq!(job.arg("--deadline-ms"), Some("1800000"));
}

/// Catches an action with no timeout running without a bound: it gets the executor's default.
#[tokio::test]
async fn an_action_without_a_timeout_gets_the_default() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    rig.inputs
        .facts
        .lock()
        .expect("facts")
        .as_mut()
        .expect("ok")
        .timeout = None;
    let _caller = rig.submit(1);
    let job = rig.first_job(1).await;
    assert_eq!(job.arg("--deadline-ms"), Some("3000000"));
    assert_eq!(job.spec.wall_limit_seconds, Some(3_300));
}

/// Catches kbf holding actions back or sizing them: six actions are all submitted at once,
/// whatever the host could take, and no job names a CPU or a memory (the job service sizes each
/// from its class's history and queues what does not fit).
#[tokio::test]
async fn every_action_is_submitted_at_once_and_its_job_names_no_size() {
    let rig = Rig::start(&[HOST], |_| {}).await;
    let callers: Vec<_> = (1..=6).map(|tag| rig.submit(tag)).collect();
    for tag in 1..=6 {
        rig.first_job(tag).await;
    }
    let jobs = rig.jobs();
    assert_eq!(jobs.len(), 6, "{jobs:?}");
    for job in &jobs {
        let needs = job.spec.needs.as_ref().expect("needs");
        assert_eq!((needs.cpu_millis, needs.memory_bytes), (0, 0), "{job:?}");
    }
    assert!(callers.iter().all(|caller| caller.finished().is_none()));
    assert_eq!(rig.exec.open(), 6);
}
