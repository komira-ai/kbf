//! The calls of a multipart upload one at a time (`create_multipart`, `upload_part_stream`,
//! `complete_multipart`, `abort_multipart`) against the loopback S3 service of `s3::fake`,
//! and against scripted servers that show what is on the wire: the headers of each request,
//! the body of a part as it is sent, and the limits of a request that makes no progress.

use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::channel::oneshot;
use futures::stream;
use kbf_objstore::s3::fake::{Failure, FakeS3, part_etag};
use kbf_objstore::s3::{Credentials, S3Config, S3Store};
use kbf_objstore::{BodyStream, ByteRange, Capabilities, ObjectKey, ObjectStore, ObjectStoreError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CONDITIONAL: Capabilities = Capabilities {
    conditional_put: true,
    object_lock: false,
};

fn key(name: &str) -> ObjectKey {
    ObjectKey::new(format!("cas/{name}")).unwrap()
}

fn ok(text: &'static str) -> Result<Bytes, ObjectStoreError> {
    Ok(Bytes::from_static(text.as_bytes()))
}

/// A body of these pieces.
fn pieces(texts: &[&'static str]) -> BodyStream {
    let mut items = Vec::new();
    for &text in texts {
        items.push(ok(text));
    }
    stream::iter(items).boxed()
}

/// A store at `endpoint` whose requests may stall for `request_timeout`.
fn store(endpoint: &str, capabilities: Capabilities, request_timeout: Duration) -> S3Store {
    S3Store::new(S3Config {
        endpoint: endpoint.to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "kbf-test".to_owned(),
        credentials: Credentials::new("AKIDEXAMPLE", "secret"),
        capabilities,
        connect_timeout: Duration::from_secs(5),
        request_timeout,
        min_body_rate: None,
    })
    .unwrap()
}

/// Uploads the part `number` of the upload `id` of `key`, of `text`.
async fn part(
    store: &S3Store,
    key: &ObjectKey,
    id: &str,
    number: u32,
    text: &'static str,
) -> Result<String, ObjectStoreError> {
    let body = pieces(&[text]);
    let sending = store.upload_part_stream(key, id, number, text.len() as u64, body);
    sending.await
}

/// The status and the S3 code of a refusal of the store.
fn refusal<T: Debug>(result: Result<T, ObjectStoreError>) -> (u16, String) {
    match result {
        Err(ObjectStoreError::Service { status, code, .. }) => (status, code),
        other => panic!("not a refusal of the store: {other:?}"),
    }
}

/// The refusal with this status and code, as [`refusal`] gives it.
fn s3_error(status: u16, code: &str) -> (u16, String) {
    (status, code.to_owned())
}

/// Asserts that parts sent as streams of pieces are stored with the tag of their bytes, and
/// that a completion names them by the numbers they were sent with, in the order given and not
/// by position: the object is the parts joined, with the tag the completion answered (in
/// quotes, ending with the number of parts), no upload is left open, and nothing was sent as
/// a single PUT. The service checks the signature of every request, which has no hash of its
/// body.
#[tokio::test]
async fn parts_are_streamed_stored_and_completed_by_their_numbers() {
    let s3 = FakeS3::start().await;
    s3.set_min_part_bytes(0);
    s3.require_credentials("fake-key", "fake-secret");
    let store = s3.store(CONDITIONAL);
    let k = key("joined");
    let id = store.create_multipart(&k).await.unwrap();
    let third = store
        .upload_part_stream(&k, &id, 3, 5, pieces(&["wo", "", "rld"]))
        .await
        .unwrap();
    let first = store
        .upload_part_stream(&k, &id, 1, 6, pieces(&["hello ", ""]))
        .await
        .unwrap();
    assert_eq!(third, part_etag(b"world"));
    assert_eq!(first, part_etag(b"hello "));
    let parts = [(1, first), (3, third)];
    let made = store.complete_multipart(&k, &id, &parts).await.unwrap();
    assert_eq!(store.head(&k).await.unwrap().etag, made);
    assert!(made.ends_with("-2\""), "{made}");
    let all = ByteRange::new(0, 11).unwrap();
    assert_eq!(store.get_range(&k, all).await.unwrap(), "hello world");
    let stats = s3.stats();
    let counts = (stats.parts, stats.completed, stats.single_puts);
    assert_eq!(counts, (2, 1, 0));
    assert_eq!(s3.incomplete_uploads(), 0);
}

/// Asserts that a part sent again with the same number replaces the part: a completion that
/// names the tag of the first one is refused as `InvalidPart`, and one that names the second
/// makes the object of the second.
#[tokio::test]
async fn a_part_sent_again_replaces_the_part() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("replaced");
    let id = store.create_multipart(&k).await.unwrap();
    let old = part(&store, &k, &id, 1, "old").await.unwrap();
    let new = part(&store, &k, &id, 1, "new!").await.unwrap();
    assert_ne!(old, new);
    let stale = store.complete_multipart(&k, &id, &[(1, old)]).await;
    assert_eq!(refusal(stale), s3_error(400, "InvalidPart"));
    let parts = [(1, new)];
    store.complete_multipart(&k, &id, &parts).await.unwrap();
    assert_eq!(s3.object_len(&k), Some(4));
}

/// Asserts that a part, a completion and an abort on an upload the service does not hold, or
/// holds for another key, are refused as `NoSuchUpload`, and that an abort ends an upload and
/// cannot be done twice.
#[tokio::test]
async fn calls_on_an_upload_the_store_does_not_hold_are_refused() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let (k, other) = (key("held"), key("other"));
    let id = store.create_multipart(&k).await.unwrap();
    let tag = part(&store, &k, &id, 1, "a").await.unwrap();
    let never = part(&store, &k, "never", 1, "a").await;
    assert_eq!(refusal(never), s3_error(404, "NoSuchUpload"));
    let elsewhere = part(&store, &other, &id, 1, "a").await;
    assert_eq!(refusal(elsewhere), s3_error(404, "NoSuchUpload"));
    let abort_never = store.abort_multipart(&k, "never").await;
    assert_eq!(refusal(abort_never), s3_error(404, "NoSuchUpload"));
    let abort_elsewhere = store.abort_multipart(&other, &id).await;
    assert_eq!(refusal(abort_elsewhere), s3_error(404, "NoSuchUpload"));
    let parts = [(1, tag)];
    let complete_never = store.complete_multipart(&k, "never", &parts).await;
    assert_eq!(refusal(complete_never), s3_error(400, "NoSuchUpload"));
    let complete_elsewhere = store.complete_multipart(&other, &id, &parts).await;
    assert_eq!(refusal(complete_elsewhere), s3_error(400, "NoSuchUpload"));
    assert_eq!(s3.incomplete_uploads(), 1);
    store.abort_multipart(&k, &id).await.unwrap();
    assert_eq!(s3.incomplete_uploads(), 0);
    let twice = store.abort_multipart(&k, &id).await;
    assert_eq!(refusal(twice), s3_error(404, "NoSuchUpload"));
}

/// Asserts that a completion over an existing key is refused as `AlreadyExists` on a store
/// that claims conditional writes, leaves the object as it was and the upload open until it
/// is aborted, and that on a store that does not claim them it replaces the object.
#[tokio::test]
async fn a_completion_never_replaces_an_object_where_writes_are_conditional() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("taken");
    let first = Bytes::from_static(b"first");
    store.put_new(&k, first, None).await.unwrap();
    let id = store.create_multipart(&k).await.unwrap();
    let tag = part(&store, &k, &id, 1, "second").await.unwrap();
    let refused = store.complete_multipart(&k, &id, &[(1, tag)]).await;
    assert!(
        matches!(refused, Err(ObjectStoreError::AlreadyExists(_))),
        "{refused:?}"
    );
    assert_eq!((s3.object_len(&k), s3.incomplete_uploads()), (Some(5), 1));
    store.abort_multipart(&k, &id).await.unwrap();
    assert_eq!((s3.incomplete_uploads(), s3.stats().aborted), (0, 1));

    let plain = s3.store(Capabilities::default());
    let id = plain.create_multipart(&k).await.unwrap();
    let tag = part(&plain, &k, &id, 1, "second").await.unwrap();
    let parts = [(1, tag)];
    plain.complete_multipart(&k, &id, &parts).await.unwrap();
    assert_eq!(s3.object_len(&k), Some(6));
}

/// Asserts that a part whose body fails, also after its last byte, or whose body is shorter
/// or longer than its length, fails the call with the error of the body and is not stored: the
/// service holds no part afterwards. A body that fails after all its bytes must not be taken
/// for a whole part.
#[tokio::test]
async fn a_body_that_fails_or_has_the_wrong_length_leaves_no_part() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("cut");
    let id = store.create_multipart(&k).await.unwrap();
    let broke = || ObjectStoreError::Body("the source broke".to_owned());
    let cases = [
        (4, vec![ok("ab"), ok("cd"), Err(broke())]),
        (4, vec![ok("ab"), Err(broke())]),
        (5, vec![ok("ab"), ok("cd")]),
        (3, vec![ok("ab"), ok("cd")]),
    ];
    for (case, (len, items)) in cases.into_iter().enumerate() {
        let body = stream::iter(items).boxed();
        let sent = store.upload_part_stream(&k, &id, 1, len, body).await;
        let failed = matches!(sent, Err(ObjectStoreError::Body(_)));
        assert!(failed, "case {case}: {sent:?}");
    }
    assert_eq!(s3.stats().parts, 0);
}

/// Asserts that a store that cannot be reached is a transport error, and not an error of the
/// body that the request was to send.
#[tokio::test]
async fn a_store_that_cannot_be_reached_is_a_transport_error() {
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let store = store(&endpoint, CONDITIONAL, Duration::from_secs(5));
    let k = key("k");
    let sent = store.upload_part_stream(&k, "u-1", 1, 3, pieces(&["abc"]));
    let error = sent.await.unwrap_err();
    assert!(matches!(error, ObjectStoreError::Transport(_)), "{error:?}");
}

/// Asserts that each step the service can fail is reported as it is: a refused create, a
/// refused part, a part answered without an `ETag` (a protocol error), a refused completion,
/// a completion answered 200 with an error, and a refused abort.
#[tokio::test]
async fn the_failures_of_the_store_are_reported() {
    let s3 = FakeS3::start().await;
    let store = s3.store(CONDITIONAL);
    let k = key("failing");
    s3.fail(Failure::Create);
    let created = store.create_multipart(&k).await;
    assert_eq!(refusal(created), s3_error(500, "InternalError"));
    let id = store.create_multipart(&k).await.unwrap();

    s3.fail(Failure::Part(2));
    let refused = part(&store, &k, &id, 2, "b").await;
    assert_eq!(refusal(refused), s3_error(500, "InternalError"));
    s3.fail(Failure::NoEtag);
    let bare = part(&store, &k, &id, 1, "a").await.unwrap_err();
    assert!(matches!(bare, ObjectStoreError::Protocol(_)), "{bare:?}");
    let text = bare.to_string();
    assert!(text.contains("part 1 of cas/failing"), "{text}");
    let tag = part(&store, &k, &id, 1, "a").await.unwrap();

    let parts = [(1, tag)];
    s3.fail(Failure::Complete);
    let refused = store.complete_multipart(&k, &id, &parts).await;
    assert_eq!(refusal(refused), s3_error(500, "InternalError"));
    s3.fail(Failure::CompleteLate);
    let late = store.complete_multipart(&k, &id, &parts).await;
    assert_eq!(refusal(late), s3_error(200, "InternalError"));
    s3.fail(Failure::Abort);
    let refused = store.abort_multipart(&k, &id).await;
    assert_eq!(refusal(refused), s3_error(500, "InternalError"));
    assert_eq!(s3.incomplete_uploads(), 1);
}

/// What a scripted server saw of a request: its head, and its body.
struct Seen {
    head: String,
    body: Vec<u8>,
}

/// The `Content-Length` a head declares.
fn declared_length(head: &str) -> usize {
    head.lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0)
}

/// Reads from `sock` until the head of a request is there, and gives where the head ends and
/// everything read so far.
async fn read_head(sock: &mut TcpStream) -> (usize, Vec<u8>) {
    let mut buf = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let n = sock.read(&mut chunk).await.unwrap();
        assert!(n > 0, "the client closed before it sent a request");
        buf.extend_from_slice(&chunk[..n]);
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            return (at + 4, buf);
        }
    }
}

/// Reads a whole request: its head, and as many bytes of body as it declares.
async fn read_request(sock: &mut TcpStream) -> Seen {
    let (head_end, mut buf) = read_head(sock).await;
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let wanted = declared_length(&head);
    let mut chunk = [0_u8; 4096];
    while buf.len() < head_end + wanted {
        let n = sock.read(&mut chunk).await.unwrap();
        assert!(n > 0, "the client closed before it sent its body");
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = buf[head_end..].to_vec();
    Seen { head, body }
}

/// Serves `responses` in order, one per connection, and records each request.
async fn script(responses: Vec<String>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        for resp in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            let request = read_request(&mut sock).await;
            log.lock().unwrap().push(request);
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

const CREATED: &str = "<InitiateMultipartUploadResult><UploadId>u-1</UploadId>\
                       </InitiateMultipartUploadResult>";

const COMPLETED: &str = "<CompleteMultipartUploadResult><ETag>\"x\"</ETag>\
                         </CompleteMultipartUploadResult>";

/// Asserts what each call sends: a create is a POST with `uploads`; a part is a PUT with its
/// number and the upload id, the length of the part as its `Content-Length` (and no chunked
/// transfer coding), `UNSIGNED-PAYLOAD` as the hash of the body, which is signed with the
/// headers of any other request, and exactly the bytes of the pieces as its body; a
/// completion is a POST with the upload id, `If-None-Match: *`, and the parts under the
/// numbers it was given with their tags, and gives the `ETag` its answer names; an abort is a
/// DELETE with the upload id. The tag of a part is given with its quotes.
#[tokio::test]
async fn each_call_sends_the_request_of_its_name() {
    let (endpoint, seen) = script(vec![
        answer("200 OK", &[], CREATED),
        answer("200 OK", &["etag: \"p2\""], ""),
        answer("200 OK", &[], COMPLETED),
        answer("204 No Content", &[], ""),
    ])
    .await;
    let store = store(&endpoint, CONDITIONAL, Duration::from_secs(5));
    let k = key("k");
    let id = store.create_multipart(&k).await.unwrap();
    assert_eq!(id, "u-1");
    let body = pieces(&["hello", " ", "world"]);
    let sending = store.upload_part_stream(&k, &id, 2, 11, body);
    let tag = sending.await.unwrap();
    assert_eq!(tag, "\"p2\"");
    let parts = [(2, tag), (5, "\"p5\"".to_owned())];
    let made = store.complete_multipart(&k, &id, &parts).await.unwrap();
    assert_eq!(made, "\"x\"");
    store.abort_multipart(&k, &id).await.unwrap();

    let seen = seen.lock().unwrap();
    let heads: Vec<String> = seen.iter().map(|s| s.head.to_ascii_lowercase()).collect();
    let create = "post /kbf-test/cas/k?uploads= http/1.1\r\n";
    assert!(heads[0].starts_with(create), "{}", heads[0]);
    let put = "put /kbf-test/cas/k?partnumber=2&uploadid=u-1 http/1.1\r\n";
    assert!(heads[1].starts_with(put), "{}", heads[1]);
    let length = "\r\ncontent-length: 11\r\n";
    assert!(heads[1].contains(length), "{}", heads[1]);
    let unsigned = "\r\nx-amz-content-sha256: unsigned-payload\r\n";
    assert!(heads[1].contains(unsigned), "{}", heads[1]);
    assert!(!heads[1].contains("transfer-encoding"), "{}", heads[1]);
    let signed = "signedheaders=host;x-amz-content-sha256;x-amz-date";
    assert!(heads[1].contains(signed), "{}", heads[1]);
    assert_eq!(seen[1].body, b"hello world");
    let post = "post /kbf-test/cas/k?uploadid=u-1 http/1.1\r\n";
    assert!(heads[2].starts_with(post), "{}", heads[2]);
    let condition = "\r\nif-none-match: *\r\n";
    assert!(heads[2].contains(condition), "{}", heads[2]);
    let xml = "\r\ncontent-type: application/xml\r\n";
    assert!(heads[2].contains(xml), "{}", heads[2]);
    let listed = String::from_utf8_lossy(&seen[2].body).into_owned();
    let named = "<Part><PartNumber>2</PartNumber><ETag>\"p2\"</ETag></Part>\
                 <Part><PartNumber>5</PartNumber><ETag>\"p5\"</ETag></Part>";
    assert!(listed.contains(named), "{listed}");
    let delete = "delete /kbf-test/cas/k?uploadid=u-1 http/1.1\r\n";
    assert!(heads[3].starts_with(delete), "{}", heads[3]);
}

/// Asserts what the answers to a completion and an abort mean: a 412 to a completion is
/// `AlreadyExists`, a 404 to an abort is the store's refusal with its code, and a store that
/// does not claim conditional writes is not sent `If-None-Match`.
#[tokio::test]
async fn a_precondition_failed_and_a_missing_upload_are_told_apart() {
    let missing = "<Error><Code>NoSuchUpload</Code><Message>m</Message></Error>";
    let (endpoint, seen) = script(vec![
        answer("412 Precondition Failed", &[], ""),
        answer("404 Not Found", &[], missing),
        answer("200 OK", &[], COMPLETED),
    ])
    .await;
    let k = key("k");
    let claims = store(&endpoint, CONDITIONAL, Duration::from_secs(5));
    let parts = [(1, "\"a\"".to_owned())];
    let refused = claims.complete_multipart(&k, "u-1", &parts).await;
    assert!(matches!(refused, Err(ObjectStoreError::AlreadyExists(_))));
    let gone = claims.abort_multipart(&k, "u-1").await;
    assert_eq!(refusal(gone), s3_error(404, "NoSuchUpload"));
    let plain = store(&endpoint, Capabilities::default(), Duration::from_secs(5));
    plain.complete_multipart(&k, "u-1", &parts).await.unwrap();

    let seen = seen.lock().unwrap();
    let conditional = seen[0].head.to_ascii_lowercase();
    assert!(conditional.contains("\r\nif-none-match: *\r\n"));
    let unconditional = seen[2].head.to_ascii_lowercase();
    assert!(!unconditional.contains("if-none-match"));
}

/// Asserts that a completion answered 200 with a result that names no `ETag` is a protocol
/// error that names the key and says the tag is missing: the object is made, and the answer
/// is all the call has of its tag.
#[tokio::test]
async fn a_completion_answered_without_an_etag_is_a_protocol_error() {
    let bare = "<CompleteMultipartUploadResult></CompleteMultipartUploadResult>";
    let (endpoint, _) = script(vec![answer("200 OK", &[], bare)]).await;
    let store = store(&endpoint, CONDITIONAL, Duration::from_secs(5));
    let (k, parts) = (key("k"), [(1, "\"a\"".to_owned())]);
    let completing = store.complete_multipart(&k, "u-1", &parts);
    let error = completing.await.unwrap_err();
    assert!(matches!(error, ObjectStoreError::Protocol(_)), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("cas/k"), "{text}");
    assert!(text.contains("no ETag"), "{text}");
}

/// Asserts that a part is on the wire before its stream has ended, which is the reason it is
/// streamed: the server has the first piece, and only then does the stream go on to give the
/// last, so a request that held the whole part before it sent any of it would wait for ever.
/// The 60 s `timeout` around the call only stops that hang; the test does not wait for it.
#[tokio::test]
async fn a_part_is_on_the_wire_before_its_stream_has_ended() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (open, gate) = oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let (head_end, mut buf) = read_head(&mut sock).await;
        let mut opened = Some(open);
        let mut chunk = [0_u8; 4096];
        while buf.len() < head_end + 12 {
            let first_piece = buf.len() >= head_end + 4;
            if let Some(open) = opened.take_if(|_| first_piece) {
                open.send(()).unwrap();
            }
            let n = sock.read(&mut chunk).await.unwrap();
            assert!(n > 0, "the client closed before it sent its body");
            buf.extend_from_slice(&chunk[..n]);
        }
        let reply = answer("200 OK", &["etag: \"streamed\""], "");
        sock.write_all(reply.as_bytes()).await.unwrap();
        sock.shutdown().await.unwrap();
        buf[head_end..].to_vec()
    });
    let last = async move {
        gate.await.unwrap();
        ok("CCCC")
    };
    let first = stream::iter(vec![ok("AAAA"), ok("BBBB")]);
    let body = first.chain(stream::once(last)).boxed();
    let store = store(&endpoint, CONDITIONAL, Duration::from_secs(30));
    let k = key("streamed");
    let sending = store.upload_part_stream(&k, "u-1", 1, 12, body);
    let sent = tokio::time::timeout(Duration::from_secs(60), sending).await;
    let tag = sent.expect("the part was held back").unwrap();
    assert_eq!(tag, "\"streamed\"");
    assert_eq!(serving.await.unwrap(), b"AAAABBBBCCCC");
}

/// Asserts that a request that makes no progress is cut after a `request_timeout` without any:
/// a store that takes the whole part and never answers, and one that answers 500 and never
/// sends the error it promised, each fail as a transport error that says no byte arrived
/// within the request timeout, and not as the refusal of the store or an error of the body.
#[tokio::test]
async fn a_request_that_makes_no_progress_is_cut_as_a_timeout() {
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_at = format!("http://{}", silent.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut sock, _) = silent.accept().await.unwrap();
        let mut sink = [0_u8; 4096];
        while sock.read(&mut sink).await.unwrap_or(0) > 0 {}
    });
    let hung = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hung_at = format!("http://{}", hung.local_addr().unwrap());
    tokio::spawn(async move {
        let (mut sock, _) = hung.accept().await.unwrap();
        read_request(&mut sock).await;
        let head = "HTTP/1.1 500 Oops\r\ncontent-length: 100\r\n\r\n";
        sock.write_all(head.as_bytes()).await.unwrap();
        std::future::pending::<()>().await;
    });
    let k = key("k");
    for endpoint in [silent_at, hung_at] {
        let store = store(&endpoint, CONDITIONAL, Duration::from_millis(200));
        let sent = store.upload_part_stream(&k, "u-1", 1, 3, pieces(&["abc"]));
        let error = sent.await.unwrap_err();
        let transport = matches!(error, ObjectStoreError::Transport(_));
        assert!(transport, "{error:?}");
        let text = error.to_string();
        let stalled = "no byte arrived within the request timeout";
        assert!(text.contains(stalled), "{text}");
    }
}

/// Asserts that a source that is slow to give a piece is not a stalled request: the source
/// gives its first piece and then holds its second until the test has seen the request wait
/// for it (the source has been asked for the piece), then lets it go only after three times
/// the request timeout has passed; the part is then stored whole, with the tag of all its
/// bytes. The wait is for the source, whose own limit is its owner's. The clock is paused
/// once the upload is created, so the wait is virtual time and takes no real time, and the
/// 60 s `timeout` around the call is virtual too: it only stops a hang.
#[tokio::test]
async fn a_slow_source_does_not_stall_the_request() {
    let s3 = FakeS3::start().await;
    let store = store(s3.endpoint(), CONDITIONAL, Duration::from_millis(200));
    let k = key("slow");
    let id = store.create_multipart(&k).await.unwrap();
    tokio::time::pause();
    let (asked, on_source) = oneshot::channel::<()>();
    let (release, gate) = oneshot::channel::<()>();
    let late = async move {
        asked.send(()).unwrap();
        gate.await.unwrap();
        ok("cd")
    };
    let body = stream::once(async { ok("ab") }).chain(stream::once(late));
    let sending = store.upload_part_stream(&k, &id, 1, 4, body.boxed());
    let letting_go = async move {
        on_source.await.unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        release.send(()).unwrap();
    };
    let both = futures::future::join(sending, letting_go);
    let done = tokio::time::timeout(Duration::from_secs(60), both).await;
    let (sent, ()) = done.expect("the part was held back");
    assert_eq!(sent.unwrap(), part_etag(b"abcd"));
    assert_eq!(s3.stats().parts, 1);
}
