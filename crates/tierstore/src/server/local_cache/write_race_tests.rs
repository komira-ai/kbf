//! A delete and a fill of one key at the same time: the fill must not put back what the delete
//! took out. The fills are driven by the gates of the upstream fixture, which hold a read after
//! it has the object (so the fill has every byte and has not yet kept any), and a delete before
//! it deletes; no test waits for a time.

use std::sync::atomic::Ordering;

use bytes::Bytes;

use super::testing::{Rig, digest, pattern};
use crate::{Backend, Error};

/// A key named by its content, so that a fill of it needs no second look at the upstream after
/// the last byte and the only thing that can stop it keeping the object is the delete.
fn content_key(bytes: &[u8]) -> String {
    format!("cas/l/{}", digest(bytes))
}

/// The most times `until` gives up the thread before it fails the test. A count and not a time:
/// a gate that never parks ends the test with a message, the same on a slow machine.
const SPINS: u32 = 1_000_000;

/// Waits, by giving up the thread, until `done` is true; fails the test, saying what `what` was
/// to have happened, if it has not after [`SPINS`] turns.
async fn until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..SPINS {
        if done() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("after {SPINS} turns of the scheduler, this never happened: {what}");
}

/// Asserts that a delete that comes while a fill holds every byte of the object, and has kept
/// none, leaves nothing in the cache: no entry, no data file, no file in the stage directory;
/// the reader that started the fill is told the object is gone; and the mark the delete left for
/// the fill goes with the fill, so that the same key written again is kept by the next fill.
/// Without the mark the fill keeps the object that was deleted, and reads serve it.
#[tokio::test]
async fn a_fill_that_a_delete_overtakes_does_not_keep_the_object() {
    let rig = Rig::writing("tscache-race-fill");
    let bytes = pattern(300);
    let key = content_key(&bytes);
    rig.put(&key, &bytes).await;
    let parked = rig.tamper.hold_key_after_read(&key);
    let (cache, name) = (rig.cache.clone(), key.clone());
    let reader = tokio::spawn(async move { cache.get(&name, None).await });
    until("the fill read the object and parked", || {
        rig.tamper.parked() >= 1
    })
    .await;
    assert_eq!(rig.cache.held(), (0, 0));

    assert_eq!(rig.cache.delete(&key).await, Ok(()));
    parked.add_permits(16);
    let read = reader.await.unwrap();
    assert!(matches!(read, Err(Error::NotFound(_))), "{read:?}");
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(&key));
    assert_eq!(rig.staged_files(), 0);
    assert!(super::locked(&rig.cache.inner.flights).is_empty());
    assert!(super::locked(&rig.cache.inner.invalidated).is_empty());
    assert!(rig.tamper.memory.head(&key).await.is_err());

    rig.put(&key, &bytes).await;
    assert_eq!(rig.get(&key).await.unwrap(), Bytes::from(bytes));
    assert_eq!(rig.cache.held(), (300, 1));
    assert!(rig.has_file(&key));
}

/// Asserts the other order: a read starts a fill after the cache has dropped its entry and
/// before the upstream has deleted the object, and takes every byte; the delete then ends. The
/// fill is told at the end of the delete and keeps nothing. A delete that only dropped the entry
/// before asking the upstream would let this fill put the object back for good.
#[tokio::test]
async fn a_fill_that_starts_during_a_delete_does_not_keep_the_object() {
    let rig = Rig::writing("tscache-race-delete");
    let bytes = pattern(300);
    let key = content_key(&bytes);
    rig.put(&key, &bytes).await;
    let held = rig.tamper.hold_deletes();
    let (cache, name) = (rig.cache.clone(), key.clone());
    let deleting = tokio::spawn(async move { cache.delete(&name).await });
    until("the delete reached the upstream", || {
        rig.tamper.deleting() >= 1
    })
    .await;

    let parked = rig.tamper.hold_key_after_read(&key);
    let (cache, name) = (rig.cache.clone(), key.clone());
    let reader = tokio::spawn(async move { cache.get(&name, None).await });
    until("the fill read the object and parked", || {
        rig.tamper.parked() >= 1
    })
    .await;

    held.add_permits(1);
    assert_eq!(deleting.await.unwrap(), Ok(()));
    parked.add_permits(16);
    let read = reader.await.unwrap();
    assert!(matches!(read, Err(Error::NotFound(_))), "{read:?}");
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(&key));
    assert_eq!(rig.staged_files(), 0);
    assert!(super::locked(&rig.cache.inner.invalidated).is_empty());
}

/// Asserts the same order when the upstream applies the delete and then answers with an error (a
/// timeout, or a 5xx after the commit): the delete is that error, and the fill that began before
/// it still keeps nothing, and the cache holds nothing of the object. A delete that dropped the
/// entry again only when the upstream answered well would let this fill put back an object that
/// the upstream no longer has.
#[tokio::test]
async fn a_fill_that_starts_during_a_delete_whose_answer_is_lost_does_not_keep_the_object() {
    let rig = Rig::writing("tscache-race-lost");
    let bytes = pattern(300);
    let key = content_key(&bytes);
    rig.put(&key, &bytes).await;
    let held = rig.tamper.hold_deletes();
    rig.tamper.lose_delete_answer.store(true, Ordering::SeqCst);
    let (cache, name) = (rig.cache.clone(), key.clone());
    let deleting = tokio::spawn(async move { cache.delete(&name).await });
    until("the delete reached the upstream", || {
        rig.tamper.deleting() >= 1
    })
    .await;

    let parked = rig.tamper.hold_key_after_read(&key);
    let (cache, name) = (rig.cache.clone(), key.clone());
    let reader = tokio::spawn(async move { cache.get(&name, None).await });
    until("the fill read the object and parked", || {
        rig.tamper.parked() >= 1
    })
    .await;

    held.add_permits(1);
    let deleted = deleting.await.unwrap();
    assert!(matches!(deleted, Err(Error::Unavailable(_))), "{deleted:?}");
    assert!(rig.tamper.memory.head(&key).await.is_err());
    parked.add_permits(16);
    let read = reader.await.unwrap();
    assert!(matches!(read, Err(Error::NotFound(_))), "{read:?}");
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file(&key));
    assert_eq!(rig.staged_files(), 0);
    assert!(super::locked(&rig.cache.inner.invalidated).is_empty());
}

/// Asserts that `until` ends a test whose event never comes, with a message that says what it
/// waited for, and does not wait for a time.
#[tokio::test]
#[should_panic(expected = "this never happened: the gate parked")]
async fn a_wait_for_an_event_that_never_comes_fails_with_what_it_waited_for() {
    until("the gate parked", || false).await;
}
