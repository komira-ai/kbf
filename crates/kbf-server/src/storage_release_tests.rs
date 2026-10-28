//! What `StorageHandle::serve` does about the cold store's lock once serving ends, on
//! virtual time: it returns at once when the store is released, and at the deadline,
//! with one warning, when a handle on the store is leaked.

use std::future::ready;

use kbf_meta::Retention;
use kbf_objstore::{Capabilities, Floor, FsStore, MemoryStore, Released};

use super::{COLD_RELEASE_DEADLINE, StorageHandle, assemble, command, parse};
use crate::RaftMetaLog;
use crate::cold_apply::Fixed;
use crate::replication::REPAIR_DIR;
use crate::test_log::capture;

/// A new, empty directory for one test, removed when it drops.
fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    crate::test_dir::scratch_dir(&format!("kbf-release-{name}"))
}

/// A node whose cold store is `cold`, told to stop at once.
async fn node(dir: &std::path::Path, cold: Option<Released>) -> StorageHandle {
    let meta_dir = dir.join("meta");
    let argv = [
        "storage".to_owned(),
        "--storage-listen=127.0.0.1:0".to_owned(),
        format!("--cold-store=file://{}", dir.join("unused-cold").display()),
        format!("--meta-dir={}", meta_dir.display()),
    ];
    let matches = command().try_get_matches_from(argv).expect("the flags");
    let flags = parse(&matches).expect("the flags");
    let log = RaftMetaLog::open_dir(&meta_dir, Retention::default(), "kbf/")
        .await
        .expect("open the log");
    let (prefix, id) = (log.store_prefix(), log.log_id());
    let store = MemoryStore::new(Capabilities {
        conditional_put: true,
        object_lock: false,
    });
    assemble(
        &flags,
        flags.listener().expect("a listener"),
        None,
        (log, Fixed(store), prefix, id),
        (|| Ok(ready(())), &meta_dir.join(REPAIR_DIR)),
        (None, cold),
        kbf_front::RebuildConfig::default(),
    )
    .await
    .expect("the node assembles")
}

/// What a stop writes once the node's calls have ended and nothing is staged on the hot
/// store (`RamDrain::after`): in every capture below, so an absent line is absent from a
/// capture that holds the lines the serve wrote.
const STOP_LINE: &str = "no block is left staged";

/// Catches a `serve` that warns, or waits, when the cold store is released in time: the
/// only handle on the store is the node's, dropped by `serve`, so it returns with no
/// virtual time passed and nothing logged. Catches: the warning is unconditional.
#[tokio::test(start_paused = true)]
async fn serve_returns_at_once_and_quietly_when_the_store_is_released() {
    let dir = scratch("quiet");
    let cold = FsStore::open(&dir.join("cold"), Floor::Bytes(1)).expect("open");
    let handle = node(&dir, Some(cold.released())).await;
    let released = cold.released();
    drop(cold);
    let log = capture();
    let start = tokio::time::Instant::now();
    handle.serve().await.expect("serves");
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);
    released
        .wait_within(std::time::Duration::ZERO)
        .await
        .expect("released");
    assert!(log.text().contains(STOP_LINE), "{}", log.text());
    assert!(!log.text().contains("still open"), "{}", log.text());
}

/// Catches a `serve` that waits for a leaked handle with no end (a production stop that
/// never finishes), one that gives up early or late, and one that does not say so: with
/// a clone of the cold store kept, `serve` returns after exactly the deadline of virtual
/// time with one warning naming the directory and the live handles. Catches: the deadline
/// arm never warns.
#[tokio::test(start_paused = true)]
async fn serve_gives_up_at_the_deadline_with_one_warning_naming_the_handles() {
    let dir = scratch("leaked");
    let cold_dir = dir.join("cold");
    let cold = FsStore::open(&cold_dir, Floor::Bytes(1)).expect("open");
    let handle = node(&dir, Some(cold.released())).await;
    let log = capture();
    let start = tokio::time::Instant::now();
    handle.serve().await.expect("serves");
    assert_eq!(start.elapsed(), COLD_RELEASE_DEADLINE);
    let text = log.text();
    assert_eq!(text.matches("still open").count(), 1, "{text}");
    assert!(text.contains("WARN"), "{text}");
    assert!(text.contains(&cold_dir.display().to_string()), "{text}");
    assert!(text.contains("clones=1"), "{text}");
    drop(cold);
}

/// Catches the variant the tests call reporting a leaked handle as released (a leak that
/// then passes unseen), or ignoring its limit: with a clone kept it returns `StillHeld`
/// naming the one handle, after exactly the limit given, and says nothing in the log.
/// Catches: the variant turns the `StillHeld` into `Ok`.
#[tokio::test(start_paused = true)]
async fn serve_checked_reports_a_leaked_handle_at_its_own_limit() {
    let dir = scratch("checked");
    let cold = FsStore::open(&dir.join("cold"), Floor::Bytes(1)).expect("open");
    let handle = node(&dir, Some(cold.released())).await;
    let log = capture();
    let limit = std::time::Duration::from_secs(3);
    let start = tokio::time::Instant::now();
    let (served, released) = handle.serve_checked(limit).await;
    served.expect("serves");
    let held = released.expect_err("a clone is alive");
    assert_eq!((held.clones, held.limit), (1, limit));
    assert_eq!(start.elapsed(), limit);
    assert!(log.text().contains(STOP_LINE), "{}", log.text());
    assert!(!log.text().contains("still open"), "{}", log.text());
    drop(cold);
}

/// Catches a `serve` that does not wait for the cold store's last handle: a clone is held
/// until the test drops it, five virtual seconds in (the limit is ten), and `serve` must
/// still be running then and finish, with no warning, once the clone is gone. No clock
/// but the runtime's: it only moves when every task is idle, and `serve` is idle at its
/// wait. The drop is on the test's own task, so no thread's timing decides anything.
/// Catches: `serve` skips the wait.
#[tokio::test(start_paused = true)]
async fn serve_waits_for_a_held_clone_and_returns_when_it_is_dropped() {
    let dir = scratch("held");
    let cold = FsStore::open(&dir.join("cold"), Floor::Bytes(1)).expect("open");
    let handle = node(&dir, Some(cold.released())).await;
    let log = capture();
    let limit = std::time::Duration::from_secs(10);
    let serving = handle.serve_checked(limit);
    tokio::pin!(serving);
    tokio::select! {
        biased;
        _ = &mut serving => panic!("serve returned with a handle on the cold store alive"),
        () = tokio::time::sleep(limit / 2) => {}
    }
    drop(cold);
    let (served, released) = serving.await;
    served.expect("serves");
    released.expect("released once the clone is dropped");
    assert!(log.text().contains(STOP_LINE), "{}", log.text());
    assert!(!log.text().contains("still open"), "{}", log.text());
}

/// Catches a `serve` that waits for, or fails on, a cold store it does not have (a node
/// with an object-store cold tier has no lock to wait for): it returns at once, both
/// results `Ok`, with no virtual time passed and nothing logged.
#[tokio::test(start_paused = true)]
async fn serve_with_no_cold_lock_to_wait_for_returns_at_once() {
    let dir = scratch("none");
    let handle = node(&dir, None).await;
    let log = capture();
    let start = tokio::time::Instant::now();
    let (served, released) = handle.serve_checked(COLD_RELEASE_DEADLINE).await;
    served.expect("serves");
    released.expect("nothing to release");
    assert_eq!(start.elapsed(), std::time::Duration::ZERO);
    assert!(log.text().contains(STOP_LINE), "{}", log.text());
    assert!(!log.text().contains("still open"), "{}", log.text());
}
