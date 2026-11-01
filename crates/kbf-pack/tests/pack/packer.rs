//! The packer: the write rule.

use std::sync::Arc;

use bytes::Bytes;
use kbf_objstore::{ByteRange, ObjectStore};
use kbf_pack::{Blob, Fetch, PackError, Packer, PackerConfig, ReaderConfig, check_create_only};
use kbf_pack_index::{MAX_BATCH, PackIndex, PackIndexError};
use kbf_segments::SegmentReader;
use kbf_segments::layout::footer_len;

use crate::support::{FaultyStore, Rig, blob, digests, filler, keys, limits, pack_all, pack_key};

/// Asserts that blobs are packed into one pack under the writer's first key, that the object
/// is a segment whose footer holds every blob, that the index points each blob at the pack,
/// that the pack is recorded under the writer, and that the report says what was written.
#[tokio::test]
async fn blobs_are_packed_stored_and_indexed() {
    let rig = Rig::new();
    let packer = rig.packer(PackerConfig::default());
    let blobs = [blob("alpha"), blob("bravo"), blob("charlie")];

    let packed = pack_all(&packer, &blobs).await;

    assert_eq!(packed.skipped, 0);
    assert_eq!(packed.packs.len(), 1);
    let pack = &packed.packs[0];
    assert_eq!(pack.key, pack_key(0));
    assert_eq!(pack.blobs, 3);
    assert_eq!(pack.indexed, 3);

    let whole = ByteRange::new(0, pack.size).unwrap();
    let object = rig.store.get_range(&keys().key(0), whole).await.unwrap();
    assert_eq!(object.len() as u64, pack.size);
    let segment = SegmentReader::open(&object).unwrap();
    for item in &blobs {
        let digest = kbf_segments::sha256(&item.bytes);
        assert_eq!(segment.get(&digest).unwrap(), Some(&item.bytes[..]));
    }

    for place in rig.index.locate(&digests(&blobs)).unwrap() {
        assert_eq!(place.unwrap().pack_key, pack.key);
    }
    let info = rig.index.pack(&pack.key).unwrap().unwrap();
    assert_eq!(info.writer, "d/node1");
    assert_eq!(info.blob_count, 3);
    assert_eq!(info.size, pack.size);
}

/// Asserts that a pack is recorded only after the store holds it: at each record the object
/// is already there, for every pack of a call that writes several.
#[tokio::test]
async fn a_pack_is_recorded_only_once_the_store_holds_it() {
    let rig = Rig::new();
    let packer = rig.packer(limits(2_000, 100));
    let blobs: Vec<Blob> = (0..5).map(|i| filler(400, i)).collect();

    let packed = pack_all(&packer, &blobs).await;

    assert_eq!(packed.packs.len(), 3);
    assert_eq!(rig.index.held_at_record(), [true, true, true]);
}

/// Asserts that blobs the index has already are not packed again, in a later call, and that
/// the next pack takes the next key.
#[tokio::test]
async fn blobs_the_index_has_are_not_packed_again() {
    let rig = Rig::new();
    let packer = rig.packer(PackerConfig::default());
    let blobs = [blob("a"), blob("b"), blob("c")];

    let first = pack_all(&packer, &blobs[..2]).await;
    assert_eq!(first.packs.len(), 1);
    assert_eq!(first.skipped, 0);

    let second = pack_all(&packer, &blobs).await;
    assert_eq!(second.packs.len(), 1);
    assert_eq!(second.packs[0].blobs, 1);
    assert_eq!(second.packs[0].key, pack_key(1));
    assert_eq!(second.skipped, 2);

    let third = pack_all(&packer, &blobs).await;
    assert!(third.packs.is_empty());
    assert_eq!(third.skipped, 3);
    assert_eq!(rig.store.keys_under("cas/").await.len(), 2);
}

/// Asserts that a blob named twice in one call is packed once.
#[tokio::test]
async fn a_blob_named_twice_is_packed_once() {
    let rig = Rig::new();
    let packer = rig.packer(PackerConfig::default());
    let blobs = [blob("same"), blob("same"), blob("other")];

    let packed = pack_all(&packer, &blobs).await;

    assert_eq!(packed.packs.len(), 1);
    assert_eq!(packed.packs[0].blobs, 2);
    assert_eq!(packed.skipped, 1);
}

/// Asserts that a call with nothing to pack writes nothing.
#[tokio::test]
async fn a_call_without_blobs_writes_nothing() {
    let rig = Rig::new();
    let packer = rig.packer(PackerConfig::default());

    let packed = pack_all(&packer, &[]).await;

    assert!(packed.packs.is_empty());
    assert_eq!(packed.skipped, 0);
    assert!(rig.store.keys_under("cas/").await.is_empty());
}

/// Asserts that a call asks the index which blobs it lacks in batches of at most `MAX_BATCH`
/// digests, so that a call of any size is one the index takes.
#[tokio::test]
async fn a_call_asks_the_index_in_batches() {
    let rig = Rig::new();
    let packer = rig.packer(PackerConfig::default());
    let blobs: Vec<Blob> = (0..1001).map(|i| blob(&format!("blob {i}"))).collect();

    let packed = pack_all(&packer, &blobs).await;

    assert_eq!(packed.packs.len(), 1);
    assert_eq!(packed.packs[0].blobs, 1001);
    assert_eq!(rig.index.find_batches(), [MAX_BATCH, 1]);
}

/// Asserts that a pack is cut when the next blob would not fit under the byte limit: five
/// blobs of 400 bytes under a limit of 2,000 are packs of two, two and one, under successive
/// keys, none longer than the limit, and every blob is in the index afterwards.
#[tokio::test]
async fn a_pack_is_cut_at_the_byte_limit() {
    let rig = Rig::new();
    let packer = rig.packer(limits(2_000, 100));
    let blobs: Vec<Blob> = (0..5).map(|i| filler(400, i)).collect();

    let packed = pack_all(&packer, &blobs).await;

    let counts: Vec<usize> = packed.packs.iter().map(|p| p.blobs).collect();
    assert_eq!(counts, [2, 2, 1]);
    let written: Vec<String> = packed.packs.iter().map(|p| p.key.clone()).collect();
    assert_eq!(written, [pack_key(0), pack_key(1), pack_key(2)]);
    assert!(packed.packs.iter().all(|p| p.size <= 2_000));
    let missing = rig.index.find_missing(&digests(&blobs)).unwrap();
    assert!(missing.is_empty());
}

/// Asserts that a pack is cut at the blob limit although the byte limit is far away.
#[tokio::test]
async fn a_pack_is_cut_at_the_blob_limit() {
    let rig = Rig::new();
    let packer = rig.packer(limits(PackerConfig::default().pack_bytes, 2));
    let blobs: Vec<Blob> = (0..5).map(|i| filler(10, i)).collect();

    let packed = pack_all(&packer, &blobs).await;

    let counts: Vec<usize> = packed.packs.iter().map(|p| p.blobs).collect();
    assert_eq!(counts, [2, 2, 1]);
}

/// Asserts that a blob limit of zero is read as one blob a pack, not as a pack that can hold
/// nothing.
#[tokio::test]
async fn a_blob_limit_of_zero_is_one() {
    let rig = Rig::new();
    let packer = rig.packer(limits(PackerConfig::default().pack_bytes, 0));
    let blobs = [filler(10, 1), filler(10, 2)];

    let packed = pack_all(&packer, &blobs).await;

    let counts: Vec<usize> = packed.packs.iter().map(|p| p.blobs).collect();
    assert_eq!(counts, [1, 1]);
}

/// Asserts that a blob too big for a pack (with its footer) is a pack of its own, written
/// before the pack of the small blobs around it, and that it reads back whole.
#[tokio::test]
async fn a_blob_too_big_for_a_pack_is_a_pack_of_its_own() {
    let rig = Rig::new();
    let packer = rig.packer(limits(2_000, 100));
    let blobs = [filler(100, 1), filler(3_000, 2), filler(100, 3)];

    let packed = pack_all(&packer, &blobs).await;

    let counts: Vec<usize> = packed.packs.iter().map(|p| p.blobs).collect();
    assert_eq!(counts, [1, 2]);
    assert_eq!(packed.packs[0].size, 3_000 + footer_len(1));
    let reader = rig.reader(None, ReaderConfig::default());
    let got = reader.read(&digests(&blobs)).await.unwrap();
    assert_eq!(got[1], Fetch::Found(blobs[1].bytes.clone()));
    assert_eq!(got[0], Fetch::Found(blobs[0].bytes.clone()));
}

/// Asserts that a call whose blobs are all too big for a pack writes one pack for each and no
/// pack of the small blobs that are not there.
#[tokio::test]
async fn only_big_blobs_make_only_packs_of_their_own() {
    let rig = Rig::new();
    let packer = rig.packer(limits(2_000, 100));
    let blobs = [filler(3_000, 1), filler(5_000, 2)];

    let packed = pack_all(&packer, &blobs).await;

    let counts: Vec<usize> = packed.packs.iter().map(|p| p.blobs).collect();
    assert_eq!(counts, [1, 1]);
}

/// Asserts that a key another writer holds is neither overwritten nor deleted, and that the
/// pack goes under the next key.
#[tokio::test]
async fn a_taken_key_is_left_alone_and_the_next_key_is_used() {
    let rig = Rig::new();
    rig.store.seed(&pack_key(0), b"someone else's").await;
    let packer = rig.packer(PackerConfig::default());

    let packed = pack_all(&packer, &[blob("x")]).await;

    assert_eq!(packed.packs[0].key, pack_key(1));
    let ask = ByteRange::new(0, 100).unwrap();
    let held = rig.store.get_range(&keys().key(0), ask).await.unwrap();
    assert_eq!(held, Bytes::from_static(b"someone else's"));
}

/// Asserts that when every key the attempts try is taken the call fails, the objects of the
/// others are untouched, and nothing is recorded.
#[tokio::test]
async fn when_every_key_is_taken_the_call_fails_and_records_nothing() {
    let rig = Rig::new();
    for seq in 0..3 {
        rig.store.seed(&pack_key(seq), b"taken").await;
    }
    let packer = rig.packer(PackerConfig::default());
    let x = blob("x");

    let error = packer.pack(vec![x.clone()]).await.unwrap_err();

    assert!(matches!(error, PackError::Put { attempts: 3, .. }));
    assert_eq!(rig.store.keys_under("cas/").await.len(), 3);
    assert!(rig.index.held_at_record().is_empty());
    assert_eq!(rig.index.find_missing(&[x.digest]).unwrap(), [x.digest]);
}

/// Asserts that a write that fails records nothing and is tried again under the next key, and
/// that the pack is recorded once, after the store holds it.
#[tokio::test]
async fn a_failed_write_is_retried_under_a_new_key() {
    let rig = Rig::new();
    rig.store.reject_puts(1);
    let packer = rig.packer(PackerConfig::default());

    let packed = pack_all(&packer, &[blob("x")]).await;

    assert_eq!(packed.packs[0].key, pack_key(1));
    assert!(!rig.store.holds(&pack_key(0)).await);
    assert_eq!(rig.index.held_at_record(), [true]);
}

/// Asserts that when every attempt to write fails the call fails with the last key tried,
/// the store holds nothing, and the index has no row for the blob.
#[tokio::test]
async fn when_every_write_fails_nothing_is_stored_or_recorded() {
    let rig = Rig::new();
    rig.store.reject_puts(3);
    let packer = rig.packer(PackerConfig::default());
    let x = blob("x");

    let error = packer.pack(vec![x.clone()]).await.unwrap_err();

    assert!(matches!(error, PackError::Put { attempts: 3, .. }));
    assert!(error.to_string().contains(&pack_key(2)), "{error}");
    assert!(rig.store.keys_under("cas/").await.is_empty());
    assert!(rig.index.held_at_record().is_empty());
    assert_eq!(rig.index.find_missing(&[x.digest]).unwrap(), [x.digest]);
}

/// Asserts that a write that reports a failure after it stored the object leaves nothing
/// behind: the packer deletes the attempt before it tries the next key.
#[tokio::test]
async fn a_write_that_landed_but_failed_is_deleted_before_the_retry() {
    let rig = Rig::new();
    rig.store.lose_puts(1);
    let packer = rig.packer(PackerConfig::default());

    let packed = pack_all(&packer, &[blob("x")]).await;

    assert_eq!(packed.packs[0].key, pack_key(1));
    assert!(!rig.store.holds(&pack_key(0)).await);
    assert_eq!(rig.store.keys_under("cas/").await, [pack_key(1)]);
}

/// Asserts that when the delete of a failed attempt fails too, the call still goes on, and the
/// attempt is an object no row points at, which the index's unreferenced query returns.
#[tokio::test]
async fn a_failed_cleanup_leaves_an_unreferenced_pack() {
    let rig = Rig::new();
    rig.store.lose_puts(1);
    rig.store.fail_deletes(1);
    let packer = rig.packer(PackerConfig::default());

    let packed = pack_all(&packer, &[blob("x")]).await;

    assert_eq!(packed.packs[0].key, pack_key(1));
    assert!(rig.store.holds(&pack_key(0)).await);
    let listed = rig.store.keys_under("cas/").await;
    let unreferenced = rig.index.unreferenced_keys(&listed).unwrap();
    assert_eq!(unreferenced, [pack_key(0)]);
}

/// Asserts that a record that fails once after the write is tried again with the same pack:
/// the call succeeds under the first key, the store holds that one pack, both records found it
/// there, and the blob is in the index.
#[tokio::test]
async fn a_failed_record_is_tried_again_for_the_same_pack() {
    let rig = Rig::new();
    rig.index.fail_record(1);
    let packer = rig.packer(PackerConfig::default());
    let x = blob("x");

    let packed = pack_all(&packer, std::slice::from_ref(&x)).await;

    assert_eq!(packed.packs[0].key, pack_key(0));
    assert_eq!(packed.packs[0].indexed, 1);
    assert_eq!(rig.store.keys_under("cas/").await, [pack_key(0)]);
    assert_eq!(rig.index.held_at_record(), [true, true]);
    assert!(rig.index.find_missing(&[x.digest]).unwrap().is_empty());
}

/// Asserts that a record the index made and whose answer was lost is tried again as a no-op:
/// the call succeeds under the first key, reports the blob as indexed, and leaves no pack
/// unreferenced.
#[tokio::test]
async fn a_record_whose_answer_was_lost_leaves_no_orphan() {
    let rig = Rig::new();
    rig.index.lose_record(1);
    let packer = rig.packer(PackerConfig::default());

    let packed = pack_all(&packer, &[blob("x")]).await;

    assert_eq!(packed.packs[0].key, pack_key(0));
    assert_eq!(packed.packs[0].indexed, 1);
    let listed = rig.store.keys_under("cas/").await;
    assert!(rig.index.unreferenced_keys(&listed).unwrap().is_empty());
    assert_eq!(rig.index.held_at_record(), [true, true]);
}

/// Asserts that a record that fails in every attempt after the write leaves the pack in the
/// store with no row, that the index's unreferenced query returns it, that the blob is still
/// missing, and that packing it again writes a new pack and leaves the first one for garbage
/// collection.
#[tokio::test]
async fn a_failed_record_leaves_an_unreferenced_pack_garbage_collection_finds() {
    let rig = Rig::new();
    rig.index.fail_record(3);
    let packer = rig.packer(PackerConfig::default());
    let x = blob("x");

    let error = packer.pack(vec![x.clone()]).await.unwrap_err();

    assert!(matches!(error, PackError::Record { .. }), "{error}");
    assert!(error.to_string().contains(&pack_key(0)), "{error}");
    assert!(rig.store.holds(&pack_key(0)).await);
    assert!(rig.index.pack(&pack_key(0)).unwrap().is_none());
    let listed = rig.store.keys_under("cas/").await;
    let unreferenced = rig.index.unreferenced_keys(&listed).unwrap();
    assert_eq!(unreferenced, [pack_key(0)]);
    assert_eq!(rig.index.find_missing(&[x.digest]).unwrap(), [x.digest]);

    let again = pack_all(&packer, &[x]).await;

    assert_eq!(again.packs[0].key, pack_key(1));
    let listed = rig.store.keys_under("cas/").await;
    let unreferenced = rig.index.unreferenced_keys(&listed).unwrap();
    assert_eq!(unreferenced, [pack_key(0)]);
    assert_eq!(rig.index.held_at_record(), [true, true, true, true]);
}

/// Builds a blob whose bytes are not the digest it names: `len` bytes of one fill under the
/// digest of `len` bytes of another.
fn liar(len: usize) -> Blob {
    Blob {
        digest: filler(len, 1).digest,
        bytes: filler(len, 2).bytes,
    }
}

/// Asserts that bytes that are not the digest named for them are refused wherever the packer
/// meets them (in an open pack, alone in a pack of their own, and first in a pack just
/// started), and that only the packs finished before the refusal are in the store.
#[tokio::test]
async fn bytes_that_are_not_their_digest_are_refused() {
    let cases = [
        (limits(2_000, 100), vec![blob("good"), liar(10)], 0),
        (limits(2_000, 100), vec![liar(3_000)], 0),
        (limits(2_000, 1), vec![filler(10, 3), liar(10)], 1),
    ];
    for (config, blobs, stored) in cases {
        let rig = Rig::new();

        let error = rig.packer(config).pack(blobs).await.unwrap_err();

        assert!(matches!(error, PackError::DigestMismatch { .. }));
        assert!(error.to_string().contains("another digest"), "{error}");
        assert_eq!(rig.store.keys_under("cas/").await.len(), stored);
    }
}

/// Asserts that bytes whose length differs from the digest's size are refused, whether they
/// are longer or shorter than the digest says, before anything is written: no pack is stored
/// and the index still lacks the blob.
#[tokio::test]
async fn bytes_of_another_length_than_the_digest_says_are_refused() {
    let longer = Blob {
        digest: filler(10, 1).digest,
        bytes: filler(3_000, 1).bytes,
    };
    let shorter = Blob {
        digest: filler(3_000, 1).digest,
        bytes: filler(10, 1).bytes,
    };
    for bad in [longer, shorter] {
        let rig = Rig::new();
        let packer = rig.packer(limits(2_000, 100));
        let digest = bad.digest;

        let error = packer.pack(vec![blob("good"), bad]).await.unwrap_err();

        assert!(matches!(error, PackError::DigestMismatch { .. }), "{error}");
        assert!(rig.store.keys_under("cas/").await.is_empty());
        assert_eq!(rig.index.find_missing(&[digest]).unwrap(), [digest]);
    }
}

/// Asserts that an index that fails the question of which blobs it lacks fails the call before
/// anything is written.
#[tokio::test]
async fn an_index_that_fails_the_lookup_fails_the_call_before_any_write() {
    let rig = Rig::new();
    rig.index.fail_find(1);
    let packer = rig.packer(PackerConfig::default());

    let error = packer.pack(vec![blob("x")]).await.unwrap_err();

    assert!(matches!(error, PackError::Index(_)), "{error}");
    assert!(error.to_string().contains("pack index"), "{error}");
    assert!(rig.store.keys_under("cas/").await.is_empty());
}

/// Asserts that a blob too big for a pack, whose pack cannot be written, fails the call.
#[tokio::test]
async fn a_big_blob_whose_write_fails_fails_the_call() {
    let rig = Rig::new();
    rig.store.reject_puts(3);
    let packer = rig.packer(limits(2_000, 100));

    let error = packer.pack(vec![filler(3_000, 1)]).await.unwrap_err();

    assert!(matches!(error, PackError::Put { attempts: 3, .. }));
}

/// Asserts that when a pack in the middle of a call cannot be written the call fails, the packs
/// before it stay recorded, and a repeated call packs only the blobs that are left.
#[tokio::test]
async fn a_pack_that_cannot_be_written_mid_call_leaves_the_earlier_packs_recorded() {
    let rig = Rig::new();
    rig.store.reject_puts_after(1, 3);
    let packer = rig.packer(limits(PackerConfig::default().pack_bytes, 1));
    let blobs = [filler(10, 1), filler(10, 2), filler(10, 3)];

    let error = packer.pack(blobs.to_vec()).await.unwrap_err();

    assert!(matches!(error, PackError::Put { attempts: 3, .. }));
    let missing = rig.index.find_missing(&digests(&blobs)).unwrap();
    assert_eq!(missing, [blobs[1].digest, blobs[2].digest]);

    let again = pack_all(&packer, &blobs).await;

    assert_eq!(again.skipped, 1);
    assert_eq!(again.packs.len(), 2);
}

/// Asserts that the create-only self test passes on a store that refuses to replace an object,
/// and leaves no object behind.
#[tokio::test]
async fn the_create_only_self_test_passes_on_a_store_that_refuses_to_replace() {
    let rig = Rig::new();

    check_create_only(&*rig.store, &keys()).await.unwrap();

    let probe = keys().self_test_key().to_string();
    assert!(!rig.store.holds(&probe).await);
}

/// Asserts that the self test fails on a store that replaces an object it holds, and that a
/// packer is not made over a store that does not claim create-only writes.
#[tokio::test]
async fn a_store_that_replaces_objects_is_refused() {
    let rig = Rig::new();
    // The same store type as every other packer of these tests, so that the refusal is a branch
    // of the one copy of `Packer::new` they all run.
    let store = Arc::new(FaultyStore::replacing());

    let tested = check_create_only(&*store, &keys()).await.unwrap_err();
    assert!(matches!(tested, PackError::CreateOnlySelfTest));
    assert!(tested.to_string().contains("second write"), "{tested}");

    let index: Arc<dyn PackIndex> = rig.index.clone();
    let made = Packer::new(store, index, keys(), PackerConfig::default());
    let refused = made.err().unwrap();
    assert!(matches!(refused, PackError::NotCreateOnly));
    assert!(refused.to_string().contains("create-only"), "{refused}");
}

/// Asserts that a failure of the self test's first write is the store's error.
#[tokio::test]
async fn the_self_test_reports_a_failed_first_write() {
    let rig = Rig::new();
    rig.store.reject_puts(1);

    let error = check_create_only(&*rig.store, &keys()).await.unwrap_err();

    assert!(matches!(error, PackError::Store(_)), "{error}");
    assert!(error.to_string().starts_with("object store: "), "{error}");
}

/// Asserts that a packer given no limits has these: the farm's segment limit, a footer
/// of under 1 MiB, and three attempts; and that a frontend packs client uploads in 32 MiB.
#[test]
fn the_default_limits_are_the_ones_documented() {
    let config = PackerConfig::default();
    assert_eq!(config.pack_bytes, 128 << 20);
    assert_eq!(config.max_blobs, 16_384);
    assert_eq!(config.put_attempts, 3);
    assert_eq!(kbf_pack::FRONTEND_PACK_BYTES, 32 << 20);
}

/// Asserts that a record the index refuses is not tried again, as a repeat gets the same answer:
/// the call fails after one record with the refusal as the source, the pack is left for
/// garbage collection, and the next call writes and records a new pack.
#[tokio::test]
async fn a_refused_record_is_not_tried_again() {
    let rig = Rig::new();
    rig.index.refuse_record(1);
    let packer = rig.packer(PackerConfig::default());
    let x = blob("x");

    let error = packer.pack(vec![x.clone()]).await.unwrap_err();

    let PackError::Record { source, .. } = &error else {
        panic!("a record error: {error}");
    };
    assert_eq!(source, &PackIndexError::Refused("scripted".to_owned()));
    assert_eq!(rig.index.held_at_record(), [true]);
    assert!(rig.store.holds(&pack_key(0)).await);

    let again = pack_all(&packer, &[x]).await;

    assert_eq!(again.packs[0].key, pack_key(1));
}
