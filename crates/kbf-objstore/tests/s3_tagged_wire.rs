//! The S3 store's conditional write, HEAD, whole-object read and stamped listing against a
//! scripted HTTP server on loopback: what each request carries, and how each answer is read.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use kbf_objstore::s3::{Credentials, ObjectHead, S3Config, S3Store};
use kbf_objstore::{Capabilities, KeyPrefix, ObjectKey, ObjectStoreError, PageSize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves `responses` in order, one per connection, and records each request head.
async fn script(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        for resp in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                assert!(n > 0, "client closed before sending a request");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
            let body_len = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while buf.len() < head_end + body_len {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
            }
            log.lock().unwrap().push(head);
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.shutdown().await.unwrap();
        }
    });
    (endpoint, seen)
}

/// An answer with `headers` and a body whose length it states.
fn answer(status: &str, headers: &[&str], body: &str) -> String {
    let mut s = format!(
        "HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: {}\r\n",
        body.len()
    );
    for h in headers {
        s.push_str(h);
        s.push_str("\r\n");
    }
    s.push_str("\r\n");
    s.push_str(body);
    s
}

/// The answer to a HEAD: exactly `headers`, and no body.
fn head_answer(status: &str, headers: &[&str]) -> String {
    let mut s = format!("HTTP/1.1 {status}\r\nconnection: close\r\n");
    for h in headers {
        s.push_str(h);
        s.push_str("\r\n");
    }
    s.push_str("\r\n");
    s
}

fn store(endpoint: String) -> S3Store {
    S3Store::new(S3Config {
        endpoint,
        region: "us-east-1".into(),
        bucket: "kbf-test".into(),
        credentials: Credentials::new("AKIDEXAMPLE", "secret"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        min_body_rate: None,
    })
    .unwrap()
}

fn key() -> ObjectKey {
    ObjectKey::new("cas/seg-1").unwrap()
}

/// The status and code of a `Service` error.
fn service(error: &ObjectStoreError) -> (u16, &str) {
    match error {
        ObjectStoreError::Service { status, code, .. } => (*status, code.as_str()),
        other => panic!("expected Service, got {other:?}"),
    }
}

const LENGTH: &str = "content-length: 7";
const ETAG: &str = "etag: \"abc\"";
const MODIFIED: &str = "last-modified: Sun, 06 Nov 1994 08:49:37 GMT";
const NO_SUCH_KEY: &str = "<Error><Code>NoSuchKey</Code></Error>";
const FAILED: &str = "<Error><Code>PreconditionFailed</Code><Message>m</Message></Error>";

/// Asserts that a replace is a PUT carrying `If-Match` with the tag as given, signed with
/// the other headers and with no `If-None-Match`, and that it gives the `ETag` of the answer.
#[tokio::test]
async fn a_put_if_match_sends_the_tag_and_reads_the_new_one() {
    let replaced = answer("200 OK", &["etag: \"new\""], "");
    let (endpoint, seen) = script(vec![replaced]).await;
    let s = store(endpoint);
    let got = s
        .put_if_match(&key(), Bytes::from("abc"), "\"old\"")
        .await
        .unwrap();
    assert_eq!(got, "\"new\"");

    let heads = seen.lock().unwrap();
    let put = heads[0].to_ascii_lowercase();
    assert!(
        put.starts_with("put /kbf-test/cas/seg-1 http/1.1\r\n"),
        "{put}"
    );
    assert!(put.contains("\r\nif-match: \"old\"\r\n"), "{put}");
    assert!(!put.contains("if-none-match"), "{put}");
    assert!(
        put.contains("signedheaders=host;if-match;x-amz-content-sha256;x-amz-date"),
        "{put}"
    );
}

/// Asserts that a replace is refused as the answer says: 412 is a `Service` error with its
/// status and code, 404 is `NotFound` naming the key, an `OK` without an `ETag` is a
/// `Protocol` error, and any other status is a `Service` error.
#[tokio::test]
async fn a_put_if_match_that_fails_is_typed() {
    let (endpoint, _) = script(vec![
        answer("412 Precondition Failed", &[], FAILED),
        answer("404 Not Found", &[], NO_SUCH_KEY),
        answer("200 OK", &[], ""),
        answer("500 Internal Server Error", &[], ""),
    ])
    .await;
    let s = store(endpoint);
    let mut errors = Vec::new();
    for _ in 0..4 {
        let k = key();
        let put = s.put_if_match(&k, Bytes::from("abc"), "\"old\"");
        errors.push(put.await.unwrap_err());
    }
    assert_eq!(service(&errors[0]), (412, "PreconditionFailed"));
    assert!(
        matches!(&errors[1], ObjectStoreError::NotFound(k) if *k == key()),
        "{:?}",
        errors[1]
    );
    assert!(
        matches!(&errors[2], ObjectStoreError::Protocol(why) if why.contains("no ETag")),
        "{:?}",
        errors[2]
    );
    assert_eq!(service(&errors[3]).0, 500);
}

/// Asserts that a HEAD is a HEAD of the key, and reads the size from `Content-Length`, the
/// tag from `ETag` as sent (quotes included) and the time from `Last-Modified`.
#[tokio::test]
async fn a_head_reads_the_size_the_tag_and_the_time_from_the_headers() {
    let found = head_answer("200 OK", &[LENGTH, ETAG, MODIFIED]);
    let (endpoint, seen) = script(vec![found]).await;
    let s = store(endpoint);
    let head = s.head(&key()).await.unwrap();
    let want = ObjectHead {
        size: 7,
        etag: "\"abc\"".to_owned(),
        modified_millis: 784_111_777_000,
    };
    assert_eq!(head, want);
    let heads = seen.lock().unwrap();
    assert!(
        heads[0].starts_with("HEAD /kbf-test/cas/seg-1 HTTP/1.1\r\n"),
        "{}",
        heads[0]
    );
}

/// Asserts that a HEAD is `NotFound` for 404, a `Service` error for another status, and a
/// `Protocol` error naming what is wrong when the answer has no size, no tag, no time of
/// last write or a time that is not one.
#[tokio::test]
async fn a_head_that_fails_or_is_incomplete_is_typed() {
    let (endpoint, _) = script(vec![
        head_answer("404 Not Found", &[]),
        head_answer("500 Internal Server Error", &[]),
        head_answer("200 OK", &[ETAG, MODIFIED]),
        head_answer("200 OK", &[LENGTH, MODIFIED]),
        head_answer("200 OK", &[LENGTH, ETAG]),
        head_answer("200 OK", &[LENGTH, ETAG, "last-modified: yesterday"]),
    ])
    .await;
    let s = store(endpoint);
    let mut errors = Vec::new();
    for _ in 0..6 {
        errors.push(s.head(&key()).await.unwrap_err());
    }
    assert!(
        matches!(&errors[0], ObjectStoreError::NotFound(k) if *k == key()),
        "{:?}",
        errors[0]
    );
    assert_eq!(service(&errors[1]).0, 500);
    let whys = [
        "missing or bad Content-Length",
        "no ETag",
        "missing or bad Last-Modified",
        "missing or bad Last-Modified",
    ];
    for (error, why) in errors[2..].iter().zip(whys) {
        let want = format!("HEAD of cas/seg-1: {why}");
        assert!(
            matches!(error, ObjectStoreError::Protocol(m) if *m == want),
            "{error:?}"
        );
    }
}

/// Asserts that a HEAD sent on a connection that closes before the answer is sent again,
/// that it gives up after the sends of a ranged read (three) with the transport error, and
/// that an answer that is not a transport failure is not sent again.
#[tokio::test]
async fn a_head_is_sent_again_after_a_transport_failure_and_only_then() {
    let (endpoint, seen) = script(vec![
        String::new(),
        head_answer("200 OK", &[LENGTH, ETAG, MODIFIED]),
    ])
    .await;
    let s = store(endpoint);
    let head = s.head(&key()).await.expect("one dropped connection");
    assert_eq!(head.size, 7);
    assert_eq!(seen.lock().unwrap().len(), 2);

    let (endpoint, seen) = script(vec![String::new(); 3]).await;
    let err = store(endpoint).head(&key()).await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    assert_eq!(seen.lock().unwrap().len(), 3);

    let (endpoint, seen) = script(vec![head_answer("404 Not Found", &[])]).await;
    store(endpoint).head(&key()).await.unwrap_err();
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// Asserts that a whole-object read and a stamped listing are each sent again after a
/// connection that closes before the answer and then give the answer, and that a replace on
/// such a connection is sent once and fails with the transport error.
#[tokio::test]
async fn reads_are_sent_again_after_a_transport_failure_and_a_replace_is_not() {
    let hello = answer("200 OK", &[], "hello");
    let (endpoint, seen) = script(vec![String::new(), hello]).await;
    let got = store(endpoint).get_all(&key()).await.unwrap();
    assert_eq!(got, "hello");
    assert_eq!(seen.lock().unwrap().len(), 2);

    let page = "<ListBucketResult><IsTruncated>false</IsTruncated>\
                <Contents><Key>p/a</Key><Size>1</Size>\
                <LastModified>2009-10-12T17:50:30.000Z</LastModified></Contents>\
                </ListBucketResult>";
    let page = answer("200 OK", &[], page);
    let (endpoint, seen) = script(vec![String::new(), page]).await;
    let s = store(endpoint);
    let prefix = KeyPrefix::new("p/").unwrap();
    let size = PageSize::new(3).unwrap();
    let listed = s.list_stamped(&prefix, None, size).await.unwrap();
    assert_eq!(listed.objects.len(), 1);
    assert_eq!(seen.lock().unwrap().len(), 2);

    let (endpoint, seen) = script(vec![String::new()]).await;
    let s = store(endpoint);
    let k = key();
    let put = s.put_if_match(&k, Bytes::from("x"), "\"t\"");
    let err = put.await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// Asserts that a whole-object read is a GET with no `Range` that gives the body, that 404
/// is `NotFound`, and that another status is a `Service` error.
#[tokio::test]
async fn a_whole_object_read_sends_no_range_and_is_typed_when_it_fails() {
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], "hello"),
        answer("404 Not Found", &[], NO_SUCH_KEY),
        answer("500 Internal Server Error", &[], ""),
    ])
    .await;
    let s = store(endpoint);
    assert_eq!(s.get_all(&key()).await.unwrap(), "hello");
    let missing = s.get_all(&key()).await.unwrap_err();
    assert!(matches!(&missing, ObjectStoreError::NotFound(k) if *k == key()));
    let failed = s.get_all(&key()).await.unwrap_err();
    assert_eq!(service(&failed).0, 500);

    let heads = seen.lock().unwrap();
    let first = heads[0].to_ascii_lowercase();
    assert!(
        first.starts_with("get /kbf-test/cas/seg-1 http/1.1\r\n"),
        "{first}"
    );
    assert!(!first.contains("\r\nrange:"), "{first}");
}

/// Asserts that a stamped listing is the ListObjectsV2 request of a plain listing, reads the
/// time of last write of each object, and is a `Protocol` error when an object has none.
#[tokio::test]
async fn a_stamped_listing_sends_a_list_request_and_reads_the_times() {
    let page = "<ListBucketResult><IsTruncated>false</IsTruncated>\
                <Contents><Key>p/a</Key><Size>1</Size>\
                <LastModified>2009-10-12T17:50:30.000Z</LastModified></Contents>\
                </ListBucketResult>";
    let untimed = "<ListBucketResult><IsTruncated>false</IsTruncated>\
                   <Contents><Key>p/a</Key><Size>1</Size></Contents></ListBucketResult>";
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], page),
        answer("200 OK", &[], untimed),
    ])
    .await;
    let s = store(endpoint);
    let prefix = KeyPrefix::new("p/").unwrap();
    let size = PageSize::new(3).unwrap();
    let listed = s.list_stamped(&prefix, None, size).await.unwrap();
    assert_eq!(listed.next, None);
    assert_eq!(listed.objects.len(), 1);
    assert_eq!(listed.objects[0].key.as_str(), "p/a");
    assert_eq!(listed.objects[0].size, 1);
    assert_eq!(listed.objects[0].modified_millis, 1_255_369_830_000);

    let err = s.list_stamped(&prefix, None, size).await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::Protocol(_)), "{err}");

    let heads = seen.lock().unwrap();
    assert!(
        heads[0].starts_with("GET /kbf-test?list-type=2&max-keys=3&prefix=p%2F HTTP/1.1"),
        "{}",
        heads[0]
    );
}
