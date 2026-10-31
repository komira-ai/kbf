//! The copy a create-only put leaves in the cache, and the lock of the cache directory: a
//! read-only cache keeps nothing, and a keep that is running when the cache loses the lock stops
//! as a fill does.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::stream;
use futures::{FutureExt as _, StreamExt as _};

use super::layout::{Sidecar, data_path, meta_path, name_of, object_dir};
use super::metrics::{SkippedKeep, read};
use super::testing::{BlockingGate, Rig, digest, never_expires, tree, wait_until};
use super::{LocalCache, Mode, Tick};
use crate::{Backend, Error, PartBody};

/// A gate that holds threads of the disk, opened when this is dropped.
struct OpenOnDrop(Arc<BlockingGate>);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// A rig of a cache that writes through, its keeps never expiring.
fn rig(name: &str) -> Rig {
    Rig::with(name, |config| {
        config.write_through = true;
        never_expires(config);
    })
}

/// Asserts that a put sent to a read-only cache is answered and is at the upstream, and that
/// the cache keeps nothing: no stage file is made, nothing is published or indexed, the listing
/// of the whole directory is the same after as before, and the put is counted as not kept
/// because the cache is read-only. It would catch a keep that writes into a directory another
/// process holds.
#[tokio::test]
async fn a_put_through_a_read_only_cache_is_answered_and_leaves_the_directory_alone() {
    let rig = rig("tscache-ro-keep");
    let holder = rig.locked();
    let reader = rig.locked();
    let before = tree(&rig.config.path);
    reader
        .put("cas/b/new", Bytes::from_static(b"new"))
        .await
        .unwrap();
    assert_eq!(rig.tamper.get("cas/b/new", None).await.unwrap(), "new");
    assert_eq!(tree(&rig.config.path), before);
    assert_eq!(reader.held(), (0, 0));
    assert_eq!(reader.counters().keeps_skipped(SkippedKeep::ReadOnly), 1);
    assert_eq!(read(&reader.counters().staging_files), 0);
    assert_eq!(reader.mode(), Mode::ReadOnly);
    assert_eq!(holder.mode(), Mode::ReadWrite);
}

/// Asserts that a keep whose object was published when the cache lost the lock (the test makes
/// it so between the publish and the index) removes the files it linked, indexes nothing, leaves
/// no stage file, answers the put, and is counted as not kept because the cache is read-only.
/// It would catch a keep that leaves an object in a directory the cache no longer owns, which
/// the new owner does not index or evict.
#[tokio::test]
async fn a_keep_published_just_before_the_lock_was_lost_is_removed() {
    let rig = rig("tscache-lost-keep-published");
    let first = rig.locked();
    let weak = Arc::downgrade(&first.inner);
    let lock = rig.config.path.join(".lock");
    *super::locked(&first.inner.before_insert) = Some(Box::new(move || {
        std::fs::remove_file(&lock).unwrap();
        let inner = weak.upgrade().unwrap();
        assert_eq!(inner.tick().unwrap(), Tick::Lost);
    }));
    first
        .put("cas/b/obj", Bytes::from_static(b"object"))
        .await
        .unwrap();
    *super::locked(&first.inner.before_insert) = None;
    assert_eq!(rig.tamper.get("cas/b/obj", None).await.unwrap(), "object");
    assert!(!rig.has_file("cas/b/obj"));
    assert!(!meta_path(&rig.config.path, &name_of("cas/b/obj").hex).exists());
    assert_eq!(first.held(), (0, 0));
    assert_eq!(first.counters().keeps_skipped(SkippedKeep::ReadOnly), 1);
    assert_eq!(rig.staged_files(), 0);
}

/// A put of `key` whose keep is abandoned at its deadline with its link held on a thread: the
/// rig (a cache that writes through, a wait of 5 s and a deadline of 60 s on the paused clock),
/// the cache, which holds the lock, and the gate that holds the link, opened when it is dropped.
/// The clock is moved by the test and by nothing else (the test never waits idle), so the wait of
/// the put and the deadline of the keep come when the test says.
async fn abandoned_keep(name: &str, key: &'static str) -> (Rig, LocalCache, OpenOnDrop) {
    let rig = Rig::with(name, |config| {
        config.write_through = true;
        config.keep_wait = Duration::from_secs(5);
        config.keep_deadline = Duration::from_secs(60);
    });
    let first = rig.locked();
    // Opened when the test ends however it ends: a thread held at the gate is waited for when
    // the runtime is dropped, so a failed assertion before the gate is opened would not end the
    // test at all.
    let gate = OpenOnDrop(rig.disk.hold_links());
    let body = Bytes::from_static(b"linking");
    let cache = first.clone();
    let putting = tokio::spawn(async move { cache.put(key, body).await });
    wait_until("the link began", || {
        !rig.disk.linked.lock().unwrap().is_empty()
    })
    .await;
    tokio::time::advance(Duration::from_secs(5)).await;
    wait_until("the put was answered", || putting.is_finished()).await;
    assert_eq!(putting.await.unwrap(), Ok(()));
    tokio::time::advance(Duration::from_secs(60)).await;
    wait_until("the keep was abandoned at its deadline", || {
        first.counters().keeps_skipped(SkippedKeep::Deadline) == 1
    })
    .await;
    (rig, first, gate)
}

/// Asserts that a keep abandoned at its deadline is undone when its link ends, while the cache
/// still owns the directory as it did when the keep began: the object it linked is removed, with
/// its sidecar, and nothing is indexed. It would catch an abandoned keep that leaves a linked
/// object that nothing indexes.
#[tokio::test(start_paused = true)]
async fn an_abandoned_keep_is_undone_while_the_cache_owns_the_directory() {
    let key = "cas/b/linking";
    let (rig, first, gate) = abandoned_keep("tscache-keep-deadline-undone", key).await;
    gate.0.open();
    wait_until("the cleanup of the keep ended", || {
        read(&first.counters().staging_files) == 0
    })
    .await;
    assert!(!rig.has_file(key));
    assert!(!meta_path(&rig.config.path, &name_of(key).hex).exists());
    assert_eq!(first.held(), (0, 0));
}

/// Asserts that a keep abandoned at its deadline whose link ends after the cache lost the lock
/// (and did not take it again: nothing emptied the stage directory, so the link is made) removes
/// the object it linked, with its sidecar, and nothing is indexed. It would catch a cleanup that
/// leaves in a directory the cache lost an object that the new owner does not index.
#[tokio::test(start_paused = true)]
async fn an_abandoned_keep_that_linked_after_the_lock_was_lost_removes_what_it_linked() {
    let key = "cas/b/linking";
    let (rig, first, gate) = abandoned_keep("tscache-keep-deadline-lost", key).await;
    std::fs::remove_file(rig.config.path.join(".lock")).unwrap();
    assert_eq!(first.tick().unwrap(), Tick::Lost);
    gate.0.open();
    wait_until("the cleanup of the keep ended", || {
        read(&first.counters().staging_files) == 0
    })
    .await;
    assert!(!rig.has_file(key));
    assert!(!meta_path(&rig.config.path, &name_of(key).hex).exists());
    assert_eq!(first.held(), (0, 0));
}

/// Asserts that a keep whose link finds the object there already (another cache made it, the
/// test says) after the cache lost the lock leaves it: the keep linked nothing, so it removes
/// nothing, is counted as read-only, and answers the put. It would catch a keep that removes the
/// files of a name because it lost the lock, whoever made them.
#[tokio::test]
async fn a_keep_that_found_the_object_there_and_lost_the_lock_leaves_it() {
    let rig = rig("tscache-keep-existed-lost");
    let first = rig.locked();
    let key = "cas/b/there";
    let gate = OpenOnDrop(rig.disk.hold_links());
    let cache = first.clone();
    let putting = tokio::spawn(async move { cache.put(key, Bytes::from_static(b"mine")).await });
    wait_until("the link began", || {
        !rig.disk.linked.lock().unwrap().is_empty()
    })
    .await;
    std::fs::remove_file(rig.config.path.join(".lock")).unwrap();
    assert_eq!(first.tick().unwrap(), Tick::Lost);
    let file = data_path(&rig.config.path, &name_of(key).hex);
    std::fs::write(&file, b"the other cache's").unwrap();
    gate.0.open();
    wait_until("the put was answered", || putting.is_finished()).await;
    assert_eq!(putting.await.unwrap(), Ok(()));
    assert_eq!(first.counters().keeps_skipped(SkippedKeep::ReadOnly), 1);
    assert_eq!(std::fs::read(&file).unwrap(), b"the other cache's");
    assert!(meta_path(&rig.config.path, &name_of(key).hex).is_file());
    assert_eq!(first.held(), (0, 0));
}

/// Asserts that a keep that is abandoned at its deadline while its link is held on a thread, in a
/// cache that then loses the lock and takes it again (the generation changed, the cache is
/// read-write), leaves alone the object and the entry that the new ownership has for the same
/// name: the keep linked nothing, so it has nothing of its own to undo. The link ends with an
/// error (the take emptied the stage directory). It would catch a cleanup that removes the
/// entry and the files of a name because the cache is read-write now, whoever made them.
#[tokio::test(start_paused = true)]
async fn an_abandoned_keep_leaves_the_object_of_the_new_ownership_alone() {
    let key = "cas/b/linking";
    let (rig, first, gate) = abandoned_keep("tscache-lost-keep-deadline", key).await;

    // The lock is lost and taken again: this is another ownership of the directory, and it has
    // an object of its own under the name.
    std::fs::remove_file(rig.config.path.join(".lock")).unwrap();
    assert_eq!(first.tick().unwrap(), Tick::Lost);
    assert_eq!(first.tick().unwrap(), Tick::Took);
    let hex = name_of(key).hex;
    let root = &rig.config.path;
    std::fs::create_dir_all(object_dir(root, &hex)).unwrap();
    std::fs::write(data_path(root, &hex), b"the new one").unwrap();
    let sidecar = Sidecar {
        key: key.to_owned(),
        size: 11,
        sha256: digest(b"the new one"),
        tag: "new".to_owned(),
        modified: 0,
    };
    std::fs::write(meta_path(root, &hex), serde_json::to_vec(&sidecar).unwrap()).unwrap();
    super::locked(&first.inner.index).insert(&hex, 11, "new".to_owned(), 0);

    gate.0.open();
    wait_until("the cleanup of the keep ended", || {
        read(&first.counters().staging_files) == 0
    })
    .await;
    assert_eq!(first.held(), (11, 1));
    assert_eq!(
        std::fs::read(data_path(root, &hex)).unwrap(),
        b"the new one"
    );
    assert!(meta_path(root, &hex).is_file());
}

/// Asserts that a keep whose body is still arriving when the cache loses the lock (the test
/// makes it so between two chunks) publishes nothing: no object file, no sidecar, nothing in the
/// index, no stage file left; the upstream has the whole object, the put is answered, and the
/// put is counted as not kept because the cache is read-only. It would catch a keep that goes
/// on writing its stage file and publishing after the lock is lost.
#[tokio::test]
async fn a_keep_in_flight_across_a_lost_lock_publishes_nothing() {
    let rig = rig("tscache-lost-keep-flight");
    let first = rig.locked();
    let lock = rig.config.path.join(".lock");
    let losing = first.clone();
    let body: PartBody<'static> = stream::unfold(0u8, move |step| {
        let (losing, lock) = (losing.clone(), lock.clone());
        async move {
            match step {
                0 => Some((Ok::<_, Error>(Bytes::from_static(b"aaa")), 1)),
                1 => {
                    std::fs::remove_file(&lock).unwrap();
                    assert_eq!(losing.tick().unwrap(), Tick::Lost);
                    Some((Ok(Bytes::from_static(b"bbb")), 2))
                }
                _ => None,
            }
        }
    })
    .fuse()
    .boxed();
    first
        .put_stream("cas/b/flight", Some(6), body)
        .map(|made| assert_eq!(made, Ok(())))
        .await;
    assert_eq!(
        rig.tamper.get("cas/b/flight", None).await.unwrap(),
        "aaabbb"
    );
    assert!(!rig.has_file("cas/b/flight"));
    assert!(!meta_path(&rig.config.path, &name_of("cas/b/flight").hex).exists());
    assert_eq!(first.held(), (0, 0));
    assert_eq!(first.counters().keeps_skipped(SkippedKeep::ReadOnly), 1);
    assert_eq!(rig.staged_files(), 0);
}
