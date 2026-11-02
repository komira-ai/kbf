//! [`TierClient`] over HTTP: against an S3 fake that checks every signature, and against
//! one-answer servers that answer what a tierstore must not, each on a loopback port of this
//! process that the test binds (no fixed port, no name to resolve).

use std::io::ErrorKind;
use std::time::Duration;

use kbf_objstore::s3::fake::FakeS3;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use zeroize::Zeroizing;

use super::*;
use crate::tierstore::loopback;

/// The key pair the fake takes.
fn keys() -> KeyPair {
    KeyPair {
        id: "tier-key".to_owned(),
        secret: Zeroizing::new("tier-secret".to_owned()),
    }
}

/// A client of `bucket` at `endpoint`, with these keys.
fn client_of(endpoint: &str, bucket: &str, keys: KeyPair) -> TierClient {
    let endpoint = loopback(endpoint).expect("a loopback endpoint");
    let timeouts = (Duration::from_secs(5), Duration::from_secs(60));
    let made = TierClient::new(&endpoint, bucket.to_owned(), keys, "us-east-1", timeouts);
    made.expect("the client")
}

fn key(text: &str) -> ObjectKey {
    ObjectKey::new(text).expect("a key")
}

fn range(offset: u64, len: u64) -> ByteRange {
    ByteRange::new(offset, len).expect("a range")
}

/// The fake, requiring the key pair of [`keys`], holding `bytes` at `cas/d/n/b/0`.
async fn fake_with(bytes: &'static [u8]) -> FakeS3 {
    let fake = FakeS3::start().await;
    let store = fake.store(Capabilities::default());
    let at = key("cas/d/n/b/0");
    store
        .put_new(&at, Bytes::from_static(bytes), None)
        .await
        .expect("seeded");
    fake.require_credentials("tier-key", "tier-secret");
    fake
}

/// Asserts that the client reads a range of an object through the signed request the fake
/// checks (its access key id and the signature over the request as received): the bytes of the
/// range, a range cut at the object's end, and the request the fake saw names the bucket and the
/// key, path-style, with no query (the fake writes each as `METHOD /path?query`).
/// Catches a request signed wrongly (the fake refuses it 403), or one for another bucket, key or
/// range.
#[tokio::test]
async fn a_range_is_read_through_a_signed_request() {
    let fake = fake_with(b"0123456789").await;
    let client = client_of(fake.endpoint(), "kbf-fake", keys());
    let got = client.get_range(&key("cas/d/n/b/0"), range(2, 3)).await;
    assert_eq!(got.expect("read"), Bytes::from_static(b"234"));
    let cut = client.get_range(&key("cas/d/n/b/0"), range(8, 10)).await;
    assert_eq!(cut.expect("read"), Bytes::from_static(b"89"));
    let seen: Vec<String> = fake
        .requests()
        .into_iter()
        .filter(|r| r.starts_with("GET"))
        .collect();
    assert_eq!(
        seen,
        ["GET /kbf-fake/cas/d/n/b/0?", "GET /kbf-fake/cas/d/n/b/0?"]
    );
    assert_eq!(fake.regions(), ["us-east-1"]);
}

/// Asserts the store's answers to what it does not hold or does not take: an object it does not
/// have is `NotFound`, a range past its end is `InvalidRange`, and a key it refuses (another
/// secret) is a `Service` failure with its status, each without the secret in its text.
#[tokio::test]
async fn what_the_store_refuses_is_said_by_kind() {
    let fake = fake_with(b"0123456789").await;
    let client = client_of(fake.endpoint(), "kbf-fake", keys());
    let absent = client.get_range(&key("cas/d/n/b/1"), range(0, 1)).await;
    assert!(
        matches!(&absent, Err(ObjectStoreError::NotFound(k)) if k.as_str() == "cas/d/n/b/1"),
        "{absent:?}"
    );
    let past = client.get_range(&key("cas/d/n/b/0"), range(10, 1)).await;
    assert!(
        matches!(past, Err(ObjectStoreError::InvalidRange { .. })),
        "{past:?}"
    );
    let wrong = KeyPair {
        id: "tier-key".to_owned(),
        secret: Zeroizing::new("another-secret".to_owned()),
    };
    let refused = client_of(fake.endpoint(), "kbf-fake", wrong)
        .get_range(&key("cas/d/n/b/0"), range(0, 1))
        .await;
    let Err(ObjectStoreError::Service {
        status, message, ..
    }) = refused
    else {
        panic!("{refused:?}");
    };
    assert_eq!(
        (status, message.as_str()),
        (403, "ranged read of cas/d/n/b/0 answered 403")
    );
}

/// A server on a loopback port of this process that answers its first request with `answer`
/// and closes; the request it read, once it has answered.
async fn one_answer(answer: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
    one_answer_at("127.0.0.1:0", answer).await
}

/// [`one_answer`], on a port of the loopback address `at`.
async fn one_answer_at(at: &str, answer: Vec<u8>) -> (String, tokio::task::JoinHandle<String>) {
    let listener = TcpListener::bind(at).await.expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    let served = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut request = Vec::new();
        let mut buf = [0_u8; 4096];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut buf).await.expect("read");
            if n == 0 {
                break;
            }
            request.extend_from_slice(&buf[..n]);
        }
        stream.write_all(&answer).await.expect("answer");
        stream.shutdown().await.expect("close");
        String::from_utf8_lossy(&request).into_owned()
    });
    (endpoint, served)
}

/// `head` and `body` as an HTTP/1.1 answer that closes its connection.
fn answer(head: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!(
        "{head}\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// The error the client makes of `answer` to a read of the first 4 bytes of `cas/x`, and the
/// request the server saw.
async fn read_against(answer: Vec<u8>) -> (Result<Bytes, ObjectStoreError>, String) {
    let (endpoint, served) = one_answer(answer).await;
    let client = client_of(&endpoint, "tierstore", keys());
    let got = client.get_range(&key("cas/x"), range(0, 4)).await;
    (got, served.await.expect("served"))
}

/// Asserts that every answer but the exact range is refused as a protocol failure, naming what is
/// wrong: an answer that ignored `Range` (200), a `206` without `Content-Range`, with one that
/// does not parse or is another range, a body shorter than its range and a body longer than it;
/// and that a well-formed answer is the bytes. The request carried the range, its signature and
/// the loopback authority as `Host`.
/// Catches bytes other than the range asked for reaching the reader as a read.
#[tokio::test]
async fn only_the_range_asked_for_is_taken() {
    let (good, request) = read_against(answer(
        "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 0-3/10",
        b"abcd",
    ))
    .await;
    assert_eq!(good.expect("read"), Bytes::from_static(b"abcd"));
    let lower = request.to_ascii_lowercase();
    assert!(
        lower.starts_with("get /tierstore/cas/x http/1.1\r\n"),
        "{request}"
    );
    assert!(lower.contains("\r\nrange: bytes=0-3\r\n"), "{request}");
    assert!(
        lower.contains("\r\nauthorization: aws4-hmac-sha256 credential=tier-key/"),
        "{request}"
    );
    let host = request
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("host:"))
        .expect("host");
    assert!(host[5..].trim().starts_with("127.0.0.1:"), "{host}");
    assert!(!request.contains("tier-secret"), "{request}");

    let cases: [(Vec<u8>, &str); 6] = [
        (
            answer("HTTP/1.1 200 OK", b"abcdefghij"),
            "ranged read of cas/x: answered 200: the store ignored Range",
        ),
        (
            answer("HTTP/1.1 206 Partial Content", b"abcd"),
            "ranged read of cas/x: 206 without Content-Range",
        ),
        (
            answer(
                "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes zero/10",
                b"abcd",
            ),
            "ranged read of cas/x: Content-Range \"bytes zero/10\" does not parse",
        ),
        (
            answer(
                "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 1-4/10",
                b"bcde",
            ),
            "ranged read of cas/x: asked for bytes 0-3 and got Content-Range \"bytes 1-4/10\"",
        ),
        (
            answer(
                "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 0-3/10",
                b"ab",
            ),
            "ranged read of cas/x: the body has 2 of the 4 bytes of its range",
        ),
        (
            answer(
                "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 0-3/10",
                b"abcdefgh",
            ),
            "ranged read of cas/x: the body is longer than the 4 bytes of its range",
        ),
    ];
    for (wire, said) in cases {
        let (got, _) = read_against(wire).await;
        let Err(ObjectStoreError::Protocol(why)) = got else {
            panic!("{said}: {got:?}");
        };
        assert_eq!(why, said);
    }
}

/// Asserts that a redirect is a failed read and never a second request: the client does not
/// connect to the place the answer names (a listener of this process that sees no connection),
/// and the failure says it does not follow redirects; and that another status is a service
/// failure with that status.
/// Catches a tierstore answer that sends the signed request off to another host.
#[tokio::test]
async fn a_redirect_is_not_followed() {
    let elsewhere = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    elsewhere.set_nonblocking(true).expect("nonblocking");
    let to = format!(
        "http://{}/tierstore/cas/x",
        elsewhere.local_addr().expect("address")
    );
    let head = format!("HTTP/1.1 307 Temporary Redirect\r\nlocation: {to}");
    let (got, _) = read_against(answer(&head, b"")).await;
    let Err(ObjectStoreError::Protocol(why)) = got else {
        panic!("{got:?}");
    };
    assert_eq!(
        why,
        "ranged read of cas/x: answered 307: redirects are not followed"
    );
    let accepted = elsewhere.accept().map(|_| ()).map_err(|e| e.kind());
    assert_eq!(accepted, Err(ErrorKind::WouldBlock));

    let (got, _) = read_against(answer("HTTP/1.1 503 Service Unavailable", b"")).await;
    let Err(ObjectStoreError::Service { status, .. }) = got else {
        panic!("{got:?}");
    };
    assert_eq!(status, 503);
}

/// Asserts that a tierstore that is not listening is a transport failure at once, tried once:
/// the port a listener of this process held and gave back refuses the connection.
#[tokio::test]
async fn a_tierstore_that_is_down_is_a_transport_failure() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let endpoint = format!("http://{}", listener.local_addr().expect("address"));
    drop(listener);
    let client = client_of(&endpoint, "tierstore", keys());
    let got = client.get_range(&key("cas/x"), range(0, 4)).await;
    assert!(
        matches!(got, Err(ObjectStoreError::Transport(_))),
        "{got:?}"
    );
}

/// Asserts that the client writes, deletes and lists nothing: each is refused as unsupported,
/// and it claims no capability.
#[tokio::test]
async fn the_client_only_reads() {
    let client = client_of("http://127.0.0.1:9", "tierstore", keys());
    let unsupported = |result: Result<(), ObjectStoreError>| {
        matches!(result, Err(ObjectStoreError::Unsupported(READ_ONLY)))
    };
    assert!(unsupported(
        client.put_new(&key("cas/x"), Bytes::new(), None).await
    ));
    assert!(unsupported(client.delete(&key("cas/x")).await));
    let prefix = KeyPrefix::new("cas/").expect("a prefix");
    let size = PageSize::new(1).expect("a page size");
    let listed = client.list(&prefix, None, size).await.map(drop);
    assert!(unsupported(listed));
    assert_eq!(client.capabilities(), Capabilities::default());
}

/// The test the proxy test runs in a process of its own, with the proxy variables set.
const PROXIED: &str =
    "tierstore::client::tests::with_proxy_variables_set_the_read_goes_to_the_tierstore";

/// Run by [`a_proxy_in_the_environment_is_not_used`] in a process whose environment names a proxy
/// for every scheme: the read must reach the tierstore it names, not the proxy.
#[ignore = "run by a_proxy_in_the_environment_is_not_used, in a process with proxy variables set"]
#[tokio::test]
async fn with_proxy_variables_set_the_read_goes_to_the_tierstore() {
    let (got, request) = read_against(answer(
        "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 0-3/4",
        b"abcd",
    ))
    .await;
    assert_eq!(got.expect("read"), Bytes::from_static(b"abcd"));
    assert!(request.starts_with("GET /tierstore/cas/x "), "{request}");
}

/// Asserts that the proxy variables of the environment (`HTTP_PROXY`, `ALL_PROXY`, both
/// spellings) do not take the request: a process of this test binary, given them all naming a
/// listener here, reads from the tierstore it was given ([`PROXIED`] passes), and the listener
/// sees no connection.
/// Catches a client that would send the signed request through a proxy off the host.
#[test]
fn a_proxy_in_the_environment_is_not_used() {
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    proxy.set_nonblocking(true).expect("nonblocking");
    let at = format!("http://{}", proxy.local_addr().expect("address"));
    let exe = std::env::current_exe().expect("the test binary");
    let mut child = std::process::Command::new(exe);
    child.args(["--exact", PROXIED, "--ignored", "--test-threads=1"]);
    for name in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
        child.env(name, &at);
    }
    let out = child
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .output()
        .expect("the child runs");
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(said.contains("1 passed"), "{said}");
    let accepted = proxy.accept().map(|_| ()).map_err(|e| e.kind());
    assert_eq!(accepted, Err(ErrorKind::WouldBlock));
}

/// Asserts the HMAC against RFC 4231's test cases 2 (a short key) and 6 (a key longer than a
/// block, hashed first), and a key of exactly a block.
#[test]
fn hmac_is_hmac_sha256() {
    let hex_of = |key: &[u8], data: &[u8]| hex::encode(hmac(key, data));
    assert_eq!(
        hex_of(b"Jefe", b"what do ya want for nothing?"),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    assert_eq!(
        hex_of(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        ),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
    assert_eq!(
        hex_of(&[b'k'; 64], b"x"),
        "f91c4c403625fb06910ef93999265bbd2d62baeaec6ff36455498cc124fe3e66"
    );
}

/// Asserts `x-amz-date` on an ordinary day, on a leap day, and on the first day of a month
/// after a February a century year makes short, and that a time before the epoch is spelled as
/// the epoch is.
#[test]
fn the_date_is_spelled_as_s3_signs_it() {
    let at = |secs: u64| amz_date(UNIX_EPOCH + Duration::from_secs(secs));
    assert_eq!(at(1_793_622_896), "20261102T123456Z");
    assert_eq!(at(1_835_481_599), "20280229T235959Z");
    assert_eq!(at(4_107_542_400), "21000301T000000Z");
    assert_eq!(amz_date(UNIX_EPOCH - Duration::from_secs(1)), at(0));
}

/// Asserts the `Content-Range` of a `206`: the range asked for, cut at the object's end, is its
/// length; a range that ends elsewhere, and one whose end is before its start, are refused.
#[test]
fn content_range_is_the_range_asked_for() {
    assert_eq!(partial(Some("bytes 0-3/10"), range(0, 4)), Ok(4));
    assert_eq!(partial(Some("bytes 8-9/10"), range(8, 10)), Ok(2));
    let short = partial(Some("bytes 0-2/10"), range(0, 4));
    assert_eq!(
        short,
        Err("asked for bytes 0-3 and got Content-Range \"bytes 0-2/10\"".to_owned())
    );
    // The object ends before the range starts: its last byte is before the first asked for.
    let before = partial(Some("bytes 5-2/3"), range(5, 1));
    assert_eq!(
        before,
        Err("asked for bytes 5-5 and got Content-Range \"bytes 5-2/3\"".to_owned())
    );
}

/// Asserts that a tierstore on the IPv6 loopback is read as one on IPv4 is: the request goes to
/// `[::1]` and names it, bracketed with its port, as its `Host`.
#[tokio::test]
async fn a_tierstore_on_the_ipv6_loopback_is_read() {
    let wire = answer(
        "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 0-3/4",
        b"abcd",
    );
    let (endpoint, served) = one_answer_at("[::1]:0", wire).await;
    assert!(endpoint.starts_with("http://[::1]:"), "{endpoint}");
    let client = client_of(&endpoint, "tierstore", keys());
    let got = client.get_range(&key("cas/x"), range(0, 4)).await;
    assert_eq!(got.expect("read"), Bytes::from_static(b"abcd"));
    let request = served.await.expect("served");
    let host = request
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("host:"))
        .expect("host");
    assert_eq!(host[5..].trim(), &endpoint[7..]);
}
