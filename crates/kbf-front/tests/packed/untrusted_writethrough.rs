//! `ingest_set` writes each large object of a run to the object store at `cas/l/<sha256>` as it
//! streams it into the CAS: one read of the task's object, two writes. The write is best effort.
//!
//! The fixtures are in `multi`. Every test says the behaviour it asserts.

use kbf_front::{Ingested, LargeWriteThrough};

use crate::multi::{LARGE_LEN, Multi, WINDOW, large, result_of};

/// The key a large blob is written to: `cas/l/` and the blob's SHA-256 in lowercase hex.
fn key_of(digest: &kbf_types::Digest) -> String {
    let hex: String = digest.hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("cas/l/{hex}")
}

async fn farm_with_large() -> Multi {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    farm.put(farm.large_key(0), &large().0).await;
    farm
}

fn count(farm: &Multi, how: LargeWriteThrough) -> u64 {
    farm.rig.packs.metrics().ingest_large_writethrough(how)
}

/// Asserts that a run with a large object leaves `cas/l/<sha256>` holding exactly the object's
/// bytes, counts one `ok` and no other ending, the ingest is cached, and the scrape names the
/// family with its three results.
#[tokio::test]
async fn a_large_object_is_written_to_its_content_bound_key() {
    let farm = farm_with_large().await;
    let (_, digest) = large();

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Ran { cached: true, .. }), "{got:?}");
    let held = farm.rig.store.held(&key_of(digest)).await.expect("written");
    assert_eq!(held.len(), LARGE_LEN);
    assert_eq!(held, large().0, "exactly the object's bytes");
    assert_eq!(count(&farm, LargeWriteThrough::Ok), 1);
    assert_eq!(count(&farm, LargeWriteThrough::Exists), 0);
    assert_eq!(count(&farm, LargeWriteThrough::Failed), 0);
    let mut scrape = String::new();
    farm.rig.packs.render_metrics(&mut scrape);
    for line in [
        "kbf_pack_ingest_large_writethrough_total{result=\"ok\"} 1\n",
        "kbf_pack_ingest_large_writethrough_total{result=\"exists\"} 0\n",
        "kbf_pack_ingest_large_writethrough_total{result=\"failed\"} 0\n",
    ] {
        assert!(scrape.contains(line), "{scrape}");
    }
}

/// Asserts that a key already held (the store's create-only refusal) is success: the ingest
/// succeeds and caches, `exists` is counted, `failed` is not, and the object under the key is the
/// one that was there.
#[tokio::test]
async fn a_create_only_refusal_of_the_key_is_exists_and_the_ingest_succeeds() {
    let farm = farm_with_large().await;
    let (bytes, digest) = large();
    farm.rig.store.seed(&key_of(digest), bytes).await;

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Ran { cached: true, .. }), "{got:?}");
    assert_eq!(count(&farm, LargeWriteThrough::Exists), 1);
    assert_eq!(count(&farm, LargeWriteThrough::Ok), 0);
    assert_eq!(count(&farm, LargeWriteThrough::Failed), 0);
    let held = farm.rig.store.held(&key_of(digest)).await;
    assert_eq!(held.as_ref(), Some(bytes));
}

/// Asserts that a write the store fails still ingests the run, with the result the manifest
/// describes and the action cached and the blob in the CAS: `failed` is counted, nothing is under
/// the key, and the ingest counts as an ingest.
#[tokio::test]
async fn a_failing_write_through_still_ingests_and_caches_the_action() {
    let farm = farm_with_large().await;
    let (_, digest) = large();
    farm.rig.store.reject_puts(1);

    let got = farm.run().await;

    let (manifest, _) = farm.large_run();
    let want = Ingested::Ran {
        result: Box::new(result_of(&manifest)),
        cached: true,
    };
    assert_eq!(got, want);
    assert_eq!(farm.cached().await.expect("a hit"), result_of(&manifest));
    let missing = farm.backend.find_missing(&[*digest]).await.expect("asked");
    assert!(missing.is_empty());
    assert_eq!(count(&farm, LargeWriteThrough::Failed), 1);
    assert_eq!(count(&farm, LargeWriteThrough::Ok), 0);
    assert!(farm.rig.store.held(&key_of(digest)).await.is_none());
    assert_eq!(farm.rig.packs.metrics().ingests(), 1);
}

/// Asserts that the object is never held whole on the way to the key: the store is handed the body
/// in pieces no larger than a read window, though the object is over 64 MiB, and the object store
/// is read once, a window at a time.
#[tokio::test]
async fn the_write_through_is_streamed_in_windows_from_the_one_read() {
    let farm = farm_with_large().await;

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Ran { .. }), "{got:?}");
    let peak = farm.rig.store.largest_piece();
    assert!(peak > 0, "the write was streamed");
    assert!(peak <= WINDOW, "a piece of {peak} bytes");
    assert!(WINDOW < LARGE_LEN as u64 / 4);
    let key = farm.large_key(0).key().to_owned();
    let ranges = farm.rig.store.ranges();
    let reads = ranges.iter().filter(|r| r.0 == key).count();
    assert_eq!(
        reads as u64,
        (LARGE_LEN as u64).div_ceil(WINDOW),
        "one read"
    );
}

/// Asserts that bytes that do not hash to the digest never land under `cas/l/<sha256>`: the ingest
/// is refused, the put is abandoned before the object is whole (counted `failed`), and the store
/// holds nothing under `cas/l/`, though all but the last window had been sent.
#[tokio::test]
async fn bytes_that_do_not_hash_are_not_left_under_the_key() {
    let farm = Multi::new().await;
    let (_, zero) = farm.large_run();
    farm.put(farm.pack_key(0), &zero).await;
    let mut wrong = large().0.to_vec();
    wrong[LARGE_LEN - 1] ^= 1;
    farm.put(farm.large_key(0), &wrong).await;

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Reject(_)), "{got:?}");
    let (_, digest) = large();
    assert!(farm.rig.store.held(&key_of(digest)).await.is_none());
    assert_eq!(count(&farm, LargeWriteThrough::Ok), 0);
    assert_eq!(count(&farm, LargeWriteThrough::Failed), 1);
    let listed = farm.rig.store.keys().await;
    assert!(
        listed.iter().all(|k| !k.starts_with("cas/l/")),
        "{listed:?}"
    );
}

/// Asserts that a run with no large object writes nothing under `cas/l/` and counts no
/// write-through.
#[tokio::test]
async fn a_run_without_a_large_object_writes_nothing_under_the_large_prefix() {
    let farm = Multi::new().await;
    let manifest = farm.manifest(1, &[]);
    let p = &farm.p;
    let records: [&[u8]; 5] = [&p.alpha, &p.beta, &p.stdout, &p.tree.1, &p.in_dir];
    let zero = Multi::pack_zero(&manifest, &records);
    farm.put(farm.pack_key(0), &zero).await;

    let got = farm.run().await;

    assert!(matches!(got, Ingested::Ran { .. }), "{got:?}");
    for how in LargeWriteThrough::ALL {
        assert_eq!(count(&farm, how), 0);
    }
    let listed = farm.rig.store.keys().await;
    assert!(listed.iter().all(|k| !k.starts_with("cas/l/")));
}
