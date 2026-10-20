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
use kbf_front::{CacheError, FLUSH_BACKOFF_FIRST, FlushRound};
use kbf_meta::{Applied, Command, ObjectId, ServerId, StagedPut, StagedState, TierId};
use kbf_objstore::{ByteRange, Capabilities, ObjectStore};
use kbf_segments::IndexEntry;
use kbf_segments::layout::encode_footer;
use kbf_store::{Fault, Fs as _};
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
/// Also catches a conflict that leaves the object staged: its space is freed only by a
/// flush, so it would never be, and the blobs would be read from a block that cannot
/// be evicted.
#[tokio::test]
async fn a_key_holding_another_well_formed_segment_of_equal_length_is_staged_again() {
    let farm = Farm::new(CONDITIONAL);
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let object = farm.log.object_of(&blobs[0].digest);
    let key = key(&cache, object);
    let other = foreign_segment(committed_len(&farm, object));
    farm.store
        .put_new(&key, other.clone(), None)
        .await
        .expect("put");
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!((round.conflicts, round.flushed), (vec![object], vec![]));
    assert_eq!(farm.log.applied("flush_conflict"), 0);
    assert_eq!(farm.log.staged_state(object), None);
    assert_eq!(farm.log.unreachable(object), None, "marked nothing");
    assert_eq!(farm.log.losses(), (0, 0), "a move is not a loss");
    assert!(!farm.has_block(object), "the block is freed");
    let moved = farm.log.object_of(&blobs[0].digest);
    assert_ne!(moved, object);
    assert_eq!(farm.log.object_of(&blobs[1].digest), moved);
    assert_eq!(farm.log.staged_state(moved), Some(StagedState::Sealed));
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!((round.conflicts, round.flushed), (vec![], vec![moved]));
    assert_eq!(farm.log.staged_state(moved), None);
    assert!(farm.has_copy(moved) && !farm.has_block(moved));
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    assert_eq!(
        stored(&farm, &key, other.len()).await,
        other,
        "the other object is untouched"
    );
}

/// The first `len` bytes the store holds under `key`.
async fn stored(farm: &Farm, key: &kbf_objstore::ObjectKey, len: usize) -> Bytes {
    let range = ByteRange::new(0, len as u64).expect("range");
    farm.store.get_range(key, range).await.expect("get")
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
/// another object), so its blobs would be lost. And a conflict that an earlier build
/// committed (`FlushConflict`, which no build commits now) that stays staged for ever:
/// the first round after the restart stages its records again and frees its block.
#[tokio::test]
async fn a_conflict_an_earlier_build_left_is_staged_again_after_a_restart() {
    let farm = Farm::new(CONDITIONAL);
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let object = farm.log.object_of(&blobs[0].digest);
    let key = key(&cache, object);
    let other = Bytes::from_static(b"some other object under this key");
    farm.store.put_new(&key, other, None).await.expect("put");
    let t = later();
    for command in [
        Command::SealStaged { object },
        Command::FlushConflict { object },
    ] {
        assert!(matches!(farm.log.commit(command), Applied::Tier(Ok(()))));
    }
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
    let round = cache.flush_tier(t).await.expect("round");
    assert_eq!(round.conflicts, [object]);
    assert_eq!(farm.log.staged_state(object), None);
    assert!(fs.contents(&kbf_tier::block_name(object)).is_none());
    let moved = farm.log.object_of(&blobs[0].digest);
    assert_ne!(moved, object);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, [moved]);
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round, FlushRound::default());
}

/// Catches: a round that takes its [`kbf_front::FLUSHES_PER_ROUND`] oldest sealed
/// objects before it drops those waiting out a backoff. Four old objects that keep
/// failing would then starve every newer one.
#[tokio::test]
async fn objects_waiting_out_a_backoff_do_not_starve_newer_ones() {
    assert_eq!(kbf_front::FLUSHES_PER_ROUND, 4);
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

// ---- Moving a conflicting object (write-path.md, section 6.4). ----

/// A farm whose first flush meets another object under the staged object's key.
async fn conflicting(farm: &Farm) -> (support::TierCache, Vec<TestBlob>, ObjectId) {
    let cache = farm.start(GIB).await;
    let blobs = blobs(1, 2, 4096);
    cache.store_blobs(verified(&blobs)).await.expect("stored");
    let object = farm.log.object_of(&blobs[0].digest);
    let taken = Bytes::from_static(b"some other object under this key");
    let at = key(&cache, object);
    farm.store.put_new(&at, taken, None).await.expect("put");
    (cache, blobs, object)
}

/// Catches: a move that marks the old object or syncs after it has (R1): a failed or
/// cut `fdatasync`, a failed copy or a power cut at any disk step of the move leaves the
/// blobs readable, whether the process lives on or restarts from what was durable.
/// With the old order (mark, then sync) the cut at the sync leaves no copy: the mark
/// unstaged the old object and the start sweeps its block. Runs the fault at each of
/// the first 40 disk operations of the round, as an error and as a crash.
#[tokio::test]
async fn a_failure_or_a_power_cut_at_any_disk_step_of_a_move_loses_no_blob() {
    for crash in [false, true] {
        for step in 0..40 {
            let farm = Farm::new(CONDITIONAL);
            let (cache, blobs, _) = conflicting(&farm).await;
            let fault = if crash { Fault::Crash } else { Fault::Error };
            farm.fs.fail_at(farm.fs.ops() + step, fault);
            let _ = cache.flush_tier(later()).await;
            let case = format!("crash {crash}, step {step}");
            // A fault planned past the round's last disk step has not struck: the live
            // reads would meet it.
            if !crash && farm.fs.faults_met() > 0 {
                for b in &blobs {
                    let read = cache.read_blob(&b.digest).await;
                    assert_eq!(read.as_ref().ok(), Some(&b.bytes), "{case}: live read");
                }
            }
            drop(cache);
            let fs = farm.fs.crash(0);
            let (cache, startup) = start(&fs, &farm.log, &farm.store, GIB, None)
                .await
                .unwrap_or_else(|e| panic!("{case}: start: {e}"));
            assert_eq!(startup.recovery.lost, [], "{case}");
            assert_eq!(farm.log.losses(), (0, 0), "{case}");
            for b in &blobs {
                let read = cache.read_blob(&b.digest).await;
                assert_eq!(read.as_ref().ok(), Some(&b.bytes), "{case}: after start");
            }
        }
    }
}

/// Catches: a move the log refuses (R2) that changes the old object, strands the new
/// block (its space is never freed: nothing stages it), or gives up on the first
/// refusal. Three refusals in a row end the attempt: the object is staged as it was, its
/// block is kept, the disk holds no other block, and the round backs off; the next
/// round moves it.
#[tokio::test]
async fn a_move_the_log_keeps_refusing_leaves_the_object_and_no_stray_block() {
    let farm = Farm::new(CONDITIONAL);
    let (cache, blobs, object) = conflicting(&farm).await;
    let files = farm.fs.list().expect("list");
    farm.log.refuse("reseat_staged", 3);
    let t0 = later();
    let round = cache.flush_tier(t0).await.expect("round");
    assert_eq!(round.conflicts, []);
    assert_eq!(round.retrying, [(object, FLUSH_BACKOFF_FIRST)]);
    assert_eq!(farm.log.staged_state(object), Some(StagedState::Sealed));
    assert!(farm.has_block(object));
    assert_eq!(farm.fs.list().expect("list"), files, "no stray block");
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    let round = cache
        .flush_tier(t0 + FLUSH_BACKOFF_FIRST)
        .await
        .expect("round");
    assert_eq!(round.conflicts, [object]);
    assert_eq!(farm.log.staged_state(object), None);
    assert!(!farm.has_block(object));
}

/// Catches: no retry of the last commit (N2): one refusal, or one answer lost after the
/// command applied, must not undo the move or apply it twice. The retry of an applied
/// command is a no-op, and the block that is synced and held is the one that stays.
#[tokio::test]
async fn the_commit_of_a_move_is_tried_again() {
    for lost in [false, true] {
        let farm = Farm::new(CONDITIONAL);
        let (cache, blobs, object) = conflicting(&farm).await;
        if lost {
            farm.log.lose_answer("reseat_staged", 1);
        } else {
            farm.log.refuse("reseat_staged", 1);
        }
        let round = cache.flush_tier(later()).await.expect("round");
        assert_eq!(round.conflicts, [object], "lost {lost}: {round:?}");
        assert_eq!(round.retrying, [], "lost {lost}");
        let moved = farm.log.object_of(&blobs[0].digest);
        assert_ne!(moved, object);
        assert_eq!(farm.log.staged_state(moved), Some(StagedState::Sealed));
        assert!(farm.has_block(moved) && !farm.has_block(object));
        let round = cache.flush_tier(later()).await.expect("round");
        assert_eq!(round.flushed, [moved], "lost {lost}");
    }
}

/// Catches: a conflict counted again each time the move is tried (N3): the counter is
/// the number of objects moved, not of attempts.
#[tokio::test]
async fn a_move_that_fails_is_not_counted_as_a_conflict_until_it_succeeds() {
    let farm = Farm::new(CONDITIONAL);
    let (cache, _, _) = conflicting(&farm).await;
    farm.log.refuse("reseat_staged", 3);
    let t0 = later();
    cache.flush_tier(t0).await.expect("round");
    assert!(
        cache
            .render_metrics()
            .contains("\nkbf_write_tier_flush_conflicts_total 0\n")
    );
    cache
        .flush_tier(t0 + FLUSH_BACKOFF_FIRST)
        .await
        .expect("round");
    assert!(
        cache
            .render_metrics()
            .contains("\nkbf_write_tier_flush_conflicts_total 1\n")
    );
}

/// Catches: an object an earlier build left in conflict whose block is gone (N4) tried
/// again every round for ever. Nothing can produce its records: it ends as a loss (marked
/// missing, counted once), and the next round has nothing to do.
#[tokio::test]
async fn a_conflict_whose_block_is_gone_ends_as_a_loss() {
    let farm = Farm::new(CONDITIONAL);
    let (cache, blobs, object) = conflicting(&farm).await;
    for command in [
        Command::SealStaged { object },
        Command::FlushConflict { object },
    ] {
        assert!(matches!(farm.log.commit(command), Applied::Tier(Ok(()))));
    }
    farm.fs
        .remove(&kbf_tier::block_name(object))
        .expect("remove");
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.conflicts, [object]);
    assert_eq!(farm.log.staged_state(object), None);
    assert!(farm.log.unreachable(object).is_some());
    assert_eq!(farm.log.losses(), (blobs.len() as u64, blobs.len() as u64));
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round, FlushRound::default());
}

/// Catches: a conflict object with no records (N4) taken out of the queue by nothing:
/// the move would discard its block and repeat every round.
#[tokio::test]
async fn a_conflict_with_no_records_leaves_the_table() {
    let farm = Farm::new(CONDITIONAL);
    let (cache, blobs, taken) = conflicting(&farm).await;
    let empty = ObjectId::of(taken.writer(), 900);
    let tier = cache.tier_id().expect("tier");
    let put = StagedPut {
        object: empty,
        end: 0,
        seal: true,
        blobs: Vec::new(),
    };
    let staged = Command::PutStaged {
        tier,
        objects: vec![put],
    };
    assert!(matches!(farm.log.commit(staged), Applied::Staged(Ok(_))));
    let conflict = Command::FlushConflict { object: empty };
    assert!(matches!(farm.log.commit(conflict), Applied::Tier(Ok(()))));
    let round = cache.flush_tier(later()).await.expect("round");
    assert!(round.conflicts.contains(&empty), "{round:?}");
    assert_eq!(farm.log.staged_state(empty), None);
    assert_eq!(farm.log.losses(), (0, 0));
    for b in &blobs {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    // The real conflict moved in the same round and flushes now; then nothing is left.
    let moved = farm.log.object_of(&blobs[0].digest);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round.flushed, [moved]);
    let round = cache.flush_tier(later()).await.expect("round");
    assert_eq!(round, FlushRound::default());
}
