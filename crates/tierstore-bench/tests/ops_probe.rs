//! The probes of what a key pair reaches: each answer a store can give, sorted.

mod ops_support;

use tierstore_bench::config::{Secret, StoreSpec};
use tierstore_bench::error::BenchError;
use tierstore_bench::provision::probe::{
    Access, BucketState, Reading, bucket_state, can_list, can_read,
};

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

/// The answer to `ListBuckets` naming these buckets.
fn buckets(names: &[&str]) -> (u16, String) {
    let items: String = names
        .iter()
        .map(|n| format!("<Bucket><Name>{n}</Name></Bucket>"))
        .collect();
    (
        200,
        format!("<ListAllMyBucketsResult><Buckets>{items}</Buckets></ListAllMyBucketsResult>"),
    )
}

/// A store whose account listing is `list` and whose bucket answers `bucket` (listings by kind).
fn world(
    list: (u16, String),
    objects: (u16, String),
    uploads: (u16, String),
) -> impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static {
    move |seen| {
        if seen.path == "/" {
            list.clone()
        } else if is_uploads(seen) {
            uploads.clone()
        } else {
            objects.clone()
        }
    }
}

/// Catches a bucket state read from the wrong place: whether the bucket exists is the account
/// listing's answer alone (named, not named, refused, or no listing at all), and the two
/// listings of the bucket are read on their own, each as a count, `NoSuchBucket` or a failure
/// (the status, or the class when there is no status).
#[tokio::test]
async fn the_state_of_a_bucket_is_the_account_listing_and_both_bucket_listings() {
    let gone = Stub::start(world(
        buckets(&["other-bucket-mark"]),
        s3_error(404, "NoSuchBucket"),
        s3_error(404, "NoSuchBucket"),
    ))
    .await;
    assert_eq!(
        bucket_state(&spec(&gone.endpoint)).await.unwrap(),
        BucketState {
            listed: Ok(false),
            objects: Reading::NoSuchBucket,
            uploads: Reading::NoSuchBucket,
        }
    );
    let signed = gone.requests();
    assert_eq!(signed[0].path, "/");
    assert_eq!(signed[0].query, "");
    assert_eq!(signed[0].key_id(), "KEYID-MARK-probe");

    let there = Stub::start(world(
        buckets(&["other-bucket-mark", "probe-bucket-mark"]),
        listing_of(3),
        uploads_of(2),
    ))
    .await;
    assert_eq!(
        bucket_state(&spec(&there.endpoint)).await.unwrap(),
        BucketState {
            listed: Ok(true),
            objects: Reading::Count(3),
            uploads: Reading::Count(2),
        }
    );

    let refused = Stub::start(|_| s3_error(403, "AccessDenied")).await;
    let denied = || Reading::Failed("status 403 AccessDenied".to_owned());
    assert_eq!(
        bucket_state(&spec(&refused.endpoint)).await.unwrap(),
        BucketState {
            listed: Err("status 403 AccessDenied".to_owned()),
            objects: denied(),
            uploads: denied(),
        }
    );

    let garbage = Stub::start(|_| (200, "garbage".to_owned())).await;
    assert_eq!(
        bucket_state(&spec(&garbage.endpoint)).await.unwrap(),
        BucketState {
            listed: Err("the answer is not a bucket list".to_owned()),
            objects: Reading::Failed("protocol".to_owned()),
            uploads: Reading::Failed("protocol".to_owned()),
        }
    );

    // The objects listing refused and the uploads listing served: the uploads are still read.
    let split = Stub::start(world(
        buckets(&["probe-bucket-mark"]),
        s3_error(403, "AccessDenied"),
        uploads_of(0),
    ))
    .await;
    assert_eq!(
        bucket_state(&spec(&split.endpoint)).await.unwrap(),
        BucketState {
            listed: Ok(true),
            objects: denied(),
            uploads: Reading::Count(0),
        }
    );

    // A list cut off before its end names nothing: it is no list, not a list without the bucket.
    let cut = Stub::start(|_| {
        (
            200,
            "<ListAllMyBucketsResult><Buckets><Bucket><Name>other-bucket-mark</Name>".to_owned(),
        )
    })
    .await;
    assert_eq!(
        bucket_state(&spec(&cut.endpoint)).await.unwrap().listed,
        Err("the answer is not a bucket list".to_owned())
    );

    // A refusal that carries no code, and a store that is not there.
    let bare = Stub::start(|_| (503, String::new())).await;
    assert_eq!(
        bucket_state(&spec(&bare.endpoint)).await.unwrap().listed,
        Err("status 503".to_owned())
    );
    let dead = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    assert_eq!(
        bucket_state(&spec(&dead)).await.unwrap().listed,
        Err("transport".to_owned())
    );

    // An empty account has no `Buckets` entries at all.
    let empty = Stub::start(world(
        (
            200,
            "<ListAllMyBucketsResult><Buckets/></ListAllMyBucketsResult>".to_owned(),
        ),
        s3_error(404, "NoSuchBucket"),
        s3_error(404, "NoSuchBucket"),
    ))
    .await;
    assert_eq!(
        bucket_state(&spec(&empty.endpoint)).await.unwrap().listed,
        Ok(false)
    );
}
