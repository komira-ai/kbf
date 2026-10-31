//! A delete or a replace sent through a read-only cache: the files of the holder stay where they
//! are while it is read-only, and must not be served once it takes the lock.
//!
//! `Rig::cache` stands for the process that holds the directory and filled it.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use super::hold::Trace;
use super::layout::name_of;
use super::metrics::read;
use super::testing::{Rig, pattern, tree};
use super::{Mode, Tick};
use crate::{Backend, Error};

/// Asserts that a key a read-only cache deleted through the upstream is read from the upstream
/// while the cache is read-only (not found, though the holder's file is on the disk), that the
/// directory is not touched meanwhile, and that after the holder is gone and the cache has taken
/// the lock a read of it is not found, the holder's files are gone, and the other object is
/// still a hit. It would catch a cache that rescans the disk when it takes the lock and serves
/// the deleted object from the files the holder left, for good.
#[tokio::test]
async fn a_key_deleted_through_a_read_only_cache_is_not_served_after_it_takes_the_lock() {
    let rig = Rig::writing("tscache-forget-delete");
    for key in ["cas/b/gone", "cas/b/kept"] {
        rig.put(key, &pattern(30)).await;
        let _ = rig.cache.get(key, None).await.unwrap();
    }
    let holder = rig.locked();
    let reader = rig.locked();
    let before = tree(&rig.config.path);
    reader.delete("cas/b/gone").await.unwrap();
    assert_eq!(tree(&rig.config.path), before);
    // Still read-only: the read goes to the upstream, which no longer has it.
    let found = reader.get("cas/b/gone", None).await;
    assert!(matches!(found, Err(Error::NotFound(_))), "{found:?}");
    assert!(rig.has_file("cas/b/gone"));
    assert_eq!(reader.mode(), Mode::ReadOnly);

    drop(holder);
    assert_eq!(reader.tick().unwrap(), Tick::Took);
    let found = reader.get("cas/b/gone", None).await;
    assert!(matches!(found, Err(Error::NotFound(_))), "{found:?}");
    assert!(!rig.has_file("cas/b/gone"));
    assert_eq!(reader.held(), (30, 1));
    assert_eq!(reader.get("cas/b/kept", None).await.unwrap(), pattern(30));
    assert_eq!(read(&reader.counters().hits), 1);
}

/// Asserts that a key replaced through a read-only cache (a delete and then a create-only put
/// of other bytes: a put of a key that exists is refused, so there is no other way to replace
/// one) is read from the upstream with the new bytes, both while read-only and after the lock is
/// taken. It would catch a cache that serves the holder's old bytes after it takes the lock.
#[tokio::test]
async fn a_key_replaced_through_a_read_only_cache_is_served_new_after_it_takes_the_lock() {
    let rig = Rig::writing("tscache-forget-replace");
    rig.put("cas/b/swap", &pattern(30)).await;
    let _ = rig.cache.get("cas/b/swap", None).await.unwrap();
    let holder = rig.locked();
    let reader = rig.locked();
    let refused = reader.put("cas/b/swap", Bytes::from_static(b"newer")).await;
    assert!(
        matches!(refused, Err(Error::AlreadyExists(_))),
        "{refused:?}"
    );
    reader.delete("cas/b/swap").await.unwrap();
    reader
        .put("cas/b/swap", Bytes::from_static(b"newer"))
        .await
        .unwrap();
    assert_eq!(reader.get("cas/b/swap", None).await.unwrap(), "newer");
    assert_eq!(rig.tamper.get("cas/b/swap", None).await.unwrap(), "newer");

    drop(holder);
    assert_eq!(reader.tick().unwrap(), Tick::Took);
    assert_eq!(reader.get("cas/b/swap", None).await.unwrap(), "newer");
    assert_eq!(read(&reader.counters().hits), 0);
}

/// Asserts that when a read-only cache has deleted more keys than it remembers, it takes the
/// lock with an empty index and no object files (every object is read from the upstream again),
/// and that a cache that stays within its limit keeps the other objects. It would catch a
/// cache that forgets a key it was told to drop because its list was full.
#[tokio::test]
async fn more_deletes_than_the_cache_remembers_empty_it_when_it_takes_the_lock() {
    let rig = Rig::writing("tscache-forget-overflow");
    let keys = ["cas/b/a", "cas/b/b", "cas/b/c", "cas/b/kept"];
    for key in keys {
        rig.put(key, &pattern(30)).await;
        let _ = rig.cache.get(key, None).await.unwrap();
    }
    let holder = rig.locked();
    let reader = rig.locked_with(|config| config.forget_limit = 2);
    for key in &keys[..3] {
        reader.delete(key).await.unwrap();
    }
    assert_eq!(reader.get("cas/b/kept", None).await.unwrap(), pattern(30));
    drop(holder);
    assert_eq!(reader.tick().unwrap(), Tick::Took);
    assert_eq!(reader.held(), (0, 0));
    for key in keys {
        assert!(!rig.has_file(key), "{key}");
    }
    assert_eq!(reader.get("cas/b/kept", None).await.unwrap(), pattern(30));
    assert_eq!(read(&reader.counters().fills), 1);
}

/// Asserts that a delete that reaches a read-only cache while it is taking the lock leaves no
/// entry in the index: the take has decided which files to trust, the delete arrives and is
/// ordered after the decision (the take is held, by the test, between the two), and when both are
/// done the index has no entry and the files are gone. It would catch a delete that removes the
/// index entry first and then waits for the take, which puts the entry back for an object that
/// is deleted, to be answered by HEAD for good.
#[tokio::test]
async fn a_delete_that_arrives_during_the_take_leaves_no_index_entry() {
    let rig = Rig::writing("tscache-forget-during");
    rig.put("cas/b/obj", &pattern(30)).await;
    let _ = rig.cache.get("cas/b/obj", None).await.unwrap();
    let holder = rig.locked();
    let reader = rig.locked();
    assert_eq!(reader.held(), (30, 1));
    drop(holder);

    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (resume_tx, resume_rx) = mpsc::channel::<()>();
    let (arrived_tx, arrived_rx) = mpsc::channel::<()>();
    let (started_tx, resume_rx) = (Mutex::new(started_tx), Mutex::new(resume_rx));
    let arrived_tx = Mutex::new(arrived_tx);
    reader.inner.set_trace(Trace {
        promote: Some(Arc::new(move || {
            started_tx.lock().unwrap().send(()).unwrap();
            resume_rx.lock().unwrap().recv().unwrap();
        })),
        before_delete: Some(Arc::new(move || {
            arrived_tx.lock().unwrap().send(()).unwrap();
        })),
    });
    let taking = reader.clone();
    let take = std::thread::spawn(move || taking.tick().unwrap());
    started_rx.recv().unwrap();
    let hex = name_of("cas/b/obj").hex;
    let deleting = reader.clone();
    let delete = std::thread::spawn(move || deleting.inner.drop_entry(&hex));
    arrived_rx.recv().unwrap();
    resume_tx.send(()).unwrap();
    assert_eq!(take.join().unwrap(), Tick::Took);
    delete.join().unwrap();
    assert_eq!(reader.held(), (0, 0));
    assert!(!rig.has_file("cas/b/obj"));
}
