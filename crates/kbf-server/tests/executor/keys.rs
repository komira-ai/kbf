//! What a task is given to write to, and what bounds it.

use std::time::Duration;

use kbf_front::Finished;
use kbf_lease_client::Outcome;

use crate::rig::Rig;

/// Catches a pack key that two runs can share (a counter, the attempt number, a value kept
/// per process): a zombie of the first run, which still holds its url, would write the key
/// the second run must create, and the create-only store would refuse the second run's own
/// pack. Each post draws a key of its own, so the zombie's late write lands on a key nobody
/// reads, and the run that is read is never refused.
#[tokio::test(start_paused = true)]
async fn a_zombie_write_cannot_land_on_the_key_of_its_rerun() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let caller = rig.submit(1);
    rig.pump().await;
    let first = rig.runs_of(1).await.remove(0);
    // The task tool fails (infrastructure): the farm runs the action again.
    rig.host_finishes(&first.run, Outcome::Failed, Some(12));
    rig.pump().await;
    let second = rig.runs_of(1).await.remove(1);
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
    rig.host_finishes(&second.run, Outcome::Succeeded, Some(0));
    assert!(matches!(rig.until_done(&caller, 4).await, Finished::Ran(_)));
    let read = rig.ingest.calls.lock().expect("calls").clone();
    assert_eq!(read.len(), 1);
    assert_eq!(
        read[0].1, keys[1],
        "the rerun's pack is read, not the zombie's"
    );
}

/// Catches a url that outlives the run it was minted for by more than the margin, or one that
/// dies before the run does (the whole task holds it): it is good for the run's wall time, the
/// action's timeout plus the margin, and the run's wall time is the same number.
#[tokio::test(start_paused = true)]
async fn the_url_and_the_wall_time_are_the_actions_timeout_plus_the_margin() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    let timeout = Duration::from_secs(1_800);
    rig.inputs
        .facts
        .lock()
        .expect("facts")
        .as_mut()
        .expect("ok")
        .timeout = Some(timeout);
    let _caller = rig.submit(1);
    rig.pump().await;
    let run = rig.runs_of(1).await.remove(0);
    let wall = timeout + rig.cfg.run_margin;
    assert_eq!(rig.presign.minted.lock().expect("minted")[0].expires, wall);
    assert_eq!(
        rig.fake.run_wall_ms(&run.run),
        Some(wall.as_millis() as u64)
    );
    assert_eq!(run.arg("--deadline-ms"), Some("1800000"));
}

/// Catches an action with no timeout running without a bound: it gets the executor's default.
#[tokio::test(start_paused = true)]
async fn an_action_without_a_timeout_gets_the_default() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
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
    let run = rig.runs_of(1).await.remove(0);
    assert_eq!(run.arg("--deadline-ms"), Some("3000000"));
    assert_eq!(rig.fake.run_wall_ms(&run.run), Some(3_300_000));
}

/// Catches a hard memory cap other than the container driver's (a plain booking would let a
/// run use half again what the farm booked; a bigger cap would overbook the host): two runs
/// of 1 GiB booked on a host of exactly their caps (2 GiB each) are both admitted, and the
/// host takes no third.
#[tokio::test(start_paused = true)]
async fn the_hard_caps_of_the_booked_runs_fill_the_shape_exactly() {
    let mut rig = Rig::shaped(&["node1"], 1, 4, |_| {}).await;
    rig.pump().await;
    let callers = [rig.submit(1), rig.submit(2)];
    rig.pump().await;
    assert_eq!(rig.runs().await.len(), 2, "the caps did not fit the shape");
    let third = rig.submit(3);
    rig.advance(Duration::from_secs(5)).await;
    rig.pump().await;
    assert!(rig.runs_of(3).await.is_empty());
    assert!(third.finished().is_none());
    drop(callers);
}

/// Catches a run cut short by its lease: the lease server cuts a run's wall time to what the
/// lease has left when the run is posted, and a renewal later does not stretch it. With a ttl
/// that only just covers the longest run, any time since the last renewal is time the run
/// would lose, so the lease is renewed first, and the run gets its whole wall time.
#[tokio::test(start_paused = true)]
async fn the_lease_is_renewed_before_a_run_that_would_outlive_it() {
    let mut rig = Rig::start(&["node1"], 1, |cfg| {
        cfg.lease_ttl = cfg.max_run + cfg.run_margin + cfg.heartbeat * 2;
        cfg.drain_before_end = cfg.lease_ttl + Duration::from_secs(60);
    })
    .await;
    rig.pump().await;
    rig.inputs
        .facts
        .lock()
        .expect("facts")
        .as_mut()
        .expect("ok")
        .timeout = Some(rig.cfg.max_run);
    // More than the two heartbeats the ttl has over the longest run.
    rig.elapse(15).await;
    let _caller = rig.submit(1);
    rig.pump().await;
    let run = rig.runs_of(1).await.remove(0);
    let wall = rig.cfg.max_run + rig.cfg.run_margin;
    assert_eq!(
        rig.fake.run_wall_ms(&run.run),
        Some(wall.as_millis() as u64)
    );
    let requests = rig.requests();
    let renewed = requests
        .iter()
        .position(|r| r.ends_with("/renew"))
        .expect("a renewal");
    let posted = requests
        .iter()
        .position(|r| r.ends_with("/runs") && r.starts_with("POST"))
        .expect("a post");
    assert!(
        renewed < posted,
        "the run was posted before the lease was renewed"
    );
}
