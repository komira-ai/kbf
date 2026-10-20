//! The write tier of a registered storage node (a node writer): its blocks named by the
//! member and boot, a tier id drawn although the node has no writer epoch, and a later
//! boot that recovers and flushes an earlier boot's staged blocks.
//!
//! Every test runs the same [`Farm`] as `write_tier.rs`. Each test's comment names the
//! defect it is there to catch.

mod common;
#[allow(dead_code)]
#[path = "write_tier/support.rs"]
mod support;

use std::num::NonZeroU64;

use bytes::Bytes;

use kbf_front::{Cache, CacheError, ObjectKeys, TierStartError};
use kbf_meta::{NodeWriter, TierId, Writer};
use kbf_objstore::{Capabilities, KeyPrefix};
use kbf_tier::block_name;
use support::{Farm, SERVER, TierCache, blobs, later, options, verified};

const GIB: u64 = 1 << 30;

fn node(member: u64, boot: u64) -> NodeWriter {
    let n = |v| NonZeroU64::new(v).expect("not zero");
    NodeWriter::new(n(member), n(boot))
}

fn keys() -> ObjectKeys {
    ObjectKeys {
        legacy: KeyPrefix::new("kbf/00000000000000aa/").expect("prefix"),
        root: KeyPrefix::new("kbf/").expect("root"),
    }
}

/// A start of `writer` over the farm, its tier attached.
async fn start_as(
    farm: &Farm,
    writer: NodeWriter,
) -> Result<(TierCache, kbf_front::TierStartup), TierStartError> {
    let cache = Cache::open_node(farm.log.clone(), farm.store.clone(), keys(), writer)
        .await
        .expect("open as a node writer");
    cache
        .attach_tier(farm.fs.clone(), options(GIB), SERVER, None)
        .await
}

/// Catches: a node writer's tier registered under an id taken from no epoch (the node
/// has none: an id of 0, or the boot, could be a tier id the log retired), its blocks
/// named as an epoch writer's, a later boot that cannot recover an earlier boot's
/// staged block (the log must still take marks and flushes of it), and a flushed key
/// that is not the node layout. Mutant seen red: `new_tier_id` answering the boot for a
/// node writer (boot 1 is the epoch writer's retired tier 1 here, and the start is
/// refused).
#[tokio::test]
async fn a_node_writer_stages_under_its_name_and_a_later_boot_flushes_it() {
    let farm = Farm::new(Capabilities::default());
    // An epoch writer ran here first: its tier (id 1, its epoch) is retired below.
    let (first, _) = support::start(&farm.fs, &farm.log, &farm.store, GIB, None)
        .await
        .expect("an epoch writer's start");
    assert_eq!(first.tier_id(), Some(TierId::new(1)));
    drop(first);
    let abandoned = Cache::open_node(farm.log.clone(), farm.store.clone(), keys(), node(7, 1))
        .await
        .expect("open");
    let (cache, startup) = abandoned
        .attach_tier(farm.fs.clone(), options(GIB), SERVER, Some(TierId::new(1)))
        .await
        .expect("a node writer's tier after abandoning the old one");
    assert!(startup.registered);
    assert_eq!(
        cache.tier_id(),
        Some(TierId::new(2)),
        "a fresh epoch as the id"
    );
    let written = blobs(1, 2, 1024);
    cache.store_blobs(verified(&written)).await.expect("staged");
    let object = farm.log.object_of(&written[0].digest);
    assert_eq!(object.writer(), Writer::Node(node(7, 1)));
    assert!(farm.fs.contents(&block_name(object)).is_some());
    assert!(farm.store.put_keys().is_empty(), "staged, not PUT");
    drop(cache);

    let (cache, startup) = start_as(&farm, node(7, 2)).await.expect("boot 2");
    assert!(!startup.registered, "the tier is the registered one");
    assert_eq!(cache.tier_id(), Some(TierId::new(2)));
    let round = cache.flush_tier(later()).await.expect("flush");
    assert!(round.flushed.contains(&object), "{round:?}");
    let flushed = cache.object_key(object).expect("key");
    assert_eq!(
        flushed.as_str(),
        format!(
            "kbf/cas/0000000000000007/0000000000000001/{:016x}",
            object.seq()
        )
    );
    assert!(farm.store.put_keys().contains(&flushed));
    for b in &written {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    drop(cache);
    let again = Cache::open_node(farm.log.clone(), farm.store.clone(), keys(), node(7, 2)).await;
    assert!(
        matches!(again, Err(CacheError::StaleWriter(_))),
        "boot 2 again: {:?}",
        again.err()
    );
}

/// Catches: a node open or a node tier start that takes an outcome the log did not
/// give for its command (a broken log): the cache would write as a writer the log never
/// set, or register a tier id nobody drew. Both are internal errors, and nothing opens.
#[tokio::test]
async fn a_log_that_misanswers_a_node_start_refuses_it() {
    let farm = Farm::new(Capabilities::default());
    farm.log.answer_wrong("set_writer", 1);
    let refused = Cache::open_node(farm.log.clone(), farm.store.clone(), keys(), node(7, 1)).await;
    assert!(
        matches!(refused, Err(CacheError::Internal(_))),
        "{:?}",
        refused.err()
    );
    farm.log.answer_wrong("alloc_epoch", 1);
    let refused = start_as(&farm, node(7, 2)).await;
    assert!(
        matches!(refused, Err(TierStartError::Cache(CacheError::Internal(_)))),
        "{:?}",
        refused.err()
    );
}

/// Catches: a node whose member and boot were handed out again by a fresh state store
/// (the keys `cas/<member>/<boot>/<seq>` of the earlier store's lifetime are in the
/// bucket) that leaves each segment staged for ever, or that moves it to the next
/// number, which the earlier lifetime used too, one conflict and one wasted upload per
/// number. The first conflict must move the numbering past the earlier lifetime's, so
/// the segment's next flush is the last; the earlier lifetime's objects stay as they were.
/// Mutants seen red: `leave_numbering` doing nothing (the segment moves to number 2,
/// one of the three taken), and a conflict that commits `FlushConflict` and leaves the
/// object staged.
#[tokio::test]
async fn a_member_and_boot_handed_out_again_by_a_fresh_store_do_not_strand_segments() {
    let first = Farm::new(support::CONDITIONAL);
    let (old, _) = start_as(&first, node(1, 2)).await.expect("first lifetime");
    let mut taken = Vec::new();
    for seed in 1..=3 {
        let written = blobs(seed * 10, 1, 2048);
        old.store_blobs(verified(&written)).await.expect("staged");
        let object = first.log.object_of(&written[0].digest);
        let round = old.flush_tier(later()).await.expect("flush");
        assert_eq!(round.flushed, [object]);
        taken.push((object, old.object_key(object).expect("key")));
    }
    assert_eq!(
        taken.iter().map(|(o, _)| o.seq()).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let before = bytes_of(&first, &taken).await;
    drop(old);

    // A frontend on a fresh state store: a new log, the same member and boot, the same
    // bucket.
    let second = Farm {
        fs: kbf_store::FaultFs::new(),
        log: support::Log::new(),
        store: first.store.clone(),
    };
    let (cache, _) = start_as(&second, node(1, 2))
        .await
        .expect("second lifetime");
    let written = blobs(100, 2, 2048);
    cache.store_blobs(verified(&written)).await.expect("staged");
    let object = second.log.object_of(&written[0].digest);
    assert_eq!(object.seq(), 1, "the numbering started again");

    let round = cache.flush_tier(later()).await.expect("conflict round");
    assert_eq!(
        (round.conflicts.clone(), round.flushed.len()),
        (vec![object], 0)
    );
    assert_eq!(second.log.staged_state(object), None);
    let moved = second.log.object_of(&written[0].digest);
    assert!(
        moved.seq() > 3,
        "past the earlier lifetime's: {}",
        moved.seq()
    );
    for b in &written {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    let round = cache.flush_tier(later()).await.expect("flush round");
    assert_eq!((round.conflicts.len(), round.flushed), (0, vec![moved]));
    assert_eq!(second.log.staged_state(moved), None);
    for b in &written {
        assert_eq!(cache.read_blob(&b.digest).await.expect("read"), b.bytes);
    }
    assert_eq!(
        bytes_of(&second, &taken).await,
        before,
        "the earlier lifetime's objects are as they were"
    );
}

/// The bytes under each of `taken`'s keys.
async fn bytes_of(
    farm: &Farm,
    taken: &[(kbf_meta::ObjectId, kbf_objstore::ObjectKey)],
) -> Vec<Bytes> {
    use kbf_objstore::{ByteRange, ObjectStore};
    let all = ByteRange::new(0, 1 << 20).expect("range");
    let mut got = Vec::new();
    for (_, key) in taken {
        got.push(farm.store.get_range(key, all).await.expect("get"));
    }
    got
}
