//! What a task is given to write to, and what bounds it.

use std::time::Duration;

use kbf_front::Finished;
use kbf_server::executor::Config;

use crate::rig::{GIB, Rig};

const HOST: &str = "host-1";

/// Catches a pack key that two jobs can share (a counter, the attempt number, a value kept
/// per process): a zombie of the first job, which still holds its url, would write the key
/// the second job must create, and the create-only store would refuse the second job's own
/// pack. Each submit draws a key of its own, so the zombie's late write lands on a key nobody
/// reads, and the job that is read is never refused.
#[tokio::test(start_paused = true)]
async fn a_zombie_write_cannot_land_on_the_key_of_its_rerun() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(1);
    rig.pump().await;
    let first = rig.jobs_of(1).remove(0);
    // The task tool fails (infrastructure): the farm runs the action again.
    rig.host_finishes(first.id(), HOST, 12);
    rig.pump().await;
    let second = rig.jobs_of(1).remove(1);
    let (zombie_url, url) = (
        first.arg("--pack-url").expect("a url").to_owned(),
        second.arg("--pack-url").expect("a url").to_owned(),
    );
    assert_ne!(zombie_url, url);
    let keys = rig.presign.keys();
    assert_eq!(keys.len(), 2);
    assert_ne!(keys[0], keys[1]);
    for (key, posted) in keys.iter().zip([&first, &second]) {
        let lease = posted.arg("--lease").expect("a lease");
        assert!(
            key.starts_with(&format!("cas/b/{lease}/")) && key.ends_with("-0"),
            "{key} for lease {lease}"
        );
    }
    // The zombie writes first, and the rerun still creates its own key.
    assert_eq!(rig.presign.put(&zombie_url), Ok(()));
    assert_eq!(rig.presign.put(&url), Ok(()));
    assert!(rig.presign.put(&url).is_err(), "the store is create-only");
    rig.host_finishes(second.id(), HOST, 0);
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
    let read = rig.ingest.calls.lock().expect("calls").clone();
    assert_eq!(read.len(), 1);
    assert_eq!(
        read[0].1, keys[1],
        "the rerun's pack is read, not the zombie's"
    );
}

/// Catches a url that outlives the job it was minted for by more than the margin, or one that
/// dies before the job does (the whole task holds it): it is good for the job's wall limit,
/// the action's timeout plus the margin, and the job's wall limit is the same number.
#[tokio::test(start_paused = true)]
async fn the_url_and_the_wall_time_are_the_actions_timeout_plus_the_margin() {
    let mut rig = Rig::start(&[HOST], 1, |_| {}).await;
    rig.pump().await;
    let timeout = Duration::from_secs(1_800);
    rig.set_timeout(timeout);
    let _caller = rig.submit(1);
    rig.pump().await;
    let job = rig.jobs_of(1).remove(0);
    let wall = timeout + rig.cfg.run_margin;
    assert_eq!(rig.presign.minted.lock().expect("minted")[0].expires, wall);
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

/// Catches a hard memory cap other than the container driver's (a plain booking would let a
/// job use half again what the farm booked; a bigger cap would overbook the host): two jobs
/// of 1 GiB booked on a worker of exactly their caps (2 GiB each) are both submitted with that
/// cap as the memory they need, and the worker takes no third.
#[tokio::test(start_paused = true)]
async fn the_hard_caps_of_the_booked_runs_fill_the_shape_exactly() {
    let mut rig = Rig::shaped(&[HOST], 1, 4, |_| {}).await;
    rig.pump().await;
    let callers = [rig.submit(1), rig.submit(2)];
    rig.pump().await;
    let jobs = rig.jobs();
    assert_eq!(jobs.len(), 2, "the caps did not fit the shape: {jobs:?}");
    for job in &jobs {
        let needs = job.spec.needs.as_ref().expect("needs");
        assert_eq!(needs.memory_bytes, Config::hard_memory(GIB), "{job:?}");
    }
    let caps: u64 = jobs
        .iter()
        .map(|job| {
            job.spec
                .needs
                .as_ref()
                .map_or(0, |needs| needs.memory_bytes)
        })
        .sum();
    assert_eq!(caps, 4 * GIB, "the caps are not the shape");
    let third = rig.submit(3);
    rig.advance(Duration::from_secs(5)).await;
    rig.pump().await;
    assert!(rig.jobs_of(3).is_empty());
    assert!(third.finished().is_none());
    drop(callers);
}
