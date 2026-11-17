//! What a task is given to write to (its put directory), and what bounds it.

use std::time::Duration;

use js_proto::v1::mount;
use kbf_front::Finished;
use kbf_server::executor::Config;

use crate::rig::{GIB, Posted, Rig};

const HOST: &str = "host-1";

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
/// lease's build id, and the directory the frontend reads is the rerun's, not the zombie's.
#[tokio::test(start_paused = true)]
async fn a_zombie_write_cannot_land_in_the_directory_of_its_rerun() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(1);
    rig.pump().await;
    let first = rig.jobs_of(1).remove(0);
    // The task tool fails (infrastructure): the farm runs the action again.
    rig.host_finishes(first.id(), HOST, 12);
    rig.pump().await;
    let second = rig.jobs_of(1).remove(1);
    let (zombie, dir) = (out_dir(&first), out_dir(&second));
    assert_ne!(zombie, dir);
    for (path, posted) in [&zombie, &dir].into_iter().zip([&first, &second]) {
        let lease = posted.arg("--lease").expect("a lease");
        assert!(
            path.starts_with(&format!("/mnt/ts/put/cas/b/{lease}/")) && path.ends_with("-0"),
            "{path} for lease {lease}"
        );
    }
    rig.host_finishes(second.id(), HOST, 0);
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
    let read = rig.ingest.calls.lock().expect("calls").clone();
    assert_eq!(read.len(), 1);
    assert_eq!(
        format!("/mnt/ts/put/{}", read[0].1),
        dir,
        "the rerun's directory is read, not the zombie's"
    );
}

/// Catches a put directory that outlives the job it was minted for by more than the margin, or
/// one that dies before the job does (the whole task holds it): the mint is asked for the job's
/// wall limit, the action's timeout plus the margin, and the job's wall limit is the same number.
#[tokio::test(start_paused = true)]
async fn the_directory_and_the_wall_time_are_the_actions_timeout_plus_the_margin() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let timeout = Duration::from_secs(1_800);
    rig.set_timeout(timeout);
    let _caller = rig.submit(1);
    rig.pump().await;
    let job = rig.jobs_of(1).remove(0);
    let wall = timeout + rig.cfg.run_margin;
    assert_eq!(*rig.ingest.walls.lock().expect("walls"), vec![wall]);
    assert_eq!(job.spec.wall_limit_seconds, Some(wall.as_secs()));
    assert_eq!(job.arg("--deadline-ms"), Some("1800000"));
}

/// Catches an action with no timeout running without a bound: it gets the executor's default.
#[tokio::test(start_paused = true)]
async fn an_action_without_a_timeout_gets_the_default() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    rig.inputs
        .facts
        .lock()
        .expect("facts")
        .as_mut()
        .expect("ok")
        .timeout = None;
    let _caller = rig.submit(1);
    rig.pump().await;
    let job = rig.jobs_of(1).remove(0);
    assert_eq!(job.arg("--deadline-ms"), Some("3000000"));
    assert_eq!(job.spec.wall_limit_seconds, Some(3_300));
}

/// Catches the farm's booking leaking into what a Linux job asks the job service for, or no
/// longer pacing what a worker sends: two runs of 1 GiB booked on a worker whose shape holds
/// exactly their hard caps (2 GiB each) are both submitted naming no memory and no CPU (the
/// service sizes each from its class's history), and the worker takes no third.
#[tokio::test(start_paused = true)]
async fn the_booked_runs_fill_the_shape_and_their_jobs_name_no_size() {
    let mut rig = Rig::shaped(&[HOST], 1, 4, |_| {}).await;
    rig.pump().await;
    let callers = [rig.submit(1), rig.submit(2)];
    rig.pump().await;
    let jobs = rig.jobs();
    assert_eq!(jobs.len(), 2, "the caps did not fit the shape: {jobs:?}");
    assert_eq!(
        2 * Config::hard_memory(GIB),
        4 * GIB,
        "the caps are the shape"
    );
    for job in &jobs {
        let needs = job.spec.needs.as_ref().expect("needs");
        assert_eq!((needs.cpu_millis, needs.memory_bytes), (0, 0), "{job:?}");
    }
    let third = rig.submit(3);
    rig.advance(Duration::from_secs(5)).await;
    rig.pump().await;
    assert!(rig.jobs_of(3).is_empty());
    assert!(third.finished().is_none());
    drop(callers);
}
