//! Segments `Locate` and the Held set (`docs/design/storage-ram-ack.md`): a blob held in
//! RAM has no object location to give. The other readers are in `held.rs`.

#[path = "segments/support.rs"]
#[allow(
    dead_code,
    reason = "shared with segments.rs, which uses the rest of it"
)]
mod support;

use bytes::Bytes;
use kbf_front::{SegmentsConfig, VerifiedBlob};
use support::{Front, blob};

/// Catches `Locate` giving a location for a blob the node holds in RAM (its caller would
/// read a range of an object the held copy is not in yet, or one the drain is about to
/// make): a blob held and also indexed (the drain's window between the index taking it and
/// the set dropping it) and one only held are both located nowhere, so the caller reads
/// each whole; a blob only indexed is located, and an absent one is not. Mutant: `locate`
/// answers from the index alone (the held-and-indexed blob is then located).
#[tokio::test]
async fn a_held_blob_has_no_location() {
    let front = Front::start(SegmentsConfig::DEFAULT).await;
    let verified = |seed| VerifiedBlob::hashed(Bytes::from(blob(seed, 300)));
    let (both, only_held, only_indexed) = (verified(1), verified(2), verified(3));
    front
        .cache
        .store_blobs(vec![both.clone(), only_indexed.clone()])
        .await
        .expect("stored");
    assert!(front.cache.hold_for_test(&both));
    assert!(front.cache.hold_for_test(&only_held));
    let absent = kbf_segments::sha256(b"nowhere");

    let mut client = front.client().await;
    let asked = [
        both.digest(),
        only_held.digest(),
        only_indexed.digest(),
        absent,
    ];
    let located: Vec<bool> = front
        .locate(&mut client, &asked)
        .await
        .iter()
        .map(|l| l.object.is_some())
        .collect();
    assert_eq!(located, [false, false, true, false]);

    // Once the set drops the blob the index's location is given, as before the set existed.
    assert!(front.cache.release_for_test(&both.digest()));
    let after = front.locate(&mut client, &[both.digest()]).await;
    assert!(after[0].object.is_some());
}
