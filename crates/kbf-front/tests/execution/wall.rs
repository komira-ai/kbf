//! The action's `timeout` on Execute: the longest an action may run, as the dispatch is
//! handed it.

use std::time::Duration;

use super::*;

/// What the dispatch is handed as the longest `timeout` lets an action run.
async fn wall_for(timeout: Option<prost_types::Duration>) -> Option<Duration> {
    let script = Arc::new(Script::default());
    let farm = Farm::with_execution(Arc::clone(&script)).await;
    let job = job_with_timeout("timed", &[], false, timeout);
    farm.upload(&job.blobs.iter().collect::<Vec<_>>()).await;
    start(&farm, &job.action).await.expect("Execute");
    let [submission] = script.submitted().try_into().expect("one submission");
    submission.max_wall
}

fn timeout(seconds: i64, nanos: i32) -> Option<prost_types::Duration> {
    Some(prost_types::Duration { seconds, nanos })
}

/// Catches: a timeout the dispatch is never handed (the scheduler then takes every
/// action to run for ever), one rounded to whole seconds, and a nanosecond part dropped.
#[tokio::test]
async fn an_actions_timeout_reaches_the_dispatch() {
    assert_eq!(
        wall_for(timeout(90, 0)).await,
        Some(Duration::from_secs(90))
    );
    assert_eq!(
        wall_for(timeout(1, 500_000_000)).await,
        Some(Duration::from_millis(1_500))
    );
}

/// Catches: an action with no timeout, a zero one (REAPI's "none") or a negative one (which
/// the daemon refuses) being given a wall time: the front does not know the daemon's
/// default, so it names none.
#[tokio::test]
async fn an_action_with_no_usable_timeout_names_no_wall_time() {
    assert_eq!(wall_for(None).await, None);
    assert_eq!(wall_for(timeout(0, 0)).await, None);
    assert_eq!(wall_for(timeout(-1, 0)).await, None);
    assert_eq!(wall_for(timeout(5, -1)).await, None);
}
