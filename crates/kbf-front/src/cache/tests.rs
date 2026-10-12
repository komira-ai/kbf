use kbf_types::DigestFunction;

use super::*;

/// A blob whose digest claims `size` bytes: what [`VerifiedBlob`] can never be made
/// to hold here (its bytes are empty), so only the size check sees it.
fn claiming(size: u64) -> VerifiedBlob {
    VerifiedBlob {
        digest: Digest::new(DigestFunction::Sha256, [7; 32], size),
        bytes: Bytes::new(),
    }
}

/// Catches (mutants "`>=` for `>`" and "no check"): a blob of exactly
/// [`MAX_BLOB_BYTES`] refused, or one byte more allowed through. The error names the
/// blob and the limit.
#[test]
fn only_blobs_over_the_largest_are_refused() {
    assert!(refuse_oversized(&[claiming(MAX_BLOB_BYTES)]).is_ok());
    let over = claiming(MAX_BLOB_BYTES + 1);
    match refuse_oversized(&[claiming(3), over.clone()]) {
        Err(CacheError::Invalid(why)) => {
            assert!(why.contains(&over.digest.to_string()), "{why}");
            assert!(why.contains("(1073741824 bytes)"), "{why}");
        }
        other => panic!("{other:?}"),
    }
}

/// Catches (mutant "check not called"): `store_blobs` storing a batch that holds a
/// blob over [`MAX_BLOB_BYTES`], or storing the rest of that batch. The cache's
/// readers (the HTTP blob route's `404`) rely on it holding no such blob.
#[tokio::test]
async fn store_blobs_refuses_a_batch_with_a_blob_over_the_largest() {
    let cache = Cache::memory();
    let small = VerifiedBlob::hashed(Bytes::from_static(b"stored only with good company"));
    let digest = small.digest;
    let refused = cache
        .store_blobs(vec![small, claiming(MAX_BLOB_BYTES + 1)])
        .await;
    assert!(
        matches!(refused, Err(CacheError::Invalid(_))),
        "{refused:?}"
    );
    let missing = cache.find_missing(&[digest]).await.expect("find");
    assert_eq!(missing, [digest], "nothing of the batch is stored");
}
