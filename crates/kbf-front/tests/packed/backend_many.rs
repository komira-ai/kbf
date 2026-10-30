//! The blobs a job-facing batch read reads through [`Backend::read_many`]: the ones in packs
//! coalesce into few ranged reads of the object store, the others are read from the node.
//! Each test says the behaviour it asserts.

use bytes::Bytes;
use kbf_front::Backend;
use kbf_storage::StorageError;
use kbf_types::{Digest, DigestFunction};

use crate::support::{Rig, blob, update};

/// Asserts that blobs of one pack are read together: three packed blobs and one only the node
/// holds are answered with their own bytes in the order asked, the three are one ranged request
/// of the store, and the blob that no one holds is `NotFound` and spoils none of the others.
#[tokio::test]
async fn packed_blobs_are_one_ranged_read_and_the_rest_come_from_the_node() {
    let rig = Rig::new().await;
    let packed = [
        blob("first of three"),
        blob("second of three"),
        blob("third of three"),
    ];
    update(&rig.front, &packed).await;
    let on_node = blob("only on the node");
    rig.node().put(on_node.1.clone()).await.expect("stored");
    let nowhere = Digest::new(DigestFunction::Sha256, [3; 32], 9);
    let backend = Backend::remote_over(rig.node().clone(), Some(rig.packs.clone()));

    let asked = [packed[2].0, on_node.0, packed[0].0, nowhere, packed[1].0];
    let got = backend.read_many(&asked).await;

    assert_eq!(got.len(), asked.len());
    let bytes = |at: usize| -> Bytes { got[at].as_ref().expect("held").clone() };
    assert_eq!(bytes(0), packed[2].1);
    assert_eq!(bytes(1), on_node.1);
    assert_eq!(bytes(2), packed[0].1);
    assert!(
        matches!(got[3], Err(StorageError::NotFound(_))),
        "{:?}",
        got[3]
    );
    assert_eq!(bytes(4), packed[1].1);
    let ranges = rig.store.ranges();
    assert_eq!(ranges.len(), 1, "{ranges:?}");
}
