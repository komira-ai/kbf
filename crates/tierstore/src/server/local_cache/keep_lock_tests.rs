//! The copy a create-only put leaves in the cache, and the lock of the cache directory: a
//! read-only cache keeps nothing, and a keep that is running when the cache loses the lock stops
//! as a fill does.

use std::sync::Arc;

use bytes::Bytes;
use futures::stream;
use futures::{FutureExt as _, StreamExt as _};

use super::layout::{meta_path, name_of};
use super::metrics::{SkippedKeep, read};
use super::testing::{Rig, never_expires, tree};
use super::{Mode, Tick};
use crate::{Backend, Error, PartBody};

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
