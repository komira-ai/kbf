use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use kbf_front::Cache;

use super::follow;
use crate::farm::Farm;

/// Catches: two followers over one farm, which a stand-down and a refused write can
/// both start (each would look in the store at every interval for ever): only the first
/// call starts one, whichever it is.
#[tokio::test]
async fn only_the_first_of_a_stand_down_and_a_fenced_write_starts_the_follower() {
    let farm = Arc::new(Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    ));
    let started = AtomicBool::new(false);
    assert!(
        follow(Arc::clone(&farm), &started),
        "the first call starts it"
    );
    assert!(
        !follow(Arc::clone(&farm), &started),
        "the second started another"
    );
    assert!(!follow(farm, &started));
}
