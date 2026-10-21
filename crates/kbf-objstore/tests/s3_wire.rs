//! The S3 backend against a scripted HTTP server on loopback: what it sends, and what it
//! refuses to believe. The real-store runs (MinIO, RustFS) are in CI; these pin the
//! cases a real store does not produce on demand.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt as _, TryStreamExt as _};
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{
    ByteRange, Capabilities, KeyPrefix, ListToken, ObjectKey, ObjectStore, ObjectStoreError,
    PageSize, listing,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves `responses` in order, one per connection, and records each request head.
async fn script(responses: Vec<String>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let endpoint = format!("http://{addr}");
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

fn store(endpoint: String, capabilities: Capabilities) -> S3Store {
    S3Store::new(S3Config {
        endpoint,
        region: "us-east-1".into(),
        bucket: "kbf-test".into(),
        credentials: Credentials::new("AKIDEXAMPLE", "secret"),
        capabilities,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        min_body_rate: None,
    })
    .unwrap()
}

fn key() -> ObjectKey {
    ObjectKey::new("cas/seg-1").unwrap()
}

/// Catches: a client that accepts a 200 to a ranged GET (a server or proxy that ignores
/// Range), or a 206 whose Content-Range names other bytes, and hands the wrong bytes to
/// the caller as if they were the window asked for.
#[tokio::test]
async fn a_server_ignoring_range_is_refused() {
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], "0123456789"),
        answer(
            "206 Partial Content",
            &["content-range: bytes 0-3/10"],
            "0123",
        ),
        answer(
            "206 Partial Content",
            &["content-range: bytes 2-5/10"],
            "2345",
        ),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let r = ByteRange::new(2, 4).unwrap();
    let err = s.get_range(&key(), r).await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Protocol(m) if m.contains("ignored Range")),
        "{err}"
    );
    let err = s.get_range(&key(), r).await.unwrap_err();
    assert!(matches!(err, ObjectStoreError::Protocol(_)), "{err}");
    assert_eq!(s.get_range(&key(), r).await.unwrap(), Bytes::from("2345"));

    let heads = seen.lock().unwrap();
    let first = heads[0].to_ascii_lowercase();
    assert!(
        first.starts_with("get /kbf-test/cas/seg-1 http/1.1\r\n"),
        "path-style: {first}"
    );
    assert!(first.contains("\r\nrange: bytes=2-5\r\n"), "{first}");
    assert!(
        first.contains("signedheaders=host;range;x-amz-content-sha256;x-amz-date"),
        "{first}"
    );
}

/// Catches: a put that does not ask for a conditional write when the store claims it,
/// or reads 412 as anything but `AlreadyExists`; and a retained put without the lock
/// headers and the checksum S3 requires.
#[tokio::test]
async fn put_new_sends_conditions_and_retention() {
    let (endpoint, seen) = script(vec![
        answer("412 Precondition Failed", &[], ""),
        answer("200 OK", &[], ""),
    ])
    .await;
    let s = store(
        endpoint,
        Capabilities {
            conditional_put: true,
            object_lock: true,
        },
    );
    let err = s
        .put_new(&key(), Bytes::from("abc"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::AlreadyExists(_)), "{err}");
    let until = std::time::UNIX_EPOCH + Duration::from_secs(4_107_542_400);
    s.put_new(&key(), Bytes::from("abc"), Some(until))
        .await
        .unwrap();

    let heads = seen.lock().unwrap();
    let put = heads[1].to_ascii_lowercase();
    assert!(put.contains("\r\nif-none-match: *\r\n"), "{put}");
    assert!(
        put.contains("\r\nx-amz-object-lock-mode: compliance\r\n"),
        "{put}"
    );
    assert!(
        put.contains("\r\nx-amz-object-lock-retain-until-date: 2100-03-01t00:00:00z\r\n"),
        "{put}"
    );
    // MD5("abc"), base64.
    assert!(
        put.contains(
            "\r\ncontent-md5: kAFQmDzST7DWlj99KOF/cg==\r\n"
                .to_ascii_lowercase()
                .as_str()
        ),
        "{put}"
    );
    // SHA-256("abc"), the signed payload hash.
    assert!(
        put.contains(
            "x-amz-content-sha256: ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        ),
        "{put}"
    );
}

/// Catches: a delete on an Object Lock store sent without the version id, which S3
/// answers by adding a delete marker (success) while keeping the bytes; and a delete of
/// an absent key reported as an error.
#[tokio::test]
async fn delete_on_a_locked_store_targets_the_version() {
    let (endpoint, seen) = script(vec![
        answer("200 OK", &["x-amz-version-id: v+1/x"], ""),
        answer(
            "403 Forbidden",
            &[],
            "<Error><Code>AccessDenied</Code><Message>locked</Message></Error>",
        ),
        answer("404 Not Found", &[], ""),
    ])
    .await;
    let s = store(
        endpoint,
        Capabilities {
            conditional_put: false,
            object_lock: true,
        },
    );
    let err = s.delete(&key()).await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Service { status: 403, code, .. } if code == "AccessDenied"),
        "{err}"
    );
    s.delete(&key()).await.unwrap();

    let heads = seen.lock().unwrap();
    assert!(
        heads[0].starts_with("HEAD /kbf-test/cas/seg-1 HTTP/1.1"),
        "{}",
        heads[0]
    );
    assert!(
        heads[1].starts_with("DELETE /kbf-test/cas/seg-1?versionId=v%2B1%2Fx HTTP/1.1"),
        "{}",
        heads[1]
    );
    assert!(heads[2].starts_with("HEAD "), "{}", heads[2]);
}

/// Catches: a list request that is not ListObjectsV2, loses the continuation token or
/// encodes the query differently from how it was signed.
#[tokio::test]
async fn list_sends_the_token_and_reads_the_next_one() {
    let page = "<ListBucketResult><IsTruncated>true</IsTruncated>\
                <NextContinuationToken>t2</NextContinuationToken>\
                <Contents><Key>p/a</Key><Size>1</Size></Contents></ListBucketResult>";
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], page),
        answer("200 OK", &[], page),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let prefix = KeyPrefix::new("p/").unwrap();
    let first = s
        .list(&prefix, None, PageSize::new(3).unwrap())
        .await
        .unwrap();
    let token = first.next.expect("a truncated page has a token");
    s.list(&prefix, Some(&token), PageSize::new(3).unwrap())
        .await
        .unwrap();

    let heads = seen.lock().unwrap();
    assert!(
        heads[0].starts_with("GET /kbf-test?list-type=2&max-keys=3&prefix=p%2F HTTP/1.1"),
        "{}",
        heads[0]
    );
    assert!(
        heads[1].starts_with(
            "GET /kbf-test?continuation-token=t2&list-type=2&max-keys=3&prefix=p%2F HTTP/1.1"
        ),
        "{}",
        heads[1]
    );
}

/// Catches: a listing [`ListToken::after`] a key that does not send it as
/// `start-after` (S3 would read it as a continuation token and refuse it, or list from
/// the start), or sends it as well as a continuation token on the pages that follow.
#[tokio::test]
async fn list_after_a_key_sends_start_after_then_the_token() {
    let page = "<ListBucketResult><IsTruncated>true</IsTruncated>\
                <NextContinuationToken>t2</NextContinuationToken>\
                <Contents><Key>p/c</Key><Size>1</Size></Contents></ListBucketResult>";
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], page),
        answer("200 OK", &[], page),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let prefix = KeyPrefix::new("p/").unwrap();
    let after = ListToken::after(&ObjectKey::new("p/b").unwrap());
    let first = s
        .list(&prefix, Some(&after), PageSize::new(3).unwrap())
        .await
        .unwrap();
    s.list(&prefix, first.next.as_ref(), PageSize::new(3).unwrap())
        .await
        .unwrap();

    let heads = seen.lock().unwrap();
    assert!(
        heads[0].starts_with(
            "GET /kbf-test?list-type=2&max-keys=3&prefix=p%2F&start-after=p%2Fb HTTP/1.1"
        ),
        "{}",
        heads[0]
    );
    assert!(
        heads[1].starts_with(
            "GET /kbf-test?continuation-token=t2&list-type=2&max-keys=3&prefix=p%2F HTTP/1.1"
        ),
        "{}",
        heads[1]
    );
}

/// Catches: a listing stream over S3 that does not carry the continuation token from
/// page to page, or stops at a truncated page: two pages, each asked for once, the
/// second with the first's token, every key in order.
#[tokio::test]
async fn the_listing_stream_follows_continuation_tokens() {
    let first = "<ListBucketResult><IsTruncated>true</IsTruncated>\
                 <NextContinuationToken>t2</NextContinuationToken>\
                 <Contents><Key>p/a</Key><Size>1</Size></Contents>\
                 <Contents><Key>p/b/c</Key><Size>2</Size></Contents></ListBucketResult>";
    let last = "<ListBucketResult><IsTruncated>false</IsTruncated>\
                <Contents><Key>p/c</Key><Size>3</Size></Contents></ListBucketResult>";
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], first),
        answer("200 OK", &[], last),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let prefix = KeyPrefix::new("p/").unwrap();
    let got: Vec<(String, u64)> = listing::objects(&s, &prefix, None, PageSize::new(2).unwrap())
        .map_ok(|o| (o.key.as_str().to_owned(), o.size))
        .try_collect()
        .await
        .unwrap();
    assert_eq!(
        got,
        [("p/a".into(), 1), ("p/b/c".into(), 2), ("p/c".into(), 3)]
    );
    let heads = seen.lock().unwrap();
    assert_eq!(heads.len(), 2);
    assert!(
        heads[1].starts_with(
            "GET /kbf-test?continuation-token=t2&list-type=2&max-keys=2&prefix=p%2F HTTP/1.1"
        ),
        "{}",
        heads[1]
    );
}

/// Catches: a listing stream resumed [`ListToken::after`] a key over S3 that restarts at
/// the start of the prefix (no `start-after`), sends the key again with the
/// continuation token, or believes a server that lists the key itself (an inclusive
/// `start-after`, which would hand a rebuild a segment twice): the stream sends
/// `start-after`, then only the token, and yields exactly the keys after the key; a
/// server answering with the key itself ends the stream with `Protocol`.
#[tokio::test]
async fn a_listing_resumed_after_a_key_lists_only_the_rest_over_s3() {
    let first = "<ListBucketResult><IsTruncated>true</IsTruncated>\
                 <NextContinuationToken>t2</NextContinuationToken>\
                 <Contents><Key>p/c</Key><Size>1</Size></Contents>\
                 <Contents><Key>p/d</Key><Size>2</Size></Contents></ListBucketResult>";
    let last = "<ListBucketResult><IsTruncated>false</IsTruncated>\
                <Contents><Key>p/e</Key><Size>3</Size></Contents></ListBucketResult>";
    let inclusive = "<ListBucketResult><IsTruncated>false</IsTruncated>\
                     <Contents><Key>p/b</Key><Size>1</Size></Contents>\
                     <Contents><Key>p/c</Key><Size>1</Size></Contents></ListBucketResult>";
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], first),
        answer("200 OK", &[], last),
        answer("200 OK", &[], inclusive),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let prefix = KeyPrefix::new("p/").unwrap();
    let from = ListToken::after(&ObjectKey::new("p/b").unwrap());
    let size = PageSize::new(2).unwrap();
    let got: Vec<String> = listing::objects(&s, &prefix, Some(from.clone()), size)
        .map_ok(|o| o.key.as_str().to_owned())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(got, ["p/c", "p/d", "p/e"]);
    {
        let heads = seen.lock().unwrap();
        assert!(
            heads[0].starts_with(
                "GET /kbf-test?list-type=2&max-keys=2&prefix=p%2F&start-after=p%2Fb HTTP/1.1"
            ),
            "{}",
            heads[0]
        );
        assert!(
            heads[1].starts_with(
                "GET /kbf-test?continuation-token=t2&list-type=2&max-keys=2&prefix=p%2F HTTP/1.1"
            ),
            "{}",
            heads[1]
        );
    }
    let refused: Result<Vec<_>, _> = listing::objects(&s, &prefix, Some(from), size)
        .try_collect()
        .await;
    match refused {
        Err(ObjectStoreError::Protocol(m)) => assert!(m.contains("p/b after p/b"), "{m}"),
        other => panic!("expected Protocol, got {other:?}"),
    }
}

/// Catches: a listing failure mapped to the wrong error, or to an empty page (a rebuild
/// would take an unreadable bucket for an empty one): an S3 error is `Service` with its
/// status and code; an answer that is not a ListObjectsV2 result, or that lists a key
/// kbf cannot name (non-ASCII here: an object kbf did not write), is `Protocol`.
#[tokio::test]
async fn listing_failures_are_typed() {
    let denied = "<Error><Code>AccessDenied</Code><Message>no</Message></Error>";
    let missing = "<Error><Code>NoSuchBucket</Code><Message>gone</Message></Error>";
    let foreign = "<ListBucketResult><IsTruncated>false</IsTruncated>\
                   <Contents><Key>p/caf\u{e9}</Key><Size>1</Size></Contents></ListBucketResult>";
    let (endpoint, _) = script(vec![
        answer("403 Forbidden", &[], denied),
        answer("404 Not Found", &[], missing),
        answer("200 OK", &[], "<html>not s3</html>"),
        answer("200 OK", &[], foreign),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let prefix = KeyPrefix::new("p/").unwrap();
    let mut answers = Vec::new();
    for _ in 0..4 {
        answers.push(s.list(&prefix, None, PageSize::MAX).await.unwrap_err());
    }
    assert!(
        matches!(&answers[0], ObjectStoreError::Service { status: 403, code, .. } if code == "AccessDenied"),
        "{:?}",
        answers[0]
    );
    assert!(
        matches!(&answers[1], ObjectStoreError::Service { status: 404, code, .. } if code == "NoSuchBucket"),
        "{:?}",
        answers[1]
    );
    assert!(
        matches!(&answers[2], ObjectStoreError::Protocol(why) if why.contains("root element is <html>")),
        "{:?}",
        answers[2]
    );
    assert!(
        matches!(&answers[3], ObjectStoreError::Protocol(why) if why.contains("listed key") && why.contains('\u{e9}')),
        "{:?}",
        answers[3]
    );
}

/// Catches: `create_bucket` that does not ask for Object Lock when the store claims it
/// (a retained put would then fail) or asks when it does not; that takes a bucket this
/// key pair already owns as an error (setup is not repeatable); or that hides any other
/// refusal, such as a bucket name another account holds.
#[tokio::test]
async fn create_bucket_asks_for_object_lock_and_accepts_its_own_bucket() {
    let owned = "<Error><Code>BucketAlreadyOwnedByYou</Code><Message>yours</Message></Error>";
    let taken = "<Error><Code>BucketAlreadyExists</Code><Message>taken</Message></Error>";
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], ""),
        answer("409 Conflict", &[], owned),
        answer("409 Conflict", &[], taken),
    ])
    .await;
    let locked = store(
        endpoint.clone(),
        Capabilities {
            conditional_put: false,
            object_lock: true,
        },
    );
    locked.create_bucket().await.unwrap();
    let plain = store(endpoint, Capabilities::default());
    plain.create_bucket().await.unwrap();
    let err = plain.create_bucket().await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Service { status: 409, code, .. } if code == "BucketAlreadyExists"),
        "{err}"
    );

    let heads = seen.lock().unwrap();
    let first = heads[0].to_ascii_lowercase();
    assert!(first.starts_with("put /kbf-test http/1.1\r\n"), "{first}");
    assert!(
        first.contains("\r\nx-amz-bucket-object-lock-enabled: true\r\n"),
        "{first}"
    );
    let second = heads[1].to_ascii_lowercase();
    assert!(second.starts_with("put /kbf-test http/1.1\r\n"), "{second}");
    assert!(!second.contains("object-lock"), "{second}");
}

/// A loopback port on which connecting is refused until [`serve_held`] turns it into a
/// listener: its socket is kept bound, so no other test can take the number meanwhile
/// (see [`kbf_testkit::port`]).
fn held_port() -> kbf_testkit::port::Reserved {
    kbf_testkit::port::Reserved::new()
}

/// The held port, listening now.
fn serve_held(held: kbf_testkit::port::Reserved) -> TcpListener {
    let listener = held.listen();
    listener.set_nonblocking(true).unwrap();
    TcpListener::from_std(listener).unwrap()
}

/// Catches a held port that is free to take (the old bind-and-drop helper): a thief
/// binding the number succeeds, and the store would be the thief.
#[test]
fn a_held_port_cannot_be_taken_by_another_listener() {
    let held = held_port();
    assert!(kbf_testkit::port::is_taken(held.addr()));
}

/// Catches: `wait_answering` returning before the store answers (no wait at all, or a
/// refused connection taken for an answer), and taking an error status for "not
/// ready" (a running S3 service answers the unsigned `GET /` with 403, so such a wait
/// would never end against a real store). The store refuses connections at first;
/// the wait must still be running then, and end once a server answers 403.
#[tokio::test]
async fn wait_answering_waits_for_a_late_store_and_takes_403_as_an_answer() {
    let held = held_port();
    let addr = held.addr();
    let s = store(format!("http://{addr}"), Capabilities::default());
    let wait = tokio::spawn(async move { s.wait_answering(Duration::from_secs(20)).await });
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        !wait.is_finished(),
        "the wait ended while nothing listened: {:?}",
        wait.await
    );
    let listener = serve_held(held);
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut chunk = [0u8; 4096];
        let n = sock.read(&mut chunk).await.unwrap();
        let head = String::from_utf8_lossy(&chunk[..n]).into_owned();
        let resp = answer(
            "403 Forbidden",
            &[],
            "<Error><Code>AccessDenied</Code></Error>",
        );
        sock.write_all(resp.as_bytes()).await.unwrap();
        sock.shutdown().await.unwrap();
        head
    });
    wait.await.unwrap().unwrap();
    let head = server.await.unwrap().to_ascii_lowercase();
    assert!(head.starts_with("get / http/1.1\r\n"), "{head}");
}

/// Catches: `wait_answering` ignoring its deadline (looping for ever on a store that
/// never starts, which hangs the CI job until its timeout) or reporting success.
#[tokio::test]
async fn wait_answering_gives_up_at_its_deadline() {
    let addr = kbf_testkit::port::dead_addr();
    let s = store(format!("http://{addr}"), Capabilities::default());
    let started = std::time::Instant::now();
    // Bounded here, so a wait that ignores its deadline fails this test, not hangs it.
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        s.wait_answering(Duration::from_millis(700)),
    )
    .await
    .expect("the wait outlived its deadline")
    .unwrap_err();
    let took = started.elapsed();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    assert!(
        took >= Duration::from_millis(700) && took < Duration::from_secs(5),
        "{took:?}"
    );
}

/// Catches a request timeout reported without its cause: a PUT to a store that takes
/// the connection and never reads the body or answers must fail at the request timeout
/// with an error whose text says it timed out, so the failure a lease reports tells a
/// slow store from a reset one ("request to the store failed" alone did not). Also
/// catches the request URL in that text: it reaches REAPI clients, and must not name the
/// store's address, bucket or key.
#[tokio::test]
async fn a_put_the_store_never_answers_fails_saying_it_timed_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let endpoint = format!("http://{addr}");
    // Holds each connection open, unread, until the test ends.
    let held = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });
    let s = S3Store::new(S3Config {
        endpoint,
        region: "us-east-1".into(),
        bucket: "kbf-test".into(),
        credentials: Credentials::new("AKIDEXAMPLE", "secret"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_millis(300),
        min_body_rate: None,
    })
    .unwrap();
    let k = key();
    let put = s.put_new(&k, Bytes::from(vec![0u8; 4 << 20]), None);
    let err = tokio::time::timeout(Duration::from_secs(10), put)
        .await
        .expect("the PUT outlived its request timeout")
        .unwrap_err();
    held.abort();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    let text = err.to_string();
    assert!(
        text.starts_with("request to the store failed: ") && text.contains("timed out"),
        "{text}"
    );
    for private in [addr.as_str(), "kbf-test", "cas/seg-1", "url"] {
        assert!(!text.contains(private), "{private:?} in {text}");
    }
}

/// A store that takes `delay` to answer each request after reading it whole, then says
/// 200 with no body. Returns its endpoint.
async fn slow_store(delay: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 65536];
                let (head_end, body_len) = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + body_len {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                tokio::time::sleep(delay).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 0\r\n\r\n")
                    .await;
                let _ = sock.shutdown().await;
            });
        }
    });
    endpoint
}

fn timed_store(endpoint: String, base: Duration, rate: Option<u64>) -> S3Store {
    S3Store::new(S3Config {
        endpoint,
        region: "us-east-1".into(),
        bucket: "kbf-test".into(),
        credentials: Credentials::new("AKIDEXAMPLE", "secret"),
        capabilities: Capabilities::default(),
        connect_timeout: Duration::from_secs(5),
        request_timeout: base,
        min_body_rate: rate.and_then(std::num::NonZeroU64::new),
    })
    .unwrap()
}

/// Catches: one flat limit for every PUT (the 60 s that cut 16 to 128 MiB segments on a
/// store that was slow but progressing). The store takes 1.2 s over a 256 KiB body, the
/// base limit is 400 ms, and the floor rate of 128 KiB/s allows it 2.4 s more.
#[tokio::test]
async fn a_slow_put_within_the_floor_rate_is_not_cut() {
    let endpoint = slow_store(Duration::from_millis(1200)).await;
    let s = timed_store(endpoint, Duration::from_millis(400), Some(128 << 10));
    s.put_new(&key(), Bytes::from(vec![7u8; 256 << 10]), None)
        .await
        .expect("a PUT inside body/rate + base was cut");
}

/// Catches: the allowance scaled by something other than the body (a fixed bonus, or a
/// rate applied to bodiless requests), and a PUT slower than the floor rate allows
/// being waited for without end. The same slow store, 64 KiB body at 128 KiB/s allows
/// 400 ms + 0.5 s, under the 1.2 s the store takes.
#[tokio::test]
async fn a_put_slower_than_the_floor_rate_still_times_out() {
    let endpoint = slow_store(Duration::from_millis(1200)).await;
    let s = timed_store(endpoint, Duration::from_millis(400), Some(128 << 10));
    let err = s
        .put_new(&key(), Bytes::from(vec![7u8; 64 << 10]), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ObjectStoreError::Transport(_)) && err.to_string().contains("timed out"),
        "{err}"
    );
}

/// Catches: the rate applied to a request with no body, which would give a GET the
/// same room as a PUT. A 1.2 s answer to a bodiless read is cut at the 400 ms base.
#[tokio::test]
async fn a_request_without_a_body_gets_only_the_base_timeout() {
    let endpoint = slow_store(Duration::from_millis(1200)).await;
    let s = timed_store(endpoint, Duration::from_millis(400), Some(1));
    let err = s
        .get_range(&key(), ByteRange::new(0, 4).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
}

/// Catches: a store that reports other capabilities than it was configured with (the
/// conformance suite and the writer pick their paths from them).
#[tokio::test]
async fn a_store_reports_the_capabilities_it_was_configured_with() {
    let claimed = Capabilities {
        conditional_put: true,
        object_lock: false,
    };
    let s = store("http://127.0.0.1:9".into(), claimed);
    assert_eq!(s.capabilities(), claimed);
}

/// Catches: a retained put on a store that did not claim Object Lock sent anyway (the
/// retention header silently ignored by a store that cannot honour it, so the object is
/// deletable while the caller believes it is locked); nothing may reach the wire.
#[tokio::test]
async fn a_retained_put_without_object_lock_is_refused_before_the_wire() {
    let (endpoint, seen) = script(vec![]).await;
    let s = store(endpoint, Capabilities::default());
    let until = std::time::UNIX_EPOCH + Duration::from_secs(4_107_542_400);
    let err = s
        .put_new(&key(), Bytes::from("abc"), Some(until))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ObjectStoreError::Unsupported("Object Lock retention")),
        "{err}"
    );
    assert!(seen.lock().unwrap().is_empty());
}

/// Catches: a 412 on a store with no conditional puts read as "already exists" (it is
/// a service error: nothing sent a condition, so nothing can have failed one).
#[tokio::test]
async fn a_412_on_an_unconditional_put_is_a_service_error() {
    let (endpoint, _) = script(vec![answer(
        "412 Precondition Failed",
        &[],
        "<Error><Code>PreconditionFailed</Code><Message>m</Message></Error>",
    )])
    .await;
    let s = store(endpoint, Capabilities::default());
    let err = s
        .put_new(&key(), Bytes::from("abc"), None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Service { status: 412, code, .. } if code == "PreconditionFailed"),
        "{err}"
    );
}

/// Catches: a ranged read that turns an unexpected status (here 500) into data or into
/// NotFound: it is a `Service` error carrying the status and code.
#[tokio::test]
async fn a_ranged_read_failing_with_a_server_error_is_a_service_error() {
    let (endpoint, _) = script(vec![answer(
        "500 Internal Server Error",
        &[],
        "<Error><Code>InternalError</Code><Message>boom</Message></Error>",
    )])
    .await;
    let s = store(endpoint, Capabilities::default());
    let err = s
        .get_range(&key(), ByteRange::new(0, 4).unwrap())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Service { status: 500, code, message } if code == "InternalError" && message == "boom"),
        "{err}"
    );
}

/// Catches: a ranged read that reads a 404 as a service error (the caller could not
/// tell a lost object from a failing store), or a 416 as anything but `InvalidRange`
/// naming the key and the range asked for.
#[tokio::test]
async fn a_ranged_read_of_a_missing_key_or_past_the_end_is_typed() {
    let (endpoint, _) = script(vec![
        answer(
            "404 Not Found",
            &[],
            "<Error><Code>NoSuchKey</Code><Message>gone</Message></Error>",
        ),
        answer(
            "416 Range Not Satisfiable",
            &[],
            "<Error><Code>InvalidRange</Code><Message>past</Message></Error>",
        ),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let r = ByteRange::new(50, 4).unwrap();
    let err = s.get_range(&key(), r).await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::NotFound(k) if *k == key()),
        "{err}"
    );
    let err = s.get_range(&key(), r).await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::InvalidRange { key: k, range } if *k == key() && *range == r),
        "{err}"
    );
}

/// Catches: a delete on an Object Lock store whose HEAD failed (here 500) carried on to
/// a DELETE blind (it may delete the wrong version, or report success for an object the
/// store refused to describe); the HEAD's error is the answer and no DELETE is sent.
#[tokio::test]
async fn a_failed_head_before_a_locked_delete_stops_the_delete() {
    let (endpoint, seen) = script(vec![answer("500 Internal Server Error", &[], "")]).await;
    let s = store(
        endpoint,
        Capabilities {
            conditional_put: false,
            object_lock: true,
        },
    );
    let err = s.delete(&key()).await.unwrap_err();
    assert!(
        matches!(err, ObjectStoreError::Service { status: 500, .. }),
        "{err}"
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// Catches: the `null` version id (an unversioned object) or an empty one sent as
/// `versionId=...`, which S3 refuses; the DELETE must carry no version query then.
#[tokio::test]
async fn a_locked_delete_of_a_null_or_empty_version_sends_no_version_id() {
    let (endpoint, seen) = script(vec![
        answer("200 OK", &["x-amz-version-id: null"], ""),
        answer("204 No Content", &[], ""),
        answer("200 OK", &["x-amz-version-id:"], ""),
        answer("204 No Content", &[], ""),
        answer("200 OK", &[], ""),
        answer("204 No Content", &[], ""),
    ])
    .await;
    let s = store(
        endpoint,
        Capabilities {
            conditional_put: false,
            object_lock: true,
        },
    );
    for _ in 0..3 {
        s.delete(&key()).await.unwrap();
    }
    let heads = seen.lock().unwrap();
    for i in [1, 3, 5] {
        assert!(
            heads[i].starts_with("DELETE /kbf-test/cas/seg-1 HTTP/1.1"),
            "{}",
            heads[i]
        );
    }
}

/// An endpoint nothing listens on: a port that was bound and then released.
async fn dead_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

/// Catches: an unreachable store reported as success, as an empty result, or as a
/// service answer: every operation, with or without Object Lock, fails `Transport`
/// (the caller retries or fails over on that class, and only on it).
#[tokio::test]
async fn every_operation_against_an_unreachable_store_fails_with_transport() {
    for object_lock in [false, true] {
        let s = store(
            dead_endpoint().await,
            Capabilities {
                conditional_put: false,
                object_lock,
            },
        );
        let range = ByteRange::new(0, 4).unwrap();
        let prefix = KeyPrefix::new("p/").unwrap();
        let errs = [
            s.create_bucket().await.unwrap_err(),
            s.put_new(&key(), Bytes::from("abc"), None)
                .await
                .unwrap_err(),
            s.get_range(&key(), range).await.unwrap_err(),
            s.delete(&key()).await.unwrap_err(),
            s.list(&prefix, None, PageSize::MAX).await.unwrap_err(),
        ];
        for err in errs {
            assert!(
                matches!(&err, ObjectStoreError::Transport(_)),
                "object_lock {object_lock}: {err}"
            );
        }
    }
}

/// An answer that promises 50 bytes, sends 3 and closes.
fn cut_short(status: &str, headers: &[&str]) -> String {
    let mut s = format!("HTTP/1.1 {status}\r\nconnection: close\r\ncontent-length: 50\r\n");
    for h in headers {
        s.push_str(h);
        s.push_str("\r\n");
    }
    s.push_str("\r\nabc");
    s
}

/// Catches: a body cut off by the connection closing taken for the bytes (a truncated
/// listing read as the end of a bucket, a short segment read as the whole): the read
/// and the listing fail `Transport`.
#[tokio::test]
async fn an_answer_cut_short_is_a_transport_error_for_reads_and_listings() {
    let (endpoint, _) = script(vec![
        cut_short("206 Partial Content", &["content-range: bytes 0-49/50"]),
        cut_short("200 OK", &[]),
    ])
    .await;
    let s = store(endpoint, Capabilities::default());
    let err = s
        .get_range(&key(), ByteRange::new(0, 50).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
    let err = s
        .list(&KeyPrefix::new("p/").unwrap(), None, PageSize::MAX)
        .await
        .unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
}

/// Catches: a small streamed write whose body fails still sent (an object stored from a
/// broken source): the body's own error comes back and nothing reaches the wire.
#[tokio::test]
async fn a_small_streamed_write_with_a_failing_body_sends_nothing() {
    let (endpoint, seen) = script(vec![]).await;
    let s = store(endpoint, Capabilities::default());
    let body: kbf_objstore::BodyStream = futures::stream::iter(vec![
        Ok(Bytes::from("ab")),
        Err(ObjectStoreError::Body("the source broke".into())),
    ])
    .boxed();
    let err = s.put_stream(&key(), 4, body).await.unwrap_err();
    assert!(
        matches!(&err, ObjectStoreError::Body(m) if m == "the source broke"),
        "{err}"
    );
    assert!(seen.lock().unwrap().is_empty());
}
