//! A delete or a replace sent through a read-only cache: the files of the holder stay where they
//! are while it is read-only, and must not be served once it takes the lock.
//!
//! `Rig::cache` stands for the process that holds the directory and filled it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use super::hold::Trace;
use super::layout::{data_path, name_of};
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

/// Asserts that a delete that is caught between its two steps by a take that runs to the end
/// leaves no entry in the index and no file. The deleter is stopped by the test between asking
/// whether the files may be removed and removing the index entry (`drop_entry`, the line after
/// `may_delete`), the whole take runs and the cache becomes read-write, and then the deleter
/// goes on. In the order that removes the index entry first and asks second, the stop falls
/// between the two the other way round: the entry is removed from the index of the read-only
/// cache, the take then builds a new index that has the entry (nothing was noted yet), and the
/// deleter, now allowed, removes the files and leaves that entry, which HEAD answers for an
/// object that is deleted. In this order the deleter was told the cache was read-only, so it
/// noted the name, and the take removed the files and left the entry out.
#[tokio::test]
async fn a_delete_caught_between_its_steps_by_the_take_leaves_no_index_entry() {
    let rig = Rig::writing("tscache-forget-during");
    rig.put("cas/b/obj", &pattern(30)).await;
    let _ = rig.cache.get("cas/b/obj", None).await.unwrap();
    let holder = rig.locked();
    let reader = rig.locked();
    assert_eq!(reader.held(), (30, 1));
    drop(holder);

    let (resume_tx, resume_rx) = mpsc::channel::<()>();
    let (arrived_tx, arrived_rx) = mpsc::channel::<()>();
    let (resume_rx, arrived_tx) = (Mutex::new(resume_rx), Mutex::new(arrived_tx));
    reader.inner.set_trace(Trace {
        between_delete_steps: Some(Arc::new(move || {
            arrived_tx.lock().unwrap().send(()).unwrap();
            resume_rx.lock().unwrap().recv().unwrap();
        })),
        ..Trace::default()
    });
    let hex = name_of("cas/b/obj").hex;
    let deleting = reader.clone();
    let delete = std::thread::spawn(move || deleting.inner.drop_entry(&hex));
    arrived_rx.recv().unwrap();
    assert_eq!(reader.tick().unwrap(), Tick::Took);
    resume_tx.send(()).unwrap();
    delete.join().unwrap();
    assert_eq!(reader.held(), (0, 0));
    assert!(!rig.has_file("cas/b/obj"));
}

/// Asserts that deletes that keep arriving while the take runs do not keep it waiting: a delete
/// is noted in each of the rounds it makes, and after the eighth the notes count as an overflow,
/// so the ninth round removes every object that is left and the take ends with an empty cache
/// and no object file, in nine rounds, though twelve objects were each noted in a round of their
/// own. It would catch a take that goes on for as long as deletes come.
#[tokio::test]
async fn deletes_that_keep_arriving_during_the_take_end_it_with_an_empty_cache() {
    let rig = Rig::writing("tscache-forget-rounds");
    let keys: Vec<String> = (0..12).map(|n| format!("cas/b/o{n}")).collect();
    for key in &keys {
        rig.put(key, &pattern(30)).await;
        let _ = rig.cache.get(key, None).await.unwrap();
    }
    let holder = rig.locked();
    let reader = rig.locked();
    assert_eq!(reader.held(), (360, 12));
    drop(holder);

    let rounds = Arc::new(AtomicUsize::new(0));
    let (counting, weak) = (Arc::clone(&rounds), Arc::downgrade(&reader.inner));
    let names: Vec<String> = keys.iter().map(|key| name_of(key).hex).collect();
    reader.inner.set_trace(Trace {
        promote_round: Some(Arc::new(move || {
            let round = counting.fetch_add(1, Ordering::SeqCst);
            if let Some(hex) = names.get(round) {
                assert!(!weak.upgrade().unwrap().may_delete(hex));
            }
        })),
        ..Trace::default()
    });
    assert_eq!(reader.tick().unwrap(), Tick::Took);
    assert_eq!(rounds.load(Ordering::SeqCst), 9);
    assert_eq!(reader.held(), (0, 0));
    for key in &keys {
        assert!(!rig.has_file(key), "{key}");
    }
}

/// Makes a take whose removal of the files of `gone` fails once (the data file of `gone` is
/// replaced by a directory between the choice of the files and their removal, and the data file
/// of `other` is removed, so that one removal finds a file already gone), with at most
/// `limit` names noted, and asserts that the tick is an error and the cache stays read-only; that
/// after the directory is put right the next tick takes the lock; and that no object file of
/// either key is left, so the names that were noted when the first try failed were not lost.
async fn a_take_whose_removal_fails(name: &str, limit: usize) {
    let rig = Rig::writing(name);
    let (gone, other) = ("cas/b/gone", "cas/b/other");
    for key in [gone, other] {
        rig.put(key, &pattern(30)).await;
        let _ = rig.cache.get(key, None).await.unwrap();
    }
    let holder = rig.locked();
    let reader = rig.locked_with(|config| config.forget_limit = limit);
    reader.delete(gone).await.unwrap();
    reader.delete(other).await.unwrap();
    drop(holder);

    let root = rig.config.path.clone();
    let (gone_file, other_file) = (
        data_path(&root, &name_of(gone).hex),
        data_path(&root, &name_of(other).hex),
    );
    let done = AtomicBool::new(false);
    let (swapped, removed) = (gone_file.clone(), other_file.clone());
    reader.inner.set_trace(Trace {
        promote_round: Some(Arc::new(move || {
            if !done.swap(true, Ordering::SeqCst) {
                std::fs::remove_file(&swapped).unwrap();
                std::fs::create_dir(&swapped).unwrap();
                std::fs::remove_file(&removed).unwrap();
            }
        })),
        ..Trace::default()
    });
    assert!(reader.tick().is_err());
    assert_eq!(reader.mode(), Mode::ReadOnly);

    std::fs::remove_dir(&gone_file).unwrap();
    std::fs::write(&gone_file, pattern(30)).unwrap();
    assert_eq!(reader.tick().unwrap(), Tick::Took);
    assert_eq!(reader.held(), (0, 0));
    assert!(!rig.has_file(gone));
    assert!(!rig.has_file(other));
}

/// Asserts the take whose removal fails with the names it noted listed (two names, room for
/// both).
#[tokio::test]
async fn a_take_whose_removal_fails_tries_again_with_the_names_it_noted() {
    a_take_whose_removal_fails("tscache-forget-failed", 8).await;
}

/// Asserts the take whose removal fails when it noted too many names to list (room for one of
/// two): the overflow is noted again as it was, and the next try empties the cache.
#[tokio::test]
async fn a_take_whose_removal_fails_tries_again_with_an_overflow() {
    a_take_whose_removal_fails("tscache-forget-failed-overflow", 1).await;
}
