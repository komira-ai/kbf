//! The probes of what a key pair reaches: each answer a store can give, sorted.

mod ops_support;

use tierstore_bench::config::{Secret, StoreSpec};
use tierstore_bench::error::BenchError;
use tierstore_bench::provision::probe::{Access, BucketState, bucket_state, can_list, can_read};

use ops_support::*;

fn spec(endpoint: &str) -> StoreSpec {
    StoreSpec {
        endpoint: endpoint.to_owned(),
        bucket: "probe-bucket-mark".to_owned(),
        region: "us-east-1".to_owned(),
        access_key_id: "KEYID-MARK-probe".to_owned(),
        secret_access_key: Secret::new("SECRET-MARK-probe"),
    }
}

/// Catches an answer sorted into the wrong class: a served listing and a missing object are both
/// "reached", a missing bucket, an unknown key and a refusal are each their own, and any other
/// answer (another status, a body that is no listing) is "failed" with a class and no text of the
/// store's.
#[tokio::test]
async fn each_answer_of_a_store_is_its_own_access() {
    let cases: [(u16, String, Access); 6] = [
        (200, empty_listing().1, Access::Reached),
        (404, s3_error(404, "NoSuchBucket").1, Access::NoSuchBucket),
        (
            403,
            s3_error(403, "InvalidAccessKeyId").1,
            Access::UnknownKey,
        ),
        (
            403,
            s3_error(403, "AccessDenied").1,
            Access::Denied("AccessDenied".to_owned()),
        ),
        (
            404,
            s3_error(404, "NoSuchKey").1,
            Access::Failed("status 404 NoSuchKey".to_owned()),
        ),
        (
            200,
            "garbage".to_owned(),
            Access::Failed("protocol".to_owned()),
        ),
    ];
    for (status, body, want) in cases {
        let stub = Stub::start(move |_| (status, body.clone())).await;
        assert_eq!(
            can_list(&spec(&stub.endpoint)).await.unwrap(),
            want,
            "{status}"
        );
    }
}

/// Catches a read probe that reads the wrong bucket or key, or counts a missing key as a refusal:
/// the probe asks the named bucket, not the account's own, for one byte of a key that is not
/// there, and a 404 on that key is a read that was allowed.
#[tokio::test]
async fn the_read_probe_asks_the_named_bucket_for_one_byte_of_an_absent_key() {
    let stub = Stub::start(|_| s3_error(404, "NoSuchKey")).await;
    assert_eq!(
        can_read(&spec(&stub.endpoint), "other-bucket-mark")
            .await
            .unwrap(),
        Access::Reached
    );
    let seen = stub.requests();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].path.starts_with("/other-bucket-mark/"),
        "{:?}",
        seen[0]
    );
    assert!(
        seen[0].head.to_lowercase().contains("range: bytes=0-0"),
        "{:?}",
        seen[0]
    );

    let denied = Stub::start(|_| s3_error(403, "AccessDenied")).await;
    assert_eq!(
        can_read(&spec(&denied.endpoint), "other-bucket-mark")
            .await
            .unwrap(),
        Access::Denied("AccessDenied".to_owned())
    );
}

/// Catches an endpoint or a bucket name the client refuses reaching the network, and a refusal
/// that is not an error: each probe returns the store-configuration error.
#[tokio::test]
async fn a_store_the_client_refuses_is_an_error_before_any_request() {
    let bad = spec("ftp://host-mark");
    assert!(matches!(can_list(&bad).await, Err(BenchError::Store(_))));
    assert!(matches!(
        can_read(&bad, "other-bucket-mark").await,
        Err(BenchError::Store(_))
    ));
    assert!(matches!(
        bucket_state(&bad).await,
        Err(BenchError::Store(_))
    ));
}

/// Catches a bucket state read from the wrong listing: gone from NoSuchBucket, counts from both
/// listings, and unreadable (with the status, or the class when there is no status) from a refusal
/// in either listing and from a listing that is no listing.
#[tokio::test]
async fn the_state_of_a_bucket_is_read_from_both_listings() {
    let gone = Stub::start(|_| s3_error(404, "NoSuchBucket")).await;
    assert_eq!(
        bucket_state(&spec(&gone.endpoint)).await.unwrap(),
        BucketState::Gone
    );

    let there = Stub::start(|seen| {
        if is_uploads(seen) {
            uploads_of(2)
        } else {
            listing_of(3)
        }
    })
    .await;
    assert_eq!(
        bucket_state(&spec(&there.endpoint)).await.unwrap(),
        BucketState::Present {
            objects: 3,
            uploads: 2
        }
    );

    let refused = Stub::start(|_| s3_error(403, "AccessDenied")).await;
    assert_eq!(
        bucket_state(&spec(&refused.endpoint)).await.unwrap(),
        BucketState::Unreadable("status 403 AccessDenied".to_owned())
    );

    let garbage = Stub::start(|_| (200, "garbage".to_owned())).await;
    assert_eq!(
        bucket_state(&spec(&garbage.endpoint)).await.unwrap(),
        BucketState::Unreadable("protocol".to_owned())
    );

    let uploads_garbage = Stub::start(|seen| {
        if is_uploads(seen) {
            (200, "garbage".to_owned())
        } else {
            empty_listing()
        }
    })
    .await;
    assert_eq!(
        bucket_state(&spec(&uploads_garbage.endpoint))
            .await
            .unwrap(),
        BucketState::Unreadable("protocol".to_owned())
    );
}
