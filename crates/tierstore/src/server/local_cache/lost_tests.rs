//! A fill that is running when the cache loses the lock of its directory.
//!
//! The lock file is replaced and another cache takes the new one: from then on the directory is
//! the other cache's, and the first must not write to it, or delete what it wrote.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use kbf_testkit::dir::TestDir;

use super::fill::{Published, publish};
use super::hold::Trace;
use super::layout::{data_path, meta_path, name_of, object_dir, stage_dir};
use super::metrics::read;
use super::testing::{Rig, pattern, wait_until};
use super::{Mode, SystemDisk, Tick};
use crate::Backend;

/// Asserts that a pass that stamps the times of the objects used stops at the file after the one
/// at which the cache lost its lock (the test makes it so in the first stamp): the stamps that
/// are left are not written into a directory that another cache holds. It would catch a pass that
/// asks about the lock once, before the first file.
#[tokio::test]
async fn a_pass_that_straddles_the_loss_of_the_lock_stamps_no_more_files() {
    let rig = Rig::new("tscache-lost-touch");
    let keys = ["cas/b/t0", "cas/b/t1", "cas/b/t2"];
    for key in keys {
        rig.put(key, &pattern(30)).await;
        let _ = rig.cache.get(key, None).await.unwrap();
    }
    let first = rig.locked();
    for key in keys {
        let _ = first.get(key, None).await.unwrap();
    }
    let weak = Arc::downgrade(&first.inner);
    let lock = rig.config.path.join(".lock");
    *rig.disk.touch_hook.lock().unwrap() = Some(Arc::new(move || {
        std::fs::remove_file(&lock).unwrap();
        let inner = weak.upgrade().unwrap();
        assert_eq!(inner.tick().unwrap(), Tick::Lost);
    }));
    first.inner.flush_touches();
    assert_eq!(rig.disk.touched.lock().unwrap().len(), 1);
    *rig.disk.touch_hook.lock().unwrap() = None;
}

/// Asserts that a publish that finds the lock lost makes nothing: asked first, it makes not even
/// the directory of the object; asked again before the rename, it has written the sidecar to the
/// stage directory and puts nothing in the object directory. It would catch a publish that goes
/// on into a directory the cache lost.
#[test]
fn a_publish_that_finds_the_lock_lost_puts_nothing_in_the_directory() {
    let dir = TestDir::under(std::env::temp_dir(), "tscache-lost-publish");
    let root = dir.path().join("cache");
    std::fs::create_dir_all(stage_dir(&root)).unwrap();
    let hex = name_of("cas/b/p").hex;
    let (from, temporary) = (stage_dir(&root).join("data"), stage_dir(&root).join("meta"));
    std::fs::write(&from, b"bytes").unwrap();
    for lost_at in [1, 2] {
        let asked = Cell::new(0);
        let owns = || {
            asked.set(asked.get() + 1);
            asked.get() < lost_at
        };
        let staged = (from.as_path(), temporary.as_path());
        let published = publish(&SystemDisk, &root, &hex, staged, b"{}", &owns).unwrap();
        assert_eq!(published, Published::Lost);
        assert_eq!(asked.get(), lost_at);
        assert!(!data_path(&root, &hex).exists());
        assert!(!meta_path(&root, &hex).exists());
        // Only the second look comes after the directory of the object was made.
        assert_eq!(object_dir(&root, &hex).exists(), lost_at == 2);
    }
}

/// Asserts that a fill that finds the lock lost after the upstream answered its HEAD, and before
/// it makes its stage file, makes none: no stage file was numbered, nothing is published, the
/// fill is counted as skipped, and the reader is answered from the upstream. It would catch a
/// fill that makes its stage file in a directory the cache lost.
#[tokio::test]
async fn a_fill_that_finds_the_lock_lost_before_its_stage_file_makes_none() {
    let rig = Rig::new("tscache-lost-before-stage");
    rig.put("cas/b/obj", &pattern(30)).await;
    let first = rig.locked();
    let gate = rig.tamper.hold_heads();
    let cache = first.clone();
    let filling = tokio::spawn(async move { cache.get("cas/b/obj", None).await });
    wait_until("the HEAD of the fill was held", || {
        rig.tamper.heading() >= 1
    })
    .await;
    std::fs::remove_file(rig.config.path.join(".lock")).unwrap();
    assert_eq!(first.tick().unwrap(), Tick::Lost);
    gate.add_permits(1000);
    assert_eq!(filling.await.unwrap().unwrap(), pattern(30));
    assert_eq!(first.inner.staged.load(Ordering::Relaxed), 0);
    assert!(!rig.has_file("cas/b/obj"));
    assert_eq!(read(&first.counters().fills_skipped), 1);
}

/// Asserts that a fill that finds the object linked already (the holder made it after the scan of
/// the cache) and then finds the lock lost leaves the object and its sidecar in the directory: it
/// linked nothing, so it has nothing to remove. It would catch a fill that removes what another
/// cache made.
#[tokio::test]
async fn a_fill_that_found_the_object_there_and_lost_the_lock_leaves_it() {
    let rig = Rig::new("tscache-lost-existed");
    rig.put("cas/b/obj", &pattern(30)).await;
    let first = rig.locked();
    let _ = rig.cache.get("cas/b/obj", None).await.unwrap();
    assert!(rig.has_file("cas/b/obj"));
    let weak = Arc::downgrade(&first.inner);
    let lock = rig.config.path.join(".lock");
    first.inner.set_trace(Trace {
        after_publish: Some(Arc::new(move || {
            std::fs::remove_file(&lock).unwrap();
            let inner = weak.upgrade().unwrap();
            assert_eq!(inner.tick().unwrap(), Tick::Lost);
        })),
        ..Trace::default()
    });
    assert_eq!(first.get("cas/b/obj", None).await.unwrap(), pattern(30));
    assert!(rig.has_file("cas/b/obj"));
    assert!(meta_path(&rig.config.path, &name_of("cas/b/obj").hex).is_file());
    assert_eq!(first.held(), (0, 0));
}

/// Asserts that a fill whose object was published and which then finds that the cache lost its
/// lock (the test makes it so between the publish and the fill's look at the lock) removes the
/// data file and the sidecar it made, leaves nothing in the index, and counts no fill; the reader
/// is answered from the upstream. The names are this fill's alone, so the removal is safe, and an
/// object that stayed would be one that the new owner never indexes and so never evicts. It
/// would catch a fill that walks away from an object it linked into a directory it no longer
/// owns.
#[tokio::test]
async fn an_object_published_just_before_the_lock_was_lost_is_removed() {
    let rig = Rig::new("tscache-lost-published");
    rig.put("cas/b/obj", &pattern(30)).await;
    let first = rig.locked();
    let weak = Arc::downgrade(&first.inner);
    let lock = rig.config.path.join(".lock");
    first.inner.set_trace(Trace {
        after_publish: Some(Arc::new(move || {
            std::fs::remove_file(&lock).unwrap();
            let inner = weak.upgrade().unwrap();
            assert_eq!(inner.tick().unwrap(), Tick::Lost);
        })),
        ..Trace::default()
    });
    assert_eq!(first.get("cas/b/obj", None).await.unwrap(), pattern(30));
    assert!(!rig.has_file("cas/b/obj"));
    assert!(!meta_path(&rig.config.path, &super::layout::name_of("cas/b/obj").hex).exists());
    assert_eq!(first.held(), (0, 0));
    assert_eq!(read(&first.counters().fills), 0);
    assert_eq!(first.mode(), Mode::ReadOnly);
}

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
    wait_until("the first fill made its stage file", || {
        read(&first.counters().staging_files) >= 1
    })
    .await;

    // The lock file is replaced and another cache makes the new one and holds it.
    std::fs::remove_file(rig.config.path.join(".lock")).unwrap();
    let second = rig.locked();
    assert_eq!(second.mode(), Mode::ReadWrite);
    let cache = second.clone();
    let new = tokio::spawn(async move { cache.get("cas/b/new", None).await });
    wait_until("the second fill made its stage file", || {
        read(&second.counters().staging_files) >= 1
    })
    .await;
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
