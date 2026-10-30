//! The S3 client wrapper against a loopback S3 service: what it times, what it logs and what it
//! refuses before a request is sent.

use std::fs;
use std::sync::Arc;

use bytes::Bytes;
use kbf_objstore::s3::fake::FakeS3;
use kbf_testkit::dir::TestDir;
use tierstore_bench::config::{Secret, StoreSpec};
use tierstore_bench::error::BenchError;
use tierstore_bench::requestlog::{RequestLog, RequestRecord};
use tierstore_bench::role::Role;
use tierstore_bench::s3::{BenchS3, Timeouts};

fn spec(endpoint: &str) -> StoreSpec {
    StoreSpec {
        endpoint: endpoint.to_owned(),
        bucket: "kbf-fake".to_owned(),
        region: "us-east-1".to_owned(),
        access_key_id: "fake-key".to_owned(),
        secret_access_key: Secret::new("fake-secret"),
    }
}

fn records(path: &std::path::Path) -> Vec<RequestRecord> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Catches a call that does not reach the store as the method it logs, a status the store did
/// not give, or a call that leaves no record: one request, one line.
#[tokio::test]
async fn each_call_is_timed_answered_and_logged_once() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-s3");
    let log_path = dir.path().join("requests.jsonl");
    let log = Arc::new(RequestLog::create(&log_path).unwrap());
    let fake = FakeS3::start().await;
    let s3 = BenchS3::new(
        Role::Instance,
        &spec(fake.endpoint()),
        Timeouts::default(),
        log.clone(),
    )
    .unwrap();
    assert_eq!(s3.role(), Role::Instance);

    let put = s3
        .put("bench/a", Bytes::from(vec![7u8; 4096]))
        .await
        .unwrap();
    assert!(put.is_ok());
    assert_eq!(put.status, Some(200));
    assert!(put.started_unix_ms > 0);
    assert_eq!(
        put.latency_us(),
        u64::try_from(put.latency.as_micros()).unwrap()
    );

    let again = s3.put("bench/a", Bytes::from_static(b"x")).await.unwrap();
    assert!(!again.is_ok());
    assert_eq!(again.status, Some(412));

    let head = s3.head("bench/a").await.unwrap();
    assert_eq!(head.result.unwrap(), Some(4096));
    let missing = s3.head("bench/none").await.unwrap();
    assert_eq!(missing.status, Some(404));
    assert_eq!(missing.result.unwrap(), None);

    let part = s3.get_range("bench/a", 10, 20).await.unwrap();
    assert_eq!(part.result.unwrap().len(), 20);
    let gone = s3.get_range("bench/none", 0, 1).await.unwrap();
    assert_eq!(gone.status, Some(404));

    let page = s3
        .list_page("bench/", None, 10)
        .await
        .unwrap()
        .result
        .unwrap();
    assert_eq!(page.objects.len(), 1);

    assert!(s3.delete("bench/a").await.unwrap().is_ok());
    assert_eq!(s3.head("bench/a").await.unwrap().result.unwrap(), None);

    let seen = records(&log_path);
    assert_eq!(log.count(), 9);
    assert_eq!(seen.len(), 9);
    assert!(
        seen.iter()
            .all(|r| r.role == Role::Instance && r.bucket == "kbf-fake")
    );
    let methods: Vec<&str> = seen.iter().map(|r| r.method.as_str()).collect();
    assert_eq!(
        methods,
        [
            "PUT", "PUT", "HEAD", "HEAD", "GET", "GET", "GET", "DELETE", "HEAD"
        ]
    );
    assert_eq!(seen[0].bytes, 4096);
    assert_eq!(seen[0].key, "bench/a");
    assert_eq!(seen[0].error_kind, None);
    assert_eq!(seen[1].error_kind.as_deref(), Some("already-exists"));
    assert_eq!(seen[1].status, Some(412));
    assert_eq!(seen[3].status, Some(404));
}

/// Catches a malformed key, range or page size reaching the wire, and one that leaves a record
/// of a request that was never sent.
#[tokio::test]
async fn a_malformed_request_is_refused_before_it_is_sent_or_logged() {
    let fake = FakeS3::start().await;
    let log = Arc::new(RequestLog::to_writer(std::io::sink()));
    let s3 = BenchS3::new(
        Role::Hot,
        &spec(fake.endpoint()),
        Timeouts::default(),
        log.clone(),
    )
    .unwrap();
    let body = Bytes::from_static(b"x");
    assert!(matches!(s3.put("a b", body).await, Err(BenchError::Key(_))));
    assert!(matches!(s3.head("../x").await, Err(BenchError::Key(_))));
    assert!(matches!(s3.delete("").await, Err(BenchError::Key(_))));
    assert!(matches!(
        s3.get_range("k", 0, 0).await,
        Err(BenchError::Request(_))
    ));
    assert!(matches!(
        s3.get_range("a b", 0, 1).await,
        Err(BenchError::Key(_))
    ));
    assert!(matches!(
        s3.list_page("a b/", None, 10).await,
        Err(BenchError::Key(_))
    ));
    assert!(matches!(
        s3.list_page("p/", None, 0).await,
        Err(BenchError::Request(_))
    ));
    assert_eq!(log.count(), 0);
}

/// Catches a store that does not answer being recorded as an answer: no status, class
/// `transport`, and the error text (which names the endpoint) kept out of the log.
#[tokio::test]
async fn a_request_that_gets_no_answer_has_no_status_and_a_class_only() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-s3-down");
    let log_path = dir.path().join("requests.jsonl");
    let log = Arc::new(RequestLog::create(&log_path).unwrap());
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let s3 = BenchS3::new(Role::Cold, &spec(&endpoint), Timeouts::default(), log).unwrap();
    let out = s3.put("bench/a", Bytes::from_static(b"x")).await.unwrap();
    assert!(!out.is_ok());
    assert_eq!(out.status, None);
    let seen = records(&log_path);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].error_kind.as_deref(), Some("transport"));
    assert!(!fs::read_to_string(&log_path).unwrap().contains("http://"));
}

/// Catches an endpoint or bucket the client would refuse only at the first request.
#[test]
fn a_bad_endpoint_or_bucket_is_refused_when_the_client_is_made() {
    let log = Arc::new(RequestLog::to_writer(std::io::sink()));
    for (endpoint, bucket) in [
        ("https://store.example:9000", "kbf-fake"),
        ("http://store.example:9000", "B"),
    ] {
        let mut s = spec(endpoint);
        s.bucket = bucket.to_owned();
        let made = BenchS3::new(Role::Hot, &s, Timeouts::default(), log.clone());
        assert!(
            matches!(made, Err(BenchError::Store(_))),
            "{endpoint} {bucket}"
        );
    }
}

/// A writer that refuses everything, as a full disk does.
struct FullDisk;

impl std::io::Write for FullDisk {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("disk full"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Catches a request whose record could not be written being reported as a success: a
/// harness-side log with a gap proves nothing, so the call is an error.
#[tokio::test]
async fn a_call_whose_record_cannot_be_written_is_an_error() {
    let log = Arc::new(RequestLog::to_writer(FullDisk));
    let fake = FakeS3::start().await;
    let s3 = BenchS3::new(
        Role::Instance,
        &spec(fake.endpoint()),
        Timeouts::default(),
        log.clone(),
    )
    .unwrap();
    let err = s3
        .put("bench/a", Bytes::from_static(b"x"))
        .await
        .unwrap_err();
    assert!(matches!(err, BenchError::Log(_)));
    assert_eq!(log.count(), 0);
}
