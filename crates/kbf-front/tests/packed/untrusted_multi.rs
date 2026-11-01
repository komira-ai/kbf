//! `ingest_set`: a run of several packs and large objects, read from the object store under the
//! keys of an `IssuedKeySet`. Pack 0 holds the manifest; packs 1.. hold more records; a large
//! object is a raw object under an `L<i>` key that is streamed through SHA-256 into the CAS, a
//! window at a time, and never held whole. The action-cache entry is written last.
//!
//! The fixtures are in `multi`. Every test says the behaviour it asserts.

use std::sync::atomic::Ordering;

use kbf_front::{
    Ingested, IssuedKeySet, IssuedPackKey, PackKeyMint, RejectReason, UntrustedLimits, ingest,
};
use kbf_pack_index::PackIndex as _;
use kbf_segments::sha256;
use tonic::Code;

use crate::multi::{LARGE_LEN, Multi, WINDOW, large, result_of, streamed_by_node};
use crate::support::blob;
use crate::untrusted_task::{LEASE, SECRET, entry_of, forged, now, segment_of, tree_of};

/// Asserts that a run of two packs is ingested into exactly the result its manifest describes and
/// cached: the tree is read from the pack that holds it and the file it names is in the other, every
/// record of both packs and both packs are in the index, an unused key is not, and the counts say
/// two packs. Pack 1 starts with a record over the manifest cap, which only a pack without a
/// manifest may do.
#[tokio::test]
async fn a_two_pack_run_is_ingested_and_cached() {
    let farm = Multi::new().await;
    let (manifest, zero, one) = farm.two_packs();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.pack_key(1), &one).await;

    let got = farm.run().await;

    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(got, want);
    assert_eq!(farm.cached().await.expect("a hit"), result_of(&manifest));
    assert!(farm.rig.index.has_all(&farm.p.digests()));
    for key in &farm.keys.packs()[..2] {
        assert!(farm.rig.index.pack(key.key()).expect("asked").is_some());
    }
    let unused = farm.pack_key(2).key();
    assert!(farm.rig.index.pack(unused).expect("asked").is_none());
    let metrics = farm.rig.packs.metrics();
    assert_eq!(metrics.ingests(), 1);
    assert_eq!(metrics.ingest_packs(), 2);
    assert_eq!(metrics.ingest_large_bytes(), 0);
    let mut scrape = String::new();
    farm.rig.packs.render_metrics(&mut scrape);
    assert!(
        scrape.contains("kbf_pack_ingest_packs_total 2\n"),
        "{scrape}"
    );
    assert!(
        scrape.contains("kbf_pack_ingest_large_bytes_total 0\n"),
        "{scrape}"
    );
}

/// Asserts that a run of one pack ingested through the set of one key is what `ingest` makes of
/// it: the same result and the cache entry, one pack counted. Catches a set that changes what a run
/// of one pack is.
#[tokio::test]
async fn a_run_of_one_pack_through_a_set_of_one_key_is_what_ingest_makes_of_it() {
    let farm = Multi::new().await;
    let p = &farm.p;
    let manifest = farm.manifest(1, &[]);
    let records: [&[u8]; 5] = [&p.alpha, &p.beta, &p.stdout, &p.tree.1, &p.in_dir];
    let zero = Multi::pack_zero(&manifest, &records);
    farm.put(farm.pack_key(0), &zero).await;
    let set = IssuedKeySet::single(farm.pack_key(0).clone());

    let through_set = farm.run_with(&set, &UntrustedLimits::default()).await;
    let (packs, backend, mint) = (&farm.rig.packs, &farm.backend, &farm.mint);
    let limits = UntrustedLimits::default();
    let key = farm.pack_key(0);
    let plain = ingest(packs, backend, mint, LEASE, key, &farm.action, &limits).await;

    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(through_set, want);
    assert_eq!(plain, want);
    assert_eq!(farm.rig.packs.metrics().ingest_packs(), 2);
}

/// Asserts that a pack 1 the store does not hold is `Incomplete`, not `Missing` (pack 0 is there),
/// and that nothing is indexed, not even pack 0, and nothing is cached.
#[tokio::test]
async fn a_missing_second_pack_is_incomplete() {
    let farm = Multi::new().await;
    let (_, zero, _) = farm.two_packs();
    farm.put(farm.pack_key(0), &zero).await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::Incomplete).await;
    farm.nothing_indexed();
}

/// Asserts that a manifest that names more packs than the set holds is a `BadManifest` and that
/// the store was asked about pack 0 only: a pack the frontend did not issue a key for is never
/// read.
#[tokio::test]
async fn a_pack_beyond_the_issued_count_is_a_bad_manifest() {
    let farm = Multi::sized(2, 2).await;
    let p = &farm.p;
    let manifest = farm.manifest(3, &[]);
    let zero = Multi::pack_zero(&manifest, &[&p.tree.1, &p.beta, &p.stdout]);
    farm.put(farm.pack_key(0), &zero).await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::BadManifest).await;
    let first = farm.pack_key(0).key();
    assert!(
        farm.rig
            .store
            .ranges()
            .iter()
            .all(|(key, _, _)| key == first)
    );
}

/// Asserts that a large entry in a slot the set holds no key for is a `BadManifest`, and that no
/// large object was asked about.
#[tokio::test]
async fn a_large_slot_beyond_the_issued_count_is_a_bad_manifest() {
    let farm = Multi::sized(3, 1).await;
    let p = &farm.p;
    let manifest = farm.manifest(1, &[(1, large().1)]);
    let records: [&[u8]; 5] = [&p.alpha, &p.beta, &p.stdout, &p.tree.1, &p.in_dir];
    farm.put(farm.pack_key(0), &Multi::pack_zero(&manifest, &records))
        .await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::BadManifest).await;
    farm.nothing_indexed();
}

/// Asserts that a large object under its key is streamed into the CAS and the action is cached:
/// the result is as the manifest describes, the CAS holds the blob under its digest (the node
/// streams back bytes that hash to it, FindMissing says it is held), the frontend serves the
/// action-cache entry (which it does only when every blob the result names is held), and the counts
/// say one pack and the large bytes.
#[tokio::test]
async fn a_large_object_is_streamed_into_the_cas_and_the_run_is_cached() {
    let farm = Multi::new().await;
    let (manifest, zero) = farm.large_run();
    let digest = large().1;
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0).await;

    let got = farm.run().await;

    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(got, want);
    assert!(
        farm.backend
            .find_missing(&[digest])
            .await
            .expect("asked")
            .is_empty()
    );
    assert_eq!(streamed_by_node(&farm.rig, &digest).await, digest);
    assert_eq!(farm.cached().await.expect("a hit"), result_of(&manifest));
    let metrics = farm.rig.packs.metrics();
    assert_eq!(metrics.ingest_packs(), 1);
    assert_eq!(metrics.ingest_large_bytes(), LARGE_LEN as u64);
    let mut scrape = String::new();
    farm.rig.packs.render_metrics(&mut scrape);
    let line = format!("kbf_pack_ingest_large_bytes_total {LARGE_LEN}\n");
    assert!(scrape.contains(&line), "{scrape}");
}

/// Asserts that a large object is never held whole: the object store is read a window at a time
/// in order, one read per piece the node takes and none ahead of it, no window over 8 MiB and no
/// piece the node is given over it, though the object is over 64 MiB.
#[tokio::test]
async fn a_large_object_is_read_and_stored_a_window_at_a_time() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0).await;

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Ran { cached: true, .. }), "{got:?}");
    let key = farm.large_key(0).key().to_owned();
    let reads: Vec<(u64, u64)> = farm
        .rig
        .store
        .ranges()
        .into_iter()
        .filter(|(k, _, _)| *k == key)
        .map(|(_, offset, len)| (offset, len))
        .collect();
    let windows = (LARGE_LEN as u64).div_ceil(WINDOW);
    assert_eq!(reads.len() as u64, windows);
    for (i, (offset, len)) in reads.iter().enumerate() {
        assert_eq!(*offset, i as u64 * WINDOW, "windows in order");
        assert!(*len <= WINDOW, "a window of {len} bytes");
    }
    let notes = &farm.watching.notes;
    assert_eq!(notes.pieces.load(Ordering::SeqCst) as u64, windows);
    assert!(notes.largest.load(Ordering::SeqCst) as u64 <= WINDOW);
    assert_eq!(notes.ahead.load(Ordering::SeqCst), 0, "no read ahead");
    assert!(
        (WINDOW as usize) < LARGE_LEN / 4,
        "the bound is well below the object"
    );
}

/// Asserts that a large object whose bytes do not hash to its digest is a `HashMismatch`, nothing
/// is kept under that digest in the CAS, nothing is indexed and nothing is cached.
#[tokio::test]
async fn a_large_object_that_does_not_hash_is_a_hash_mismatch_and_not_cached() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    let mut wrong = large().0.to_vec();
    wrong[LARGE_LEN / 3] ^= 1;
    farm.put(farm.large_key(0), &wrong).await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::HashMismatch).await;
    let digest = large().1;
    let missing = farm.backend.find_missing(&[digest]).await.expect("asked");
    assert_eq!(missing, [digest], "nothing is kept under the digest");
    farm.nothing_indexed();
}

/// Asserts that a large object of another length than the manifest's is a `HashMismatch` found
/// by the length alone: no byte of it is read.
#[tokio::test]
async fn a_large_object_of_another_length_is_a_hash_mismatch_unread() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0[..LARGE_LEN - 1])
        .await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::HashMismatch).await;
    let key = farm.large_key(0).key();
    assert!(farm.rig.store.ranges().iter().all(|(k, _, _)| k != key));
    farm.nothing_indexed();
}

/// Asserts that an object that is shorter than its length said (the store answered a length that
/// the bytes do not reach) is a `HashMismatch` once the last window comes back short, and that
/// nothing is cached.
#[tokio::test]
async fn an_object_shorter_than_its_length_said_is_a_hash_mismatch() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0[..LARGE_LEN - 3])
        .await;
    farm.rig.store.overstate_len_of(farm.large_key(0).key(), 3);

    let got = farm.run().await;

    farm.rejected(got, RejectReason::HashMismatch).await;
    farm.nothing_indexed();
}

/// Asserts that a large object the store does not hold is `Incomplete`, nothing is indexed and
/// nothing is cached.
#[tokio::test]
async fn a_missing_large_object_is_incomplete() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::Incomplete).await;
    farm.nothing_indexed();
}

/// Asserts that a read of the large object that fails in the object store is `Unavailable`, counted
/// so and not as a refusal, and that nothing is indexed or cached.
#[tokio::test]
async fn a_large_object_the_store_cannot_read_is_unavailable() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0).await;
    farm.rig.store.reject_big_reads(1);

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Unavailable(_)), "{got:?}");
    let metrics = farm.rig.packs.metrics();
    assert_eq!(metrics.ingest_unavailable(), 1);
    assert_eq!(metrics.ingests(), 0);
    farm.nothing_indexed();
    let cached = farm.cached().await.expect_err("nothing cached");
    assert_eq!(cached.code(), Code::NotFound);
}

/// Asserts that a storage node that cannot take the streamed blob makes the ingest `Unavailable`
/// (the object is fine, the lease may be ingested again), not a refusal of the object.
#[tokio::test]
async fn a_node_that_cannot_take_the_large_blob_is_unavailable() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0).await;
    farm.watching.refuse.store(true, Ordering::SeqCst);

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Unavailable(_)), "{got:?}");
    assert_eq!(farm.rig.packs.metrics().ingest_unavailable(), 1);
    farm.nothing_indexed();
    let cached = farm.cached().await.expect_err("nothing cached");
    assert_eq!(cached.code(), Code::NotFound);
}

/// Asserts that a node that takes every byte of the large blob and then fails is `Unavailable`
/// (the bytes were the blob's, the CAS did not keep them: the lease may be ingested again), not a
/// refusal of the object, and nothing is indexed or cached.
#[tokio::test]
async fn a_node_that_fails_after_the_last_byte_is_unavailable() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0).await;
    farm.watching.refuse_after.store(true, Ordering::SeqCst);

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Unavailable(_)), "{got:?}");
    assert_eq!(farm.rig.packs.metrics().ingest_unavailable(), 1);
    farm.nothing_indexed();
    let cached = farm.cached().await.expect_err("nothing cached");
    assert_eq!(cached.code(), Code::NotFound);
}

/// Asserts that a length question about the large object that fails in the object store is
/// `Unavailable`, and no byte of the object is read.
#[tokio::test]
async fn a_failing_length_of_a_large_object_is_unavailable() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    farm.rig.store.reject_large_lens(1);

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Unavailable(_)), "{got:?}");
    assert_eq!(farm.rig.packs.metrics().ingest_unavailable(), 1);
    let key = farm.large_key(0).key();
    assert!(farm.rig.store.ranges().iter().all(|(k, _, _)| k != key));
    farm.nothing_indexed();
    let cached = farm.cached().await.expect_err("nothing cached");
    assert_eq!(cached.code(), Code::NotFound);
}

/// Asserts that a digest that is a record of two packs is `Malformed` and nothing is indexed: a
/// digest is in one place.
#[tokio::test]
async fn a_digest_in_two_packs_is_malformed() {
    let farm = Multi::new().await;
    let (_, zero, _) = farm.two_packs();
    let p = &farm.p;
    farm.put(farm.pack_key(0), &zero).await;
    let twice = segment_of(&[&p.alpha, &p.in_dir, &p.beta]);
    farm.put(farm.pack_key(1), &twice).await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::Malformed).await;
    farm.nothing_indexed();
}

/// Asserts that a tree that names a file no pack and no large object holds is `Incomplete`
/// though the tree is in pack 0 and its other file in pack 1, and nothing is indexed.
#[tokio::test]
async fn a_tree_naming_a_file_no_pack_holds_is_incomplete() {
    let farm = Multi::new().await;
    let p = &farm.p;
    let hollow = tree_of(&[sha256(&p.in_dir), blob("held nowhere").0]);
    let mut manifest = farm.manifest(2, &[]);
    manifest.output_directories[0].tree = hollow.0;
    let zero = Multi::pack_zero(&manifest, &[&hollow.1, &p.beta, &p.stdout]);
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.pack_key(1), &segment_of(&[&p.alpha, &p.in_dir]))
        .await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::Incomplete).await;
    farm.nothing_indexed();
}

/// Asserts that an output directory whose tree is a large object is `Incomplete` (a tree is read
/// from a record), found before the large object is read.
#[tokio::test]
async fn a_tree_that_is_a_large_object_is_incomplete_and_the_object_unread() {
    let farm = Multi::new().await;
    let p = &farm.p;
    let digest = large().1;
    let mut manifest = farm.manifest(1, &[(0, digest)]);
    manifest.output_directories[0].tree = digest;
    let zero = Multi::pack_zero(&manifest, &[&p.alpha, &p.beta, &p.stdout]);
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0).await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::Incomplete).await;
    let key = farm.large_key(0).key();
    assert!(farm.rig.store.ranges().iter().all(|(k, _, _)| k != key));
}

/// Asserts that the large cap is the limits': a manifest that names a large object over
/// `max_large_bytes`, or any when it is 0, is a `BadManifest`, and no large object is asked about.
#[tokio::test]
async fn a_large_object_over_the_limit_is_a_bad_manifest() {
    for cap in [0, LARGE_LEN as u64 - 1] {
        let farm = Multi::new().await;
        let (_, zero) = farm.large_run();
        farm.put(farm.pack_key(0), &zero).await;
        let limits = UntrustedLimits {
            max_large_bytes: cap,
            ..UntrustedLimits::default()
        };

        let got = farm.run_with(&farm.keys, &limits).await;

        farm.rejected(got, RejectReason::BadManifest).await;
        farm.nothing_indexed();
    }
}

/// Asserts that a pack 1 whose records do not lie back to back is `Malformed` by the rules of a
/// task pack, and nothing is indexed.
#[tokio::test]
async fn a_second_pack_that_is_not_laid_out_is_malformed() {
    let farm = Multi::new().await;
    let (_, zero, _) = farm.two_packs();
    let p = &farm.p;
    farm.put(farm.pack_key(0), &zero).await;
    let records = [&p.alpha[..], &p.in_dir[..]].concat();
    let gap = p.alpha.len() as u64 + 1;
    let entries = [entry_of(&p.alpha, 0), entry_of(&p.in_dir, gap)];
    farm.put(farm.pack_key(1), &forged(&records, &entries))
        .await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::Malformed).await;
    farm.nothing_indexed();
}

/// Asserts that a record of pack 1 that is not its digest is a `HashMismatch`, and that pack 0 is
/// not indexed and nothing is cached: pack 1 is committed before pack 0, so a run that fails
/// midway leaves no row for the pack that holds the manifest.
#[tokio::test]
async fn a_record_of_the_second_pack_that_is_not_its_digest_leaves_pack_zero_unrecorded() {
    let farm = Multi::new().await;
    let (_, zero, _) = farm.two_packs();
    let p = &farm.p;
    farm.put(farm.pack_key(0), &zero).await;
    let garbage = vec![0x5a; p.in_dir.len()];
    let records = [&p.alpha[..], &garbage[..]].concat();
    let entries = [
        entry_of(&p.alpha, 0),
        entry_of(&p.in_dir, p.alpha.len() as u64),
    ];
    farm.put(farm.pack_key(1), &forged(&records, &entries))
        .await;

    let got = farm.run().await;

    farm.rejected(got, RejectReason::HashMismatch).await;
    assert!(
        farm.rig
            .index
            .pack(farm.pack_key(0).key())
            .expect("asked")
            .is_none()
    );
}

/// Asserts that a key set that is not the one issued is `WrongKey` and the store is not asked for a
/// byte: a pack key of another lease, keys in the wrong order, a pack key in a large place, a key
/// whose slot was changed to another slot of the run or to one a run does not have, no pack key, more
/// keys than a run has slots.
#[tokio::test]
async fn a_key_set_that_is_not_the_one_issued_is_wrong_key() {
    let farm = Multi::new().await;
    let (_, zero, one) = farm.two_packs();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.pack_key(1), &one).await;
    let (packs, large) = (farm.keys.packs().to_vec(), farm.keys.large().to_vec());
    let foreign = "00000000000000010000000000000003";
    let foreign = farm
        .mint
        .mint_set(foreign, &farm.action, now(), 3, 2)
        .expect("keys");
    let changed = |key: &IssuedPackKey, slot: char| {
        let nonce = key.key().rsplit('/').next().expect("a nonce").to_owned();
        let moved = format!("{}{slot}", &nonce[..nonce.len() - 1]);
        IssuedPackKey::new(LEASE, &moved).expect("a key")
    };
    let sets = [
        vec![packs[0].clone(), foreign[1].clone(), packs[2].clone()],
        vec![packs[1].clone(), packs[0].clone(), packs[2].clone()],
        vec![packs[0].clone(), changed(&packs[1], '2'), packs[2].clone()],
        vec![packs[0].clone(), changed(&packs[1], '9'), packs[2].clone()],
        Vec::new(),
        vec![packs[0].clone(); 9],
    ];
    let mut cases: Vec<IssuedKeySet> = sets
        .into_iter()
        .map(|packs| IssuedKeySet::new(packs, large.clone()))
        .collect();
    cases.push(IssuedKeySet::new(packs.clone(), vec![packs[0].clone()]));
    cases.push(IssuedKeySet::new(packs.clone(), vec![large[0].clone(); 9]));
    let elsewhere = blob("an action this run is not").0;
    let minted = |count| farm.mint.mint_set(LEASE, &elsewhere, now(), 3, count);
    let other_action = IssuedKeySet::from_minted(minted(2).expect("keys"), 3);
    cases.push(other_action);

    for keys in &cases {
        let got = farm.run_with(keys, &UntrustedLimits::default()).await;
        assert_eq!(got, Ingested::Reject(RejectReason::WrongKey));
    }

    let counted = farm
        .rig
        .packs
        .metrics()
        .ingest_rejects(RejectReason::WrongKey);
    assert_eq!(counted, cases.len() as u64);
    assert!(farm.rig.store.ranges().is_empty(), "the store was not read");
    farm.nothing_indexed();
}

/// Asserts how a minted set is split: the first keys are the pack keys and the rest the large
/// ones, a count over the keys takes them all as pack keys, and one key is a set of one pack.
#[test]
fn a_minted_set_splits_into_pack_keys_then_large_keys() {
    let mint = PackKeyMint::new(SECRET).expect("a mint");
    let action = blob("an action").0;
    let keys = mint.mint_set(LEASE, &action, now(), 3, 2).expect("keys");

    let set = IssuedKeySet::from_minted(keys.clone(), 3);
    assert_eq!(set.packs(), &keys[..3]);
    assert_eq!(set.large(), &keys[3..]);
    let all = IssuedKeySet::from_minted(keys.clone(), 9);
    assert_eq!(all.packs(), &keys[..]);
    assert!(all.large().is_empty());
    let single = IssuedKeySet::single(keys[0].clone());
    assert_eq!(single.packs(), &keys[..1]);
    assert!(single.large().is_empty());
}

/// Asserts that a large entry is found by its slot number, not its place in the list: with six
/// large keys issued and the one large object in slot 5 (slots need not be contiguous or start at
/// 0), the object is read under the key of slot `L5` and the run is cached. Catches a lookup by
/// position, which would ask for `L0`, absent.
#[tokio::test]
async fn a_large_object_in_a_slot_that_is_not_zero_is_read_under_that_slot() {
    let farm = Multi::sized(3, 6).await;
    let p = &farm.p;
    let manifest = farm.manifest(1, &[(5, large().1)]);
    let records: [&[u8]; 5] = [&p.alpha, &p.beta, &p.stdout, &p.tree.1, &p.in_dir];
    farm.put(farm.pack_key(0), &Multi::pack_zero(&manifest, &records))
        .await;
    farm.put(farm.large_key(5), &large().0).await;

    let got = farm.run().await;

    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(got, want);
    let used = farm.large_key(5).key();
    let ranges = farm.rig.store.ranges();
    assert!(ranges.iter().any(|(k, _, _)| k == used));
    for slot in 0..5 {
        let key = farm.large_key(slot).key();
        assert!(ranges.iter().all(|(k, _, _)| k != key), "L{slot} was read");
    }
}

/// Asserts that the advisory `inputs` object of a manifest is accepted and ignored: the run is
/// ingested into the same result as without it.
#[tokio::test]
async fn the_advisory_inputs_of_a_manifest_are_accepted_and_ignored() {
    let farm = Multi::new().await;
    let p = &farm.p;
    let mut manifest = farm.manifest(1, &[]);
    manifest.inputs = Some(kbf_pack::InputStats {
        blobs: 7,
        ..kbf_pack::InputStats::default()
    });
    let records: [&[u8]; 5] = [&p.alpha, &p.beta, &p.stdout, &p.tree.1, &p.in_dir];
    farm.put(farm.pack_key(0), &Multi::pack_zero(&manifest, &records))
        .await;

    let got = farm.run().await;

    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(got, want);
}

/// Asserts that the MAC of every key of the set is checked, not only pack 0's: with the right
/// lease and the right slot, a pack 1 key or an `L0` key whose MAC is wrong is `WrongKey`, and
/// the store is not read. Pack 0's key is valid in both sets.
#[tokio::test]
async fn the_mac_of_a_key_that_is_not_pack_zeros_is_checked() {
    let farm = Multi::new().await;
    let (packs, large) = (farm.keys.packs().to_vec(), farm.keys.large().to_vec());
    let forged = |key: &IssuedPackKey| {
        let nonce = key.key().rsplit('/').next().expect("a nonce");
        let first = if nonce.starts_with('0') { '1' } else { '0' };
        let nonce = format!("{first}{}", &nonce[1..]);
        IssuedPackKey::new(LEASE, &nonce).expect("a key")
    };
    let mut bad_pack = packs.clone();
    bad_pack[1] = forged(&packs[1]);
    let mut bad_large = large.clone();
    bad_large[0] = forged(&large[0]);
    let sets = [
        IssuedKeySet::new(bad_pack, large.clone()),
        IssuedKeySet::new(packs.clone(), bad_large),
    ];

    for keys in &sets {
        let got = farm.run_with(keys, &UntrustedLimits::default()).await;
        assert_eq!(got, Ingested::Reject(RejectReason::WrongKey));
    }

    assert!(farm.rig.store.ranges().is_empty(), "the store was not read");
}
