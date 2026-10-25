//! The `kbf.worker.v1.PackIndex` service over a pack index (`kbf_server::pack_locate`), called
//! without a listener: what it answers for blobs the index has and has not, the bound on a call,
//! the digests it refuses, an index that fails, and that a call is admitted before it is
//! answered. The service on a started frontend's worker listener, under mutual TLS and the
//! role table, is `tests/frontend/pack_locate.rs`.

use std::sync::Arc;
use std::time::Duration;

use kbf_proto::reapi::Digest;
use kbf_proto::worker::pack_index_server::PackIndex as _;
use kbf_proto::worker::{PackLocateRequest, PackLocation};
use kbf_server::Peers;
use kbf_server::pack_locate::{PackLocateService, service};
use kbf_state::pack_index::memory::MemoryPackIndex;
use kbf_state::{
    BlobDigest, Location, MAX_BATCH, ManualClock, NewPack, PackBlob, PackIndex, PackIndexError,
    PackInfo, Recorded,
};
use tonic::{Code, Request};

const PACK: &str = "cas/d/node1/boot1/0000000000000001";

/// The digest of a blob of `size` bytes whose hash is `byte` repeated, in both spellings.
fn digest(byte: u8, size: u64) -> (BlobDigest, Digest) {
    let blob = BlobDigest {
        hash: [byte; 32],
        size,
    };
    let wire = Digest {
        hash: hex::encode(blob.hash),
        size_bytes: i64::try_from(size).expect("a small size"),
    };
    (blob, wire)
}

/// An index that holds one pack of two blobs: `digest(1, 10)` at offset 0 and `digest(2, 20)` at
/// offset 10.
fn index() -> Arc<dyn PackIndex> {
    let index = MemoryPackIndex::new(Arc::new(ManualClock::default()));
    let pack = NewPack {
        key: PACK.to_owned(),
        writer: "node1".to_owned(),
        size: 100,
        blobs: vec![
            PackBlob {
                digest: digest(1, 10).0,
                offset: 0,
            },
            PackBlob {
                digest: digest(2, 20).0,
                offset: 10,
            },
        ],
    };
    index.record_pack(&pack).expect("recorded");
    Arc::new(index)
}

/// A service over `index` that admits no one: it is not asked who calls in `answer`.
fn serving(index: Arc<dyn PackIndex>) -> PackLocateService {
    PackLocateService::new(index, Peers::Unauthenticated)
}

fn located(offset: u64, length: u64) -> PackLocation {
    PackLocation {
        located: true,
        pack: PACK.to_owned(),
        offset,
        length,
    }
}

/// An index whose every call fails.
struct Broken;

impl PackIndex for Broken {
    fn record_pack(&self, _: &NewPack) -> Result<Recorded, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn pack(&self, _: &str) -> Result<Option<PackInfo>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn locate(&self, _: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn find_missing(&self, _: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn touch(&self, _: &[BlobDigest]) -> Result<usize, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn dead_packs(&self, _: Duration, _: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn unreferenced_keys(&self, _: &[String]) -> Result<Vec<String>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }
}

/// An index whose `locate` panics.
struct Panics;

impl PackIndex for Panics {
    fn record_pack(&self, _: &NewPack) -> Result<Recorded, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn pack(&self, _: &str) -> Result<Option<PackInfo>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn locate(&self, _: &[BlobDigest]) -> Result<Vec<Option<Location>>, PackIndexError> {
        panic!("the index is broken")
    }

    fn find_missing(&self, _: &[BlobDigest]) -> Result<Vec<BlobDigest>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn touch(&self, _: &[BlobDigest]) -> Result<usize, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn dead_packs(&self, _: Duration, _: usize) -> Result<Vec<PackInfo>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }

    fn unreferenced_keys(&self, _: &[String]) -> Result<Vec<String>, PackIndexError> {
        Err(PackIndexError::NotMigrated)
    }
}

/// Asserts that each digest is answered in the order asked: a blob of the index with its pack, its
/// offset and its length, one the index does not have as not located (with the other fields
/// empty), and a digest named twice twice.
#[tokio::test]
async fn each_blob_is_answered_where_it_is_in_the_order_asked() {
    let asked = [
        digest(2, 20).1,
        digest(9, 5).1,
        digest(1, 10).1,
        digest(2, 20).1,
    ];
    let answer = serving(index()).answer(&asked).await.expect("answered");
    let none = PackLocation::default();
    assert_eq!(
        answer.locations,
        [located(10, 20), none, located(0, 10), located(10, 20)]
    );
    assert!(!answer.locations[1].located);
    assert!(answer.locations[1].pack.is_empty());
}

/// Asserts that a blob of the same hash and another size is a different blob: the index names a
/// blob by both.
#[tokio::test]
async fn a_blob_is_named_by_its_hash_and_its_size() {
    let asked = [digest(1, 11).1, digest(1, 10).1];
    let answer = serving(index()).answer(&asked).await.expect("answered");
    assert_eq!(answer.locations, [PackLocation::default(), located(0, 10)]);
}

/// Asserts that a call that names no digest is answered with none.
#[tokio::test]
async fn a_call_with_no_digest_is_answered_with_none() {
    let answer = serving(index()).answer(&[]).await.expect("answered");
    assert!(answer.locations.is_empty());
}

/// Asserts the bound on a call: the index's own batch is answered, and one digest more is
/// INVALID_ARGUMENT, saying how many were named and the bound.
#[tokio::test]
async fn a_call_names_at_most_the_indexs_batch() {
    let service = serving(index());
    let (_, wire) = digest(7, 1);
    let batch = vec![wire; MAX_BATCH];
    let answer = service.answer(&batch).await.expect("a full batch");
    assert_eq!(answer.locations.len(), MAX_BATCH);

    let (_, wire) = digest(7, 1);
    let over = vec![wire; MAX_BATCH + 1];
    let refused = service.answer(&over).await.expect_err("one too many");
    assert_eq!(refused.code(), Code::InvalidArgument);
    assert_eq!(refused.message(), "1001 digests in one call, at most 1000");
}

/// Asserts that a digest that is not a SHA-256 one is INVALID_ARGUMENT, naming it, whichever way
/// it is wrong: a hash that is not hex, one of another length, a negative size.
#[tokio::test]
async fn a_digest_that_is_not_sha256_is_invalid_argument() {
    let service = serving(index());
    let wrong = |hash: &str, size_bytes: i64| Digest {
        hash: hash.to_owned(),
        size_bytes,
    };
    let not_hex = "zz".repeat(32);
    let short = "ab".repeat(31);
    for bad in [
        wrong(&not_hex, 1),
        wrong(&short, 1),
        wrong("", 1),
        wrong(&"ab".repeat(32), -1),
    ] {
        let named = format!("{}/{} is not a SHA-256 digest", bad.hash, bad.size_bytes);
        let (_, good) = digest(1, 10);
        let refused = service.answer(&[good, bad]).await.expect_err("refused");
        assert_eq!(refused.code(), Code::InvalidArgument, "{named}");
        assert_eq!(refused.message(), named);
    }
}

/// Asserts that an index that fails is UNAVAILABLE with a fixed text, which tells a daemon to read
/// as it did before and shows it nothing of the index's own error.
#[tokio::test]
async fn an_index_that_fails_is_unavailable_with_a_fixed_text() {
    let service = serving(Arc::new(Broken));
    let refused = service
        .answer(&[digest(1, 10).1])
        .await
        .expect_err("the index fails");
    assert_eq!(refused.code(), Code::Unavailable);
    assert_eq!(refused.message(), "the pack index could not be read");
}

/// Asserts that an index whose call panics is INTERNAL with a fixed text, and that the call
/// returns instead of panicking in the handler.
#[tokio::test]
async fn an_index_that_panics_is_internal_with_a_fixed_text() {
    let service = serving(Arc::new(Panics));
    let refused = service
        .answer(&[digest(1, 10).1])
        .await
        .expect_err("the index panics");
    assert_eq!(refused.code(), Code::Internal);
    assert_eq!(refused.message(), "the pack index could not be read");
}

/// Asserts that a call is admitted before it is answered: with no mutual TLS, or with it and no
/// certificate, a call for a blob the index has is refused UNAUTHENTICATED and nothing is
/// answered, as the listener's blob calls are.
#[tokio::test]
async fn a_call_is_admitted_before_it_is_answered() {
    let certified = Peers::Certified {
        deny_list: None,
        denied_rows: None,
    };
    for peers in [Peers::Unauthenticated, certified] {
        let service = PackLocateService::new(index(), peers);
        let request = PackLocateRequest {
            digests: vec![digest(1, 10).1],
        };
        let refused = service
            .locate(Request::new(request))
            .await
            .expect_err("not admitted");
        assert_eq!(refused.code(), Code::Unauthenticated);
    }
}

/// Asserts that the listener registers the service only when it was given an index: with none,
/// no service is built (a daemon's call is then UNIMPLEMENTED), and with one it is.
#[test]
fn the_service_is_built_only_over_an_index() {
    assert!(service(None, &Peers::Unauthenticated).is_none());
    assert!(service(Some(&index()), &Peers::Unauthenticated).is_some());
}
