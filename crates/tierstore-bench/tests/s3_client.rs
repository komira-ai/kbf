//! The S3 client wrapper against a loopback S3 service: what it times, what it logs and what it
//! refuses before a request is sent.

use std::fs;
use std::sync::Arc;

use bytes::Bytes;
use kbf_testkit::dir::TestDir;
use objstore_s3::PageSize;
use objstore_s3::s3::PART_BYTES;
use objstore_s3::s3::fake::{Failure, FakeS3};
use tierstore_bench::config::{Secret, StoreSpec};
use tierstore_bench::error::BenchError;
use tierstore_bench::requestlog::{RequestLog, RequestRecord};
use tierstore_bench::role::Role;
use tierstore_bench::s3::{BenchS3, Timeouts};

mod dead_store;

use dead_store::DeadStore;

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
    let dead = DeadStore::start().await;
    let s3 = BenchS3::new(Role::Cold, &spec(dead.endpoint()), Timeouts::default(), log).unwrap();
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

/// A writer that takes `n` writes and then refuses, as a disk that fills up does.
struct FailAfter(usize);

impl std::io::Write for FailAfter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.0 == 0 {
            return Err(std::io::Error::other("disk full"));
        }
        self.0 -= 1;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn multipart_body() -> Bytes {
    // One part and a bit more: two parts, the first of the 16 MiB the client sends.
    Bytes::from(vec![3u8; (PART_BYTES as usize) + 1000])
}

fn client(endpoint: &str, log: Arc<RequestLog>) -> BenchS3 {
    BenchS3::new(Role::Instance, &spec(endpoint), Timeouts::default(), log).unwrap()
}

/// Catches a multipart put that is logged as one request (the cold path's cost is in its parts),
/// timed short of its parts, or that stores other than what it was given: four records (create,
/// two parts, complete), a latency that is at least the sum of theirs, and the object whole.
#[tokio::test]
async fn a_multipart_put_logs_each_request_and_is_timed_as_the_whole() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-s3-mp");
    let log_path = dir.path().join("requests.jsonl");
    let log = Arc::new(RequestLog::create(&log_path).unwrap());
    let fake = FakeS3::start().await;
    let s3 = client(fake.endpoint(), log);
    let put = s3.put_multipart("mp/a", multipart_body()).await.unwrap();
    assert!(put.is_ok());
    assert_eq!(put.status, Some(200));
    let seen = records(&log_path);
    let methods: Vec<&str> = seen.iter().map(|r| r.method.as_str()).collect();
    assert_eq!(methods, ["POST", "PUT", "PUT", "POST"]);
    assert_eq!(seen[1].bytes, PART_BYTES);
    assert_eq!(seen[2].bytes, 1000);
    let sum: u64 = seen.iter().map(|r| r.latency_us).sum();
    assert!(put.latency_us() >= sum, "{} < {sum}", put.latency_us());
    let stats = fake.stats();
    assert_eq!((stats.parts, stats.completed, stats.aborted), (2, 1, 0));
    let key = objstore_s3::ObjectKey::new("mp/a").unwrap();
    assert_eq!(fake.object_len(&key), Some(PART_BYTES + 1000));
    assert_eq!(
        s3.head("mp/a").await.unwrap().result.unwrap(),
        Some(PART_BYTES + 1000)
    );
}

/// Catches a failure of a multipart put that is not a failure (a store error is a measurement,
/// with its status), an upload left open after a part or the completion failed, and one aborted
/// when the creation failed: three failures, each as the store made it.
#[tokio::test]
async fn a_failed_multipart_put_is_a_measurement_and_aborts_what_it_began() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "tierstore-bench-s3-mp-fail");
    let log_path = dir.path().join("requests.jsonl");
    let log = Arc::new(RequestLog::create(&log_path).unwrap());
    let fake = FakeS3::start().await;
    let s3 = client(fake.endpoint(), log);

    fake.fail(Failure::Create);
    let created = s3.put_multipart("mp/c", multipart_body()).await.unwrap();
    assert_eq!(created.status, Some(500));
    assert!(!created.is_ok());
    assert_eq!(records(&log_path).len(), 1);

    fake.fail(Failure::Part(2));
    let part = s3.put_multipart("mp/p", multipart_body()).await.unwrap();
    assert_eq!(part.status, Some(500));
    assert!(!part.is_ok());

    fake.fail(Failure::Complete);
    let complete = s3.put_multipart("mp/q", multipart_body()).await.unwrap();
    assert_eq!(complete.status, Some(500));

    let stats = fake.stats();
    assert_eq!((stats.aborted, stats.completed), (2, 0));
    assert_eq!(fake.incomplete_uploads(), 0);
    assert!(fake.keys().is_empty());
    let seen = records(&log_path);
    let aborts = seen.iter().filter(|r| r.method == "DELETE").count();
    assert_eq!(aborts, 2);
}

/// Catches a malformed key or an empty body reaching the wire, and a put whose record cannot be
/// written being reported as a put: the call is an error at each request it makes (creation, a
/// part, completion, abort).
#[tokio::test]
async fn a_multipart_put_refuses_bad_input_and_stops_when_its_log_does() {
    let fake = FakeS3::start().await;
    let log = Arc::new(RequestLog::to_writer(std::io::sink()));
    let s3 = client(fake.endpoint(), log.clone());
    assert!(matches!(
        s3.put_multipart("a b", multipart_body()).await,
        Err(BenchError::Key(_))
    ));
    assert!(matches!(
        s3.put_multipart("mp/e", Bytes::new()).await,
        Err(BenchError::Request(_))
    ));
    assert_eq!(log.count(), 0);

    for taken in 0..4 {
        let log = Arc::new(RequestLog::to_writer(FailAfter(taken)));
        let s3 = client(fake.endpoint(), log);
        let key = format!("mp/log{taken}");
        let result = s3.put_multipart(&key, multipart_body()).await;
        assert!(matches!(result, Err(BenchError::Log(_))), "{taken}");
    }
    fake.fail(Failure::Complete);
    let log = Arc::new(RequestLog::to_writer(FailAfter(4)));
    let s3 = client(fake.endpoint(), log);
    let result = s3.put_multipart("mp/log-abort", multipart_body()).await;
    assert!(matches!(result, Err(BenchError::Log(_))));
}

/// Catches unfinished uploads left under a prefix, uploads outside it touched, a listing that
/// stops at its first page, and a count that is not what was found: three open uploads, one
/// outside, swept in pages of one.
#[tokio::test]
async fn unfinished_uploads_under_a_prefix_are_aborted_page_by_page() {
    let fake = FakeS3::start().await;
    let log = Arc::new(RequestLog::to_writer(std::io::sink()));
    let s3 = client(fake.endpoint(), log);
    for key in ["sweep/a", "sweep/b", "sweep/c", "keep/d"] {
        fake.fail(Failure::Complete);
        fake.fail(Failure::Abort);
        let put = s3.put_multipart(key, multipart_body()).await.unwrap();
        assert!(!put.is_ok());
    }
    assert_eq!(fake.incomplete_uploads(), 4);
    let found = s3
        .abort_uploads_in_pages("sweep/", PageSize::new(1).unwrap())
        .await
        .unwrap();
    assert_eq!(found, 3);
    assert_eq!(fake.incomplete_uploads(), 1);
    assert_eq!(s3.abort_uploads("keep/").await.unwrap(), 1);
    assert_eq!(s3.abort_uploads("keep/").await.unwrap(), 0);
    assert_eq!(fake.incomplete_uploads(), 0);
    assert!(matches!(
        s3.abort_uploads("a b/").await,
        Err(BenchError::Key(_))
    ));
}

/// Catches a sweep that goes on when its log cannot be written: at the listing, and at an abort.
#[tokio::test]
async fn a_sweep_stops_when_its_log_does() {
    let fake = FakeS3::start().await;
    let quiet = Arc::new(RequestLog::to_writer(std::io::sink()));
    let s3 = client(fake.endpoint(), quiet);
    fake.fail(Failure::Complete);
    fake.fail(Failure::Abort);
    s3.put_multipart("sweep/a", multipart_body()).await.unwrap();
    for taken in 0..2 {
        let log = Arc::new(RequestLog::to_writer(FailAfter(taken)));
        let sweeper = client(fake.endpoint(), log);
        let result = sweeper.abort_uploads("sweep/").await;
        assert!(matches!(result, Err(BenchError::Log(_))), "{taken}");
    }
    // The second sweep sent its abort before its record could not be written.
    assert_eq!(fake.incomplete_uploads(), 0);
}

/// Catches a sweep of a store that cannot be listed being an error of the run: it ends with
/// what it found, nothing.
#[tokio::test]
async fn a_sweep_of_a_dead_store_finds_nothing() {
    let dead = DeadStore::start().await;
    let log = Arc::new(RequestLog::to_writer(std::io::sink()));
    let s3 = client(dead.endpoint(), log);
    assert_eq!(s3.abort_uploads("sweep/").await.unwrap(), 0);
}
