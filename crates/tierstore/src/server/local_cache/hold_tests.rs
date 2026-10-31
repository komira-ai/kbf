//! The lock of the cache directory: who holds it, how a cache that does not hold it takes it,
//! and what a cache does when the file it holds is replaced.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use super::testing::Rig;
use super::{Mode, Tick};

/// Asserts that a cache holds the lock when it opens with the lock free, and that a second cache
/// over the directory while the first is open is read-only, finds the lock held at every try,
/// and takes it, becoming read-write, at the first try after the first cache is gone and not
/// before: it would catch two caches that both write to one directory.
#[tokio::test]
async fn a_second_cache_over_a_held_directory_is_read_only_until_the_first_is_gone() {
    let rig = Rig::new("tscache-hold-second");
    let first = rig.locked();
    assert_eq!(first.mode(), Mode::ReadWrite);
    let second = rig.locked();
    assert_eq!(second.mode(), Mode::ReadOnly);
    assert_eq!(first.tick().unwrap(), Tick::Held);
    for _ in 0..3 {
        assert_eq!(second.tick().unwrap(), Tick::Waiting);
        assert_eq!(second.mode(), Mode::ReadOnly);
    }
    drop(first);
    // The lock is free, and nothing has told the second cache.
    assert_eq!(second.mode(), Mode::ReadOnly);
    assert_eq!(second.tick().unwrap(), Tick::Took);
    assert_eq!(second.mode(), Mode::ReadWrite);
    assert_eq!(second.tick().unwrap(), Tick::Held);
    // A third is read-only beside the second, which holds it now.
    assert_eq!(rig.locked().mode(), Mode::ReadOnly);
}

/// Asserts that a cache that is alone opens read-write with a lock file of its own in the
/// directory, mode 0600, and that a cache opened without the lock (`open`) has none to try: it
/// is read-write at every tick and makes no lock file. It would catch a cache that is read-write
/// without holding anything.
#[tokio::test]
async fn a_lone_cache_holds_the_lock_at_open_and_a_lockless_one_never_tries() {
    let rig = Rig::new("tscache-hold-lone");
    let lock = rig.config.path.join(".lock");
    assert!(!lock.exists());
    assert_eq!(rig.cache.mode(), Mode::ReadWrite);
    assert_eq!(rig.cache.tick().unwrap(), Tick::Held);
    assert!(!lock.exists());
    let cache = rig.locked();
    assert!(lock.is_file());
    let mode = std::fs::metadata(&lock).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(cache.mode(), Mode::ReadWrite);
}

/// Asserts that a cache whose lock file was replaced lets the lock go at its next tick and is
/// read-only from then on, though no other cache holds the file it had locked; that it finds the
/// new file held by the cache that made it; and that it takes the new file when that cache is
/// gone. It would catch a holder that goes on writing under a lock that nothing else looks at,
/// beside a second cache that holds the new file.
#[tokio::test]
async fn a_replaced_lock_file_is_let_go_and_the_new_one_is_taken_when_it_is_free() {
    let rig = Rig::new("tscache-hold-replaced");
    let first = rig.locked();
    assert_eq!(first.tick().unwrap(), Tick::Held);
    std::fs::remove_file(rig.config.path.join(".lock")).unwrap();
    // A process that starts now makes a new lock file and holds it: two caches that both think
    // they hold the lock.
    let second = rig.locked();
    assert_eq!(second.mode(), Mode::ReadWrite);
    assert_eq!(first.tick().unwrap(), Tick::Lost);
    assert_eq!(first.mode(), Mode::ReadOnly);
    assert_eq!(first.tick().unwrap(), Tick::Waiting);
    assert_eq!(first.mode(), Mode::ReadOnly);
    drop(second);
    assert_eq!(first.tick().unwrap(), Tick::Took);
    assert_eq!(first.mode(), Mode::ReadWrite);
}

/// Asserts that a lock file that is gone from its path with nobody to replace it is lost as well,
/// and is made again and taken at the next tick, and that a lock file that is a link is an error
/// of the open and of the tick, not a lock: the open follows no link.
#[tokio::test]
async fn a_removed_lock_file_is_made_again_and_a_link_is_an_error() {
    let rig = Rig::new("tscache-hold-removed");
    let cache = rig.locked();
    let lock = rig.config.path.join(".lock");
    std::fs::remove_file(&lock).unwrap();
    assert_eq!(cache.tick().unwrap(), Tick::Lost);
    assert_eq!(cache.tick().unwrap(), Tick::Took);
    assert!(lock.is_file());
    assert_eq!(cache.mode(), Mode::ReadWrite);

    let elsewhere = rig.dir.path().join("elsewhere");
    std::fs::write(&elsewhere, b"").unwrap();
    std::fs::remove_file(&lock).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &lock).unwrap();
    assert!(rig.try_locked().is_err());
    assert_eq!(cache.tick().unwrap(), Tick::Lost);
    assert!(cache.tick().is_err());
    assert_eq!(cache.mode(), Mode::ReadOnly);
}

/// Asserts that a tick says what it changed and nothing else: the lock taken, the lock file
/// replaced, and an error, each with the directory; and nothing for a tick that found the lock
/// held, by this cache or another.
#[tokio::test]
async fn a_tick_says_what_it_changed_and_nothing_else() {
    let rig = Rig::new("tscache-hold-says");
    let cache = rig.locked();
    let path = rig.config.path.display().to_string();
    let said = |outcome: io::Result<Tick>| cache.inner.describe(&outcome);
    let took = said(Ok(Tick::Took)).unwrap();
    assert!(took.contains("took the lock"), "{took}");
    assert!(took.contains(&path), "{took}");
    let lost = said(Ok(Tick::Lost)).unwrap();
    assert!(lost.contains("replaced"), "{lost}");
    assert!(lost.contains(&path), "{lost}");
    let failed = said(Err(io::Error::other("no space"))).unwrap();
    assert!(failed.contains("no space"), "{failed}");
    assert!(failed.contains(&path), "{failed}");
    assert_eq!(said(Ok(Tick::Held)), None);
    assert_eq!(said(Ok(Tick::Waiting)), None);
    assert_eq!(Mode::ReadWrite.as_str(), "read-write");
    assert_eq!(Mode::ReadOnly.as_str(), "read-only");
}

/// Asserts that the gauge is 1 for the mode the cache is in and 0 for the other, in a cache that
/// holds the lock, in one that does not, and in the second one after it took the lock.
#[tokio::test]
async fn the_mode_gauge_is_one_for_the_mode_the_cache_is_in() {
    let rig = Rig::new("tscache-hold-gauge");
    let first = rig.locked();
    let second = rig.locked();
    let gauge = |cache: &super::LocalCache, only: u8, write: u8| {
        let text = cache.metrics();
        let only = format!("tierstore_cache_mode{{mode=\"read_only\"}} {only}\n");
        let write = format!("tierstore_cache_mode{{mode=\"read_write\"}} {write}\n");
        assert!(text.contains(&only), "{only} in {text}");
        assert!(text.contains(&write), "{write} in {text}");
    };
    gauge(&first, 0, 1);
    gauge(&second, 1, 0);
    drop(first);
    assert_eq!(second.tick().unwrap(), Tick::Took);
    gauge(&second, 0, 1);
}

/// Asserts that the loop tries the lock at the interval, takes it at a tick after it is free, and
/// ends when it is halted. The runtime's clock is paused and moved by the test, and the test
/// looks at the mode and at nothing of time: it would catch a loop that never tries again, or
/// one that does not stop.
#[tokio::test(start_paused = true)]
async fn the_loop_takes_the_lock_at_a_tick_after_it_is_free_and_ends_when_halted() {
    let rig = Rig::new("tscache-hold-loop");
    let first = rig.locked();
    let second = rig.locked();
    assert_eq!(second.mode(), Mode::ReadOnly);
    let halt = Arc::new(Notify::new());
    let cache = second.clone();
    let stop = Arc::clone(&halt);
    let looping = tokio::spawn(async move { cache.hold(Duration::from_secs(1), stop).await });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(second.mode(), Mode::ReadOnly);
    drop(first);
    tokio::time::advance(Duration::from_secs(1)).await;
    while second.mode() == Mode::ReadOnly {
        tokio::task::yield_now().await;
    }
    assert_eq!(second.mode(), Mode::ReadWrite);
    halt.notify_one();
    looping.await.unwrap();
}
