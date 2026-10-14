//! [`Spool`]: an action's outputs gathered in one local file before they are stored.

use std::sync::Arc;

use futures::stream;
use kbf_proto::reapi::Digest;
use kbf_transfer::{Cas, CasError, Spool, digest_of, label};
use kbf_types::LeaseId;

use super::patterned;
use super::segments::scratch;

fn chunks(parts: Vec<Vec<u8>>) -> kbf_transfer::Chunks {
    Box::pin(stream::iter(parts.into_iter().map(Ok)))
}

/// Catches a blob written twice (two records of one digest would make a footer the
/// server refuses) and a blob read again when it is spooled already: a second put, by
/// bytes or by chunks, adds nothing and reads nothing.
#[tokio::test]
async fn each_blob_is_spooled_once() {
    let spool = Spool::create(&scratch("spool-once")).expect("spool");
    let bytes = patterned(1, 5000);
    let digest = spool.put(bytes.clone()).await.expect("put");
    assert_eq!(spool.put(bytes.clone()).await, Ok(digest.clone()));
    let unread: kbf_transfer::Chunks = Box::pin(stream::once(async {
        panic!("a spooled blob's chunks were read again")
    }));
    assert_eq!(
        spool.put_chunks(digest.clone(), unread).await,
        Ok(digest.clone())
    );
    assert_eq!(spool.records().len(), 1);
    assert_eq!(spool.get(&digest).await, Ok(bytes));
}

/// Catches chunks that do not hash to their digest recorded, and a failed blob that
/// can never be spooled again (mutant: the reservation kept): the bad put is
/// CORRUPT and records nothing, and the right bytes put later are recorded.
#[tokio::test]
async fn a_blob_whose_chunks_fail_their_digest_is_not_recorded() {
    let spool = Spool::create(&scratch("spool-corrupt")).expect("spool");
    let bytes = patterned(2, 3000);
    let digest = digest_of(&bytes);
    let mut wrong = bytes.clone();
    wrong[10] ^= 1;
    let refused = spool
        .put_chunks(
            digest.clone(),
            chunks(vec![wrong[..1000].to_vec(), wrong[1000..].to_vec()]),
        )
        .await;
    assert!(matches!(refused, Err(CasError::Corrupt(..))), "{refused:?}");
    let long = spool
        .put_chunks(digest.clone(), chunks(vec![bytes.clone(), vec![0]]))
        .await;
    assert!(matches!(long, Err(CasError::Corrupt(..))), "{long:?}");
    assert!(spool.records().is_empty());
    assert_eq!(
        spool.get(&digest).await,
        Err(CasError::Missing(label(&digest)))
    );
    let stored = spool
        .put_chunks(
            digest.clone(),
            chunks(vec![bytes[..7].to_vec(), bytes[7..].to_vec()]),
        )
        .await;
    assert_eq!(stored, Ok(digest.clone()));
    assert_eq!(spool.get(&digest).await, Ok(bytes));
}

/// Catches a spool that leaves its file behind: the directory holds nothing once the
/// spool exists, and a spool cannot be made where no file can be created.
#[tokio::test]
async fn the_spool_file_is_unlinked_at_once() {
    let dir = scratch("spool-unlinked");
    let spool = Spool::create(&dir).expect("spool");
    spool.put(patterned(3, 10)).await.expect("put");
    assert_eq!(std::fs::read_dir(&dir).expect("list").count(), 0);
    assert!(Spool::create(&dir.join("absent")).is_err());
}

/// Catches a store without segment upload that loses or alters a spooled blob: the
/// default `put_spool` stores every one with `put`, or `put_chunks` for a large one.
#[tokio::test]
async fn the_default_put_spool_stores_every_blob() {
    let spool = Arc::new(Spool::create(&scratch("spool-default-from")).expect("spool"));
    let mut blobs: Vec<Vec<u8>> = (0..30).map(|i| patterned(i, 100 + i * 50)).collect();
    // Over a megabyte: streamed through `put_chunks`, not read whole.
    blobs.push(patterned(31, (2 << 20) + 17));
    for blob in &blobs {
        spool.put(blob.clone()).await.expect("put");
    }
    let target = Arc::new(Spool::create(&scratch("spool-default-to")).expect("spool"));
    let lease = target.lease(&LeaseId::new(1, 1));
    lease
        .put_spool(Arc::clone(&spool))
        .await
        .expect("put spool");
    for blob in blobs {
        assert_eq!(target.get(&digest_of(&blob)).await, Ok(blob));
    }
}

/// Catches a failed spool write recorded or kept reserved (mutants: the reservation
/// kept after a failed write, so the blob counts as spooled; the write error ignored),
/// through the public API, so the engine's own build of the spool runs these paths:
/// once a put has reserved the spool's offsets up to `i64::MAX` (its chunks then fail
/// their digest, leaving those bytes dead), the next blob starts past the largest
/// offset a file can have. Its put, by bytes and then by chunks, fails each time with
/// a read error of the spool naming the blob, and nothing is recorded.
#[tokio::test]
async fn a_spool_write_past_the_largest_offset_records_nothing() {
    let spool = Spool::create(&scratch("spool-write-fails")).expect("spool");
    let huge = Digest {
        hash: digest_of(b"abc").hash,
        size_bytes: i64::MAX,
    };
    let refused = spool.put_chunks(huge, chunks(vec![b"abc".to_vec()])).await;
    assert!(matches!(refused, Err(CasError::Corrupt(..))), "{refused:?}");
    let bytes = patterned(4, 10);
    let digest = digest_of(&bytes);
    let by_bytes = spool.put(bytes.clone()).await;
    let by_chunks = spool.put_chunks(digest.clone(), chunks(vec![bytes])).await;
    for failed in [by_bytes, by_chunks] {
        let Err(CasError::Read(blob, why)) = &failed else {
            panic!("{failed:?}");
        };
        assert_eq!(blob, &label(&digest));
        assert!(why.starts_with("spool: "), "{failed:?}");
    }
    assert!(spool.records().is_empty());
    assert_eq!(
        spool.get(&digest).await,
        Err(CasError::Missing(label(&digest)))
    );
}

/// Catches a chunk that fails to be read taken as the blob's end, or its blob left
/// reserved (mutants: the chunk error skipped; the reservation kept): `put_chunks`
/// whose second chunk fails is a read error naming the blob with the chunk's error,
/// records nothing, and the blob's right chunks put later are recorded.
#[tokio::test]
async fn a_chunk_that_fails_to_be_read_records_nothing() {
    let spool = Spool::create(&scratch("spool-chunk-fails")).expect("spool");
    let bytes = patterned(6, 3000);
    let digest = digest_of(&bytes);
    let broken: kbf_transfer::Chunks = Box::pin(stream::iter([
        Ok(bytes[..1000].to_vec()),
        Err(std::io::Error::other("the file went away")),
    ]));
    assert_eq!(
        spool.put_chunks(digest.clone(), broken).await,
        Err(CasError::Read(
            label(&digest),
            "the file went away".to_owned()
        ))
    );
    assert!(spool.records().is_empty());
    let stored = spool
        .put_chunks(digest.clone(), chunks(vec![bytes.clone()]))
        .await;
    assert_eq!(stored, Ok(digest.clone()));
    assert_eq!(spool.get(&digest).await, Ok(bytes));
}

/// Catches a digest that is not SHA-256 answered otherwise than CORRUPT naming it, or
/// its chunks read, through the public API: `put_chunks` of one refuses it before
/// reading a chunk, and records nothing.
#[tokio::test]
async fn a_digest_that_is_not_sha256_is_refused_before_its_chunks() {
    let spool = Spool::create(&scratch("spool-not-sha256")).expect("spool");
    let digest = Digest {
        hash: "not a SHA-256 hash".to_owned(),
        size_bytes: 3,
    };
    let unread: kbf_transfer::Chunks = Box::pin(stream::once(async {
        panic!("the chunks of a refused digest were read")
    }));
    assert_eq!(
        spool.put_chunks(digest.clone(), unread).await,
        Err(CasError::Corrupt(
            label(&digest),
            "not a SHA-256 digest".to_owned()
        ))
    );
    assert!(spool.records().is_empty());
}
