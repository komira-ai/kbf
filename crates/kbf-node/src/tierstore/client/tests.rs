//! [`TierClient`] over its wire, in process ([`crate::tierstore::tests::wire`]): every byte it
//! sends is read by a task of the test, which answers as a tierstore does, as one must not, or
//! not at all. No socket is opened. The deadlines run on tokio's paused clock, which moves only
//! when nothing else can.

use std::time::Duration;

use zeroize::Zeroizing;

use super::*;
use crate::tierstore::loopback;
use crate::tierstore::tests::wire::{Far, Scripted, Store, answer, header};

/// The key pair the store takes.
fn keys() -> KeyPair {
    KeyPair {
        id: "tier-key".to_owned(),
        secret: Zeroizing::new("tier-secret".to_owned()),
    }
}

/// The timeouts of the daemon: connect, request.
const TIMEOUTS: (Duration, Duration) = (Duration::from_secs(5), Duration::from_secs(60));

/// A client of `bucket` at `endpoint`, with these keys, reaching it through `dial`.
fn client_of(endpoint: &str, bucket: &str, keys: KeyPair, dial: &Arc<Scripted>) -> TierClient {
    let endpoint = loopback(endpoint).expect("a loopback endpoint");
    let dial: Arc<dyn Dial> = dial.clone();
    TierClient::new(
        &endpoint,
        bucket.to_owned(),
        keys,
        "us-east-1",
        TIMEOUTS,
        dial,
    )
}

fn key(text: &str) -> ObjectKey {
    ObjectKey::new(text).expect("a key")
}

fn range(offset: u64, len: u64) -> ByteRange {
    ByteRange::new(offset, len).expect("a range")
}

/// A store of bucket `tierstore` that takes the key pair of [`keys`] and holds `0123456789` at
/// `cas/d/n/b/0`.
fn store() -> Arc<Store> {
    let store = Store::new("tierstore", "tier-key", "tier-secret");
    Arc::new(store.holding("cas/d/n/b/0", b"0123456789"))
}

/// Asserts that the client reads a range of an object through a request the store checks the
/// signature of over the request as it received it: the bytes of the range, and a range cut at
/// the object's end; that each read is one connection to the endpoint's address and nothing
/// else; and that the request is a path-style `GET` of the bucket and the key, with the range,
/// and never the secret.
/// Catches a request signed wrongly (the store answers 403), sent elsewhere, or for another
/// bucket, key or range.
#[tokio::test]
async fn a_range_is_read_through_a_signed_request() {
    let dial = Scripted::new(vec![Far::Store(store())]);
    let client = client_of("http://127.0.0.1:9300", "tierstore", keys(), &dial);
    let got = client.get_range(&key("cas/d/n/b/0"), range(2, 3)).await;
    assert_eq!(got.expect("read"), Bytes::from_static(b"234"));
    let cut = client.get_range(&key("cas/d/n/b/0"), range(8, 10)).await;
    assert_eq!(cut.expect("read"), Bytes::from_static(b"89"));
    let at = SocketAddr::from(([127, 0, 0, 1], 9300));
    assert_eq!(dial.addresses(), [at, at]);
    let requests = dial.requests();
    assert!(
        requests[0].starts_with("GET /tierstore/cas/d/n/b/0 HTTP/1.1\r\n"),
        "{}",
        requests[0]
    );
    assert_eq!(header(&requests[0], "range"), Some("bytes=2-4"));
    assert_eq!(header(&requests[1], "range"), Some("bytes=8-17"));
    assert!(
        requests.iter().all(|r| !r.contains("tier-secret")),
        "{requests:?}"
    );
}

/// Asserts that the `Host` the request carries is the one it is signed with, however the
/// endpoint was spelled: an explicit default port, a port left out, an IPv6 address written out
/// in full, and the IPv4-mapped form of a loopback address are each sent as given, and the store,
/// checking the signature over the `Host` it received, takes every one.
/// Catches a `Host` that the HTTP layer rewrites after it was signed, which the tierstore would
/// refuse and so read every blob from the frontend.
#[tokio::test]
async fn the_host_is_sent_as_it_is_signed() {
    let spellings = [
        ("http://127.0.0.1:80", "127.0.0.1:80"),
        ("http://127.0.0.1", "127.0.0.1"),
        ("http://[0:0:0:0:0:0:0:1]:9300", "[0:0:0:0:0:0:0:1]:9300"),
        ("http://[::ffff:127.0.0.1]:9300", "[::ffff:127.0.0.1]:9300"),
        ("http://[::1]:9300/", "[::1]:9300"),
    ];
    for (endpoint, host) in spellings {
        let dial = Scripted::new(vec![Far::Store(store())]);
        let client = client_of(endpoint, "tierstore", keys(), &dial);
        let got = client.get_range(&key("cas/d/n/b/0"), range(0, 1)).await;
        assert_eq!(got.expect(endpoint), Bytes::from_static(b"0"), "{endpoint}");
        assert_eq!(
            header(&dial.requests()[0], "host"),
            Some(host),
            "{endpoint}"
        );
    }
}

/// Asserts the store's answers to what it does not hold or does not take: an object it does not
/// have is `NotFound`, a range past its end is `InvalidRange`, and a key it refuses (another
/// secret) is a `Service` failure with its status and the read named.
#[tokio::test]
async fn what_the_store_refuses_is_said_by_kind() {
    let dial = Scripted::new(vec![Far::Store(store())]);
    let client = client_of("http://127.0.0.1:9300", "tierstore", keys(), &dial);
    let absent = client.get_range(&key("cas/d/n/b/1"), range(0, 1)).await;
    let not_found =
        matches!(&absent, Err(ObjectStoreError::NotFound(k)) if k.as_str() == "cas/d/n/b/1");
    assert!(not_found, "{absent:?}");
    let past = client.get_range(&key("cas/d/n/b/0"), range(10, 1)).await;
    assert!(
        matches!(past, Err(ObjectStoreError::InvalidRange { .. })),
        "{past:?}"
    );
    let wrong = KeyPair {
        id: "tier-key".to_owned(),
        secret: Zeroizing::new("another-secret".to_owned()),
    };
    let refused = client_of("http://127.0.0.1:9300", "tierstore", wrong, &dial);
    let refused = refused.get_range(&key("cas/d/n/b/0"), range(0, 1)).await;
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

/// The client's answer to a read of the first 4 bytes of `cas/x` when the far end is `far`, and
/// the dial that served it.
async fn read_against(far: Far) -> (Result<Bytes, ObjectStoreError>, Arc<Scripted>) {
    let dial = Scripted::new(vec![far]);
    let client = client_of("http://127.0.0.1:9300", "tierstore", keys(), &dial);
    let got = client.get_range(&key("cas/x"), range(0, 4)).await;
    (got, dial)
}

/// Asserts that every answer but the exact range is refused as a protocol failure, naming what is
/// wrong: an answer that ignored `Range` (200), a `206` without `Content-Range`, with one that
/// does not parse or is another range, a body shorter than its range and a body longer than it;
/// and that a well-formed answer is the bytes.
/// Catches bytes other than the range asked for reaching the reader as a read.
#[tokio::test]
async fn only_the_range_asked_for_is_taken() {
    let partial = |range: &str, body: &[u8]| {
        Far::Answer(answer(
            &format!("HTTP/1.1 206 Partial Content\r\ncontent-range: {range}"),
            body,
        ))
    };
    let (good, _) = read_against(partial("bytes 0-3/10", b"abcd")).await;
    assert_eq!(good.expect("read"), Bytes::from_static(b"abcd"));
    let cases: [(Far, &str); 6] = [
        (
            Far::Answer(answer("HTTP/1.1 200 OK", b"abcdefghij")),
            "ranged read of cas/x: answered 200: the store ignored Range",
        ),
        (
            Far::Answer(answer("HTTP/1.1 206 Partial Content", b"abcd")),
            "ranged read of cas/x: 206 without Content-Range",
        ),
        (
            partial("bytes zero/10", b"abcd"),
            "ranged read of cas/x: Content-Range \"bytes zero/10\" does not parse",
        ),
        (
            partial("bytes 1-4/10", b"bcde"),
            "ranged read of cas/x: asked for bytes 0-3 and got Content-Range \"bytes 1-4/10\"",
        ),
        (
            partial("bytes 0-3/10", b"ab"),
            "ranged read of cas/x: the body has 2 of the 4 bytes of its range",
        ),
        (
            partial("bytes 0-3/10", b"abcdefgh"),
            "ranged read of cas/x: the body is longer than the 4 bytes of its range",
        ),
    ];
    for (far, said) in cases {
        let (got, _) = read_against(far).await;
        let Err(ObjectStoreError::Protocol(why)) = got else {
            panic!("{said}: {got:?}");
        };
        assert_eq!(why, said);
    }
}

/// Asserts that a redirect is a failed read and never a second request: one connection is made,
/// to the endpoint, and the failure says redirects are not followed; that another status is a
/// service failure with that status; and that an answer whose body ends early is a transport
/// failure.
/// Catches a tierstore answer that sends the signed request off to another host.
#[tokio::test]
async fn a_redirect_is_not_followed() {
    let redirect = "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://192.0.2.1/tierstore/cas/x";
    let (got, dial) = read_against(Far::Answer(answer(redirect, b""))).await;
    let Err(ObjectStoreError::Protocol(why)) = got else {
        panic!("{got:?}");
    };
    assert_eq!(
        why,
        "ranged read of cas/x: answered 307: redirects are not followed"
    );
    assert_eq!(dial.addresses(), [SocketAddr::from(([127, 0, 0, 1], 9300))]);

    let (got, _) = read_against(Far::Answer(answer("HTTP/1.1 503 Service Unavailable", b""))).await;
    assert!(
        matches!(got, Err(ObjectStoreError::Service { status: 503, .. })),
        "{got:?}"
    );
    // A body of 4 bytes is announced and 2 come before the connection closes.
    let cut = b"HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 0-3/4\r\ncontent-length: 4\r\n\r\nab";
    let (got, _) = read_against(Far::Answer(cut.to_vec())).await;
    assert!(
        matches!(got, Err(ObjectStoreError::Transport(_))),
        "{got:?}"
    );
}

/// Asserts that a tierstore that refuses the connection is a transport failure at once, tried
/// once.
#[tokio::test]
async fn a_tierstore_that_is_down_is_a_transport_failure() {
    let (got, dial) = read_against(Far::Refused).await;
    assert!(
        matches!(got, Err(ObjectStoreError::Transport(_))),
        "{got:?}"
    );
    assert_eq!(dial.addresses().len(), 1);
}

/// The text of a transport failure.
fn transport_text(got: &Result<Bytes, ObjectStoreError>) -> String {
    match got {
        Err(ObjectStoreError::Transport(error)) => error.to_string(),
        other => panic!("not a transport failure: {other:?}"),
    }
}

/// Asserts the request deadline, on the paused clock: a tierstore that takes the request and
/// never answers, and one that answers the head and part of the body and then nothing more, are
/// each a transport failure of the request timeout, saying so: the paused clock moves only to the
/// client's own timer, so it is that timer that ended the read.
/// Catches a read held for good by a tierstore that stops answering.
#[tokio::test(start_paused = true)]
async fn a_read_the_tierstore_stops_answering_ends_at_the_request_timeout() {
    let head =
        "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes 0-3/4\r\ncontent-length: 4\r\n\r\nab";
    for far in [Far::Silent, Far::Stalls(head.as_bytes().to_vec())] {
        let (got, dial) = read_against(far).await;
        assert_eq!(
            transport_text(&got),
            "the tierstore's answer did not come within 60s"
        );
        assert_eq!(dial.requests().len(), 1);
    }
}

/// Asserts the connect deadline, on the paused clock: a connection that is never made is a
/// transport failure of the connect timeout, saying so.
#[tokio::test(start_paused = true)]
async fn a_connection_never_made_ends_at_the_connect_timeout() {
    let (got, _) = read_against(Far::Hangs).await;
    assert_eq!(
        transport_text(&got),
        "the tierstore's connection did not come within 5s"
    );
}

/// Asserts that the client writes, deletes and lists nothing: each is refused as unsupported,
/// and it claims no capability; nothing is dialled.
#[tokio::test]
async fn the_client_only_reads() {
    let dial = Scripted::new(vec![Far::Refused]);
    let client = client_of("http://127.0.0.1:9", "tierstore", keys(), &dial);
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
    assert!(dial.addresses().is_empty());
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
