//! The write tier's flush queue and its footer check (`docs/design/write-path.md`,
//! sections 6.3 and 6.4): which sealed objects a round takes, and when a key's
//! well-formed footer counts as this object's.
//!
//! Every test runs the same [`Farm`] as `write_tier.rs`. Each test's comment names the
//! defect it is there to catch.

mod common;
#[allow(dead_code)]
#[path = "write_tier/support.rs"]
mod support;

use std::time::{Duration, Instant};

use bytes::Bytes;
use common::faulty::PutFault;
use common::pseudo_random;
use kbf_meta::{Applied, Command, ObjectId, ServerId, StagedPut, StagedState, TierId};
use kbf_objstore::{Capabilities, ObjectStore};
use kbf_segments::IndexEntry;
use kbf_segments::layout::encode_footer;
use kbf_storage::{CacheError, FLUSH_BACKOFF_FIRST, FlushRound};
use support::{CONDITIONAL, Farm, SEGMENT, TestBlob, blobs, key, later, start, verified};

const GIB: u64 = 1 << 30;

/// The committed length of staged `object`.
fn committed_len(farm: &Farm, object: ObjectId) -> u64 {
    farm.log
        .state(move |s| {
            let mut staged = s.staged_objects();
            staged
                .find(|(o, _)| *o == object)
                .map(|(_, o)| o.committed_len())
        })
        .expect("staged")
}

/// A well-formed segment of `len` record bytes (`len >= 8192`) holding two blobs
/// other than any test's: its footer has the same length as one over two blobs, so
/// it ends exactly where a two-blob staged object's would.
fn foreign_segment(len: u64) -> Bytes {
    let n = usize::try_from(len).expect("len");
    let mut body = pseudo_random(n, 7_777);
    let mut entries = Vec::new();
    for (i, blob) in blobs(500, 2, 4096).iter().enumerate() {
        let at = i * 4096;
        body[at..at + 4096].copy_from_slice(&blob.bytes);
        entries.push(IndexEntry {
            digest: blob.digest,
            offset: at as u64,
            crc32c: kbf_segments::crc32c(&blob.bytes),
        });
    }
    entries.sort_by_key(|e| e.digest);
    body.extend_from_slice(&encode_footer(len, &entries));
    Bytes::from(body)
}

/// Catches: a key holding another, well-formed segment of the same total length taken
/// as this object's flush because some footer parsed there. `ObjectFlushed` would
/// commit, the block become an evictable copy, and once it is evicted every read would
/// go to the store, which holds the other object's bytes: acknowledged blobs lost.
#[tokio::test]
async fn a_key_holding_another_well_formed_segment_of_equal_length_is_a_conflict() {
    let farm = Farm::new(CONDITIONAL);
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let object = farm.log.object_of(&blobs[0].digest);
    let key = key(&cache, object);
    let other = foreign_segment(committed_len(&farm, object));
    farm.store.put_new(&key, other, None).await.expect("put");
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!((round.conflicts, round.flushed), (vec![object], vec![]));
    assert_eq!(farm.log.staged_state(object), Some(StagedState::Conflict));
    assert_eq!(farm.log.applied("object_flushed"), 0);
    assert!(farm.has_block(object) && !farm.has_copy(object));
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
}

/// Catches: our own PUT read back with another (well-formed) footer taken as a flush:
/// the store kept other bytes under the key, so the flush must not commit. The next
/// attempt meets those bytes as `AlreadyExists` and calls the conflict.
#[tokio::test]
async fn an_own_put_read_back_with_another_footer_is_not_flushed() {
    let farm = Farm::new(CONDITIONAL);
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let object = farm.log.object_of(&blobs[0].digest);
    let other = foreign_segment(committed_len(&farm, object));
    farm.store.on_put(PutFault::Replace(other));
    let t0 = later();
    let round = cache.flush_tier(t0).await.expect("round");
    assert_eq!(round.flushed, []);
    assert_eq!(round.retrying, [(object, FLUSH_BACKOFF_FIRST)]);
    assert_eq!(farm.log.staged_state(object), Some(StagedState::Sealed));
    assert_eq!(farm.log.applied("object_flushed"), 0);
    assert!(farm.has_block(object) && !farm.has_copy(object));
    let round = cache
        .flush_tier(t0 + FLUSH_BACKOFF_FIRST)
        .await
        .expect("round");
    assert_eq!((round.conflicts, round.flushed), (vec![object], vec![]));
}

/// Catches: a restart that does not count a conflict object as staged and sweeps its
/// block as an orphan. That block is the object's only good copy (the key holds
/// another object), so its blobs would be lost.
#[tokio::test]
async fn a_conflict_objects_block_survives_a_restart_and_is_read_locally() {
    let farm = Farm::new(CONDITIONAL);
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let object = farm.log.object_of(&blobs[0].digest);
    let key = key(&cache, object);
    let other = Bytes::from_static(b"some other object under this key");
    farm.store.put_new(&key, other, None).await.expect("put");
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.conflicts, [object]);
    drop(cache);

    let fs = farm.fs.restart();
    let (cache, startup) = start(&fs, &farm.log, &farm.store, GIB, None)
        .await
        .expect("restart");
    assert_eq!(startup.recovery.removed, Vec::<String>::new());
    assert_eq!(startup.recovery.lost, []);
    assert!(fs.contents(&kbf_tier::block_name(object)).is_some());
    assert_eq!(farm.log.staged_state(object), Some(StagedState::Conflict));
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    assert_eq!(farm.log.unreachable(object), None);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round, FlushRound::default(), "a conflict is not retried");
}

/// Catches: a round that takes its [`kbf_storage::FLUSHES_PER_ROUND`] oldest sealed
/// objects before it drops those waiting out a backoff. Four old objects that keep
/// failing would then starve every newer one.
#[tokio::test]
async fn objects_waiting_out_a_backoff_do_not_starve_newer_ones() {
    assert_eq!(kbf_storage::FLUSHES_PER_ROUND, 4);
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let len = usize::try_from(SEGMENT).expect("size") + 1;
    let mut objects = Vec::new();
    for seed in 1..=5 {
        let big = TestBlob::new(seed, len);
        cache
            .store_blobs(vec![big.verified()])
            .await
            .expect("stored");
        objects.push(farm.log.object_of(&big.digest));
    }
    objects.sort();
    for object in &objects {
        assert_eq!(farm.log.staged_state(*object), Some(StagedState::Sealed));
    }
    for _ in 0..4 {
        farm.store.on_put(PutFault::Fail);
    }
    let t0 = Instant::now();
    let round = cache.flush_tier(t0).await.expect("round");
    let failed: Vec<_> = objects[..4]
        .iter()
        .map(|o| (*o, FLUSH_BACKOFF_FIRST))
        .collect();
    assert_eq!((round.retrying, round.flushed), (failed, vec![]));
    let round = cache
        .flush_tier(t0 + Duration::from_millis(1))
        .await
        .expect("round");
    assert_eq!((round.flushed, round.retrying), (vec![objects[4]], vec![]));
}

/// Catches: a flush queue that takes sealed objects staged on another server's tier.
/// This tier cannot read them, so each round would retry them forever, and with four
/// of them it would flush nothing of its own.
#[tokio::test]
async fn objects_sealed_on_another_servers_tier_are_not_flushed_here() {
    let farm = Farm::new(Capabilities::default());
    let cache = farm.start(GIB).await;
    let theirs = TierId::new(999);
    let added = farm.log.commit(Command::AddTier {
        server: ServerId::new(2),
        tier: theirs,
    });
    assert_eq!(added, Applied::Tier(Ok(())));
    let Applied::Epoch(epoch) = farm.log.commit(Command::AllocEpoch) else {
        panic!("an epoch");
    };
    let b = TestBlob::new(1, 4096);
    let object = ObjectId::new(epoch, 1);
    let put = StagedPut {
        object,
        end: 4096,
        seal: true,
        blobs: vec![(b.digest, 0)],
    };
    let staged = farm.log.commit(Command::PutStaged {
        tier: theirs,
        objects: vec![put],
    });
    assert!(matches!(staged, Applied::Staged(Ok(_))), "{staged:?}");
    assert_eq!(farm.log.staged_state(object), Some(StagedState::Sealed));
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round, FlushRound::default());
    assert_eq!(farm.store.put_keys(), Vec::<kbf_objstore::ObjectKey>::new());
    assert_eq!(farm.log.unreachable(object), None);
}

/// Catches: a `PutStaged` that applied but whose answer was lost (a Raft log that
/// stopped after the entry was durable) leaving the tier's committed length below the
/// index's. The blob is indexed as staged here, but the tier would refuse to read it
/// (`NotCommitted`), and the block, whose only range counted as failed, would be removed
/// at its seal instead of flushed. Also catches the opposite: a range the log refused,
/// in the same block, counted as committed (its bytes would count as unflushed).
#[tokio::test]
async fn a_put_staged_that_applied_but_lost_its_answer_is_read_and_flushed() {
    let farm = Farm::new(CONDITIONAL);
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    let (a, b) = (&blobs[0], &blobs[1]);
    farm.log.lose_answer("put_staged", 1);
    let lost = cache.store_blobs(vec![a.verified()]).await;
    assert!(matches!(lost, Err(CacheError::Meta(_))), "{lost:?}");
    let object = farm.log.object_of(&a.digest);
    assert_eq!(cache.read_blob(&a.digest).await.expect("read"), a.bytes);

    farm.log.refuse("put_staged", 1);
    let refused = cache.store_blobs(vec![b.verified()]).await;
    assert!(matches!(refused, Err(CacheError::Meta(_))), "{refused:?}");
    assert_eq!(farm.log.object_of(&a.digest), object);
    let tier = cache.write_tier().expect("tier");
    assert_eq!(
        tier.unflushed(Instant::now()).bytes,
        committed_len(&farm, object)
    );

    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!((round.sealed, round.flushed), (vec![object], vec![object]));
    assert_eq!(farm.log.located(&a.digest).1, None, "flushed");
    assert_eq!(cache.read_blob(&a.digest).await.expect("read"), a.bytes);
}

/// Catches: a `PutStaged` whose answer was lost, and whose committed state then could
/// not be read, counted as committed (taking the failed read for "applied"). Nothing
/// says the commit applied, so the range must count as failed: none of its bytes
/// count as unflushed, and its block, whose only range failed, is not flushed. The
/// commit did apply here, and the next start recovers it from the committed state.
#[tokio::test]
async fn a_lost_put_staged_answer_whose_state_read_fails_counts_the_range_as_failed() {
    let farm = Farm::new(CONDITIONAL);
    let cache = farm.start(GIB).await;
    let a = TestBlob::new(1, 4096);
    farm.log.lose_answer_and_next_read("put_staged");
    let lost = cache.store_blobs(vec![a.verified()]).await;
    assert!(matches!(lost, Err(CacheError::Meta(_))), "{lost:?}");
    assert_eq!(farm.log.failed_reads(), 1, "the read after the lost answer");
    let object = farm.log.object_of(&a.digest);
    assert!(committed_len(&farm, object) > 0, "the commit applied");
    let tier = cache.write_tier().expect("tier");
    assert_eq!(tier.unflushed(Instant::now()).bytes, 0, "counted as failed");

    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, Vec::<ObjectId>::new(), "{round:?}");
    assert_eq!(farm.store.put_keys(), Vec::<kbf_objstore::ObjectKey>::new());
}
