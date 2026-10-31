//! A fill that is running when the cache loses the lock of its directory.
//!
//! The lock file is replaced and another cache takes the new one: from then on the directory is
//! the other cache's, and the first must not write to it, or delete what it wrote.

use super::metrics::read;
use super::testing::{Rig, pattern};
use super::{Mode, Tick};
use crate::Backend;

/// Asserts that a fill that is in the middle of its fetch when the cache loses its lock
/// publishes nothing, counts no fill and puts nothing in the cache's index, though the upstream
/// then gives it every byte, and that the reader that started it is answered from the upstream;
/// and that it does not delete the staging file of the cache that holds the directory now (that
/// cache clears `.stage/` when it opens, and fills an object of its own, which is in the middle
/// of its fetch too): that fill is published, whole. It would catch a fill that goes on writing
/// into a directory another cache owns, and one whose cleanup removes a file of the new owner
/// because both name their staging files from a counter that starts at 0.
#[tokio::test]
async fn a_fill_in_flight_across_a_lost_lock_publishes_nothing_and_leaves_the_new_owners_files() {
    let rig = Rig::new("tscache-lost-fill");
    rig.put("cas/b/old", &pattern(30)).await;
    rig.put("cas/b/new", &pattern(40)).await;
    let first = rig.locked();
    let gate = rig.tamper.hold();
    let cache = first.clone();
    let old = tokio::spawn(async move { cache.get("cas/b/old", None).await });
    while read(&first.counters().staging_files) < 1 {
        tokio::task::yield_now().await;
    }

    // The lock file is replaced and another cache makes the new one and holds it.
    std::fs::remove_file(rig.config.path.join(".lock")).unwrap();
    let second = rig.locked();
    assert_eq!(second.mode(), Mode::ReadWrite);
    let cache = second.clone();
    let new = tokio::spawn(async move { cache.get("cas/b/new", None).await });
    while read(&second.counters().staging_files) < 1 {
        tokio::task::yield_now().await;
    }
    assert_eq!(first.tick().unwrap(), Tick::Lost);

    gate.add_permits(1000);
    assert_eq!(old.await.unwrap().unwrap(), pattern(30));
    assert_eq!(new.await.unwrap().unwrap(), pattern(40));
    assert!(!rig.has_file("cas/b/old"));
    assert_eq!(read(&first.counters().fills), 0);
    assert_eq!(first.held(), (0, 0));
    assert!(rig.has_file("cas/b/new"));
    assert_eq!(read(&second.counters().fills), 1);
    assert_eq!(second.held(), (40, 1));
    assert_eq!(rig.staged_files(), 0);
}
