//! What the server does with a request that carries a read grant, through `Server::handle`, on a
//! storage front and on the front of a cache.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, StatusCode};

use crate::{Backend, Op, TestClock};

use super::auth::sign;
use super::grant::{GRANT_HEADER, MAX_TOKEN_BYTES, mint_grant};
use super::local_cache::testing::Rig as CacheRig;
use super::testing::{
    BUCKET, KEY_ID, NOW, Reply, Rig, SECRET as FULL_SECRET, Scripted, Step, texts,
};
use super::{AccessKey, Server, ServerConfig};

const MEBIBYTE: u64 = 1 << 20;

/// A call to try: its method, its URI and the headers it carries.
type Call<'a> = (&'a str, String, &'a [(&'a str, &'a str)]);

const ID: &str = "build-grant";
const SECRET: &str = "grant-secret-0123456789";

/// Ten minutes after the clock of the rig.
const EXPIRES: u64 = NOW / 1000 + 600;

/// Ten bytes of text, so that a range is read back as it is.
const TEXT: &str = "0123456789";

fn key() -> AccessKey {
    AccessKey::new(ID, SECRET)
}

/// The token of a grant for the bucket of the rig that lists `names` and ends at `EXPIRES`.
fn token(names: &[&str]) -> String {
    mint_grant(&key(), BUCKET, EXPIRES, names.iter().copied()).unwrap()
}

fn uri(key: &str) -> String {
    format!("/{BUCKET}/{key}")
}

/// A server with the grant key `ID`, the time set by hand, and these objects, created with the
/// full key.
struct Fixture {
    rig: Rig,
    clock: Arc<TestClock>,
}

async fn fixture() -> Fixture {
    let clock = Arc::new(TestClock::new());
    clock.set(NOW);
    let shared = Arc::clone(&clock);
    let made = Rig::with(MEBIBYTE, move |config| {
        config.clock = shared;
        config.grant_keys = vec![key()];
    });
    let rig = made.await;
    for object in ["cas/b/pack-a", "cas/l/blob-1", "cas/b/other"] {
        assert_eq!(
            rig.put(object, TEXT.as_bytes()).await.status,
            StatusCode::OK
        );
    }
    Fixture { rig, clock }
}

/// A request for the grant `token`, built by hand: no signature, no body.
fn request(token: &str, method: &str, uri: &str, extra: &[(&str, &str)]) -> Request<Full<Bytes>> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "tier.test")
        .header(GRANT_HEADER, token);
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    builder.body(Full::new(Bytes::new())).unwrap()
}

impl Fixture {
    async fn with_grant(
        &self,
        token: &str,
        method: &str,
        uri: &str,
        extra: &[(&str, &str)],
    ) -> Reply {
        self.rig.send_raw(request(token, method, uri, extra)).await
    }
}

fn refused(reply: &Reply) -> (u16, String) {
    (reply.status.as_u16(), reply.code())
}

fn access_denied() -> (u16, String) {
    (403, "AccessDenied".to_owned())
}

/// The text of the error: what it says, apart from the request it answers.
fn message(reply: &Reply) -> String {
    texts(&reply.body, "Message").concat()
}

/// Asserts that a grant reads what it lists: GetObject whole, by a range and by a suffix range,
/// HeadObject, and a get that carries the parameter the AWS SDKs add (`x-id`), for each object
/// of the grant, and that the answers are those of the full key.
#[tokio::test(start_paused = true)]
async fn a_grant_reads_the_objects_it_lists_whole_or_by_range() {
    let f = fixture().await;
    let t = token(&["cas/b/pack-a", "cas/l/blob-1"]);
    for key in ["cas/b/pack-a", "cas/l/blob-1"] {
        let whole = f.with_grant(&t, "GET", &uri(key), &[]).await;
        assert_eq!((whole.status, whole.body.as_str()), (StatusCode::OK, TEXT));
        let full = f.rig.get(key, None).await;
        assert_eq!(whole.header("etag"), full.header("etag"));
        let part = f
            .with_grant(&t, "GET", &uri(key), &[("range", "bytes=2-4")])
            .await;
        assert_eq!(part.status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(part.body, "234");
        assert_eq!(part.header("content-range"), "bytes 2-4/10");
        let tail = f
            .with_grant(&t, "GET", &uri(key), &[("range", "bytes=-3")])
            .await;
        assert_eq!(tail.body, "789");
        let head = f.with_grant(&t, "HEAD", &uri(key), &[]).await;
        assert_eq!(head.status, StatusCode::OK);
        assert_eq!(head.header("content-length"), "10");
        let sdk = format!("{}?x-id=GetObject", uri(key));
        assert_eq!(f.with_grant(&t, "GET", &sdk, &[]).await.body, TEXT);
    }
}

/// Asserts that a grant does not read an object it does not list, whether the object exists or
/// not, with one answer that tells nothing of the object (and no byte of it): another object, an
/// object that does not exist, a prefix and an extension of a listed key, and HEAD as well as GET.
/// A listed object that does not exist is a plain 404.
#[tokio::test(start_paused = true)]
async fn a_grant_does_not_read_an_object_it_does_not_list() {
    let f = fixture().await;
    let t = token(&["cas/b/pack-a", "cas/l/absent"]);
    let mut texts_seen = Vec::new();
    for key in [
        "cas/b/other",
        "cas/b/missing",
        "cas/b/pack",
        "cas/b/pack-a/x",
        "cas/l/blob-1",
    ] {
        for method in ["GET", "HEAD"] {
            let reply = f.with_grant(&t, method, &uri(key), &[]).await;
            if method == "GET" {
                assert_eq!(refused(&reply), access_denied(), "{key}");
                assert!(!reply.body.contains(TEXT), "{key}");
                texts_seen.push(message(&reply));
            }
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {key}");
        }
    }
    texts_seen.dedup();
    assert_eq!(texts_seen.len(), 1, "{texts_seen:?}");
    assert!(texts_seen[0].contains("not one the read grant lists"));
    let absent = f.with_grant(&t, "GET", &uri("cas/l/absent"), &[]).await;
    assert_eq!(absent.status, StatusCode::NOT_FOUND);
}

/// Asserts that a grant never writes and never lists, whatever object it lists, and that nothing
/// changes: a put (also of a key it lists and one that does not exist), a copy, a delete, the
/// calls of a multipart upload, a listing of the bucket and of the open uploads, a call on the
/// bucket and on the root, and a get with a parameter that makes it another call. Each is
/// 403 `AccessDenied`.
#[tokio::test(start_paused = true)]
async fn a_grant_never_writes_deletes_or_lists() {
    let f = fixture().await;
    let t = token(&["cas/b/pack-a", "cas/l/absent"]);
    let object = uri("cas/b/pack-a");
    let absent = uri("cas/l/absent");
    let bucket = format!("/{BUCKET}");
    let none: &[(&str, &str)] = &[];
    let create: &[(&str, &str)] = &[("if-none-match", "*")];
    let copy: &[(&str, &str)] = &[("x-amz-copy-source", "/tier-bucket/cas/b/other")];
    let calls: Vec<Call> = vec![
        ("PUT", object.clone(), create),
        ("PUT", absent.clone(), create),
        ("PUT", absent.clone(), none),
        ("PUT", absent.clone(), copy),
        ("DELETE", object.clone(), none),
        ("POST", format!("{absent}?uploads"), none),
        ("POST", format!("{object}?uploadId=u"), none),
        ("PUT", format!("{absent}?partNumber=1&uploadId=u"), none),
        ("DELETE", format!("{object}?uploadId=u"), none),
        (
            "GET",
            format!("{bucket}?list-type=2&prefix=cas%2Fb%2F"),
            none,
        ),
        ("GET", format!("{bucket}?list-type=2"), none),
        ("GET", format!("{bucket}?uploads&prefix=cas%2F"), none),
        ("HEAD", bucket.clone(), none),
        ("PUT", bucket.clone(), none),
        ("GET", "/".to_owned(), none),
        ("GET", format!("{object}?uploadId=u"), none),
        ("GET", format!("{object}?partNumber=1"), none),
        ("GET", format!("{object}?uploads"), none),
        ("GET", format!("{object}?versionId=1"), none),
        ("HEAD", format!("{object}?partNumber=1"), none),
        ("PATCH", object.clone(), none),
    ];
    for (method, uri, extra) in calls {
        let reply = f.with_grant(&t, method, &uri, extra).await;
        assert_eq!(refused(&reply), access_denied(), "{method} {uri}");
    }
    let keys = texts(&f.rig.list("").await.body, "Key");
    assert_eq!(keys, ["cas/b/other", "cas/b/pack-a", "cas/l/blob-1"]);
    assert_eq!(f.rig.get("cas/b/pack-a", None).await.body, TEXT);
    assert_eq!(
        f.rig.get("cas/l/absent", None).await.status,
        StatusCode::NOT_FOUND
    );
    let open = f.rig.list("uploads&prefix=").await;
    assert!(texts(&open.body, "UploadId").is_empty(), "{}", open.body);
}

/// Asserts that a call a grant does not allow is refused before its body is read: a put whose body
/// would fail on the first poll is answered 403, not with an error of the body.
#[tokio::test(start_paused = true)]
async fn a_refused_call_of_a_grant_does_not_read_its_body() {
    let f = fixture().await;
    let t = token(&["cas/l/absent"]);
    let (parts, _) =
        request(&t, "PUT", &uri("cas/l/absent"), &[("if-none-match", "*")]).into_parts();
    let body = Scripted::new(vec![Step::Data(b"x"), Step::Fail]);
    let reply = f.rig.send_raw(Request::from_parts(parts, body)).await;
    assert_eq!(refused(&reply), access_denied());
    assert_eq!(
        f.rig.get("cas/l/absent", None).await.status,
        StatusCode::NOT_FOUND
    );
}

/// Asserts the clock on the server: a grant is good through the second before the one it names,
/// and from that second on every read of it is 403 and says it has expired, with no byte of the
/// object.
#[tokio::test(start_paused = true)]
async fn an_expired_grant_reads_nothing() {
    let f = fixture().await;
    let t = token(&["cas/b/pack-a"]);
    f.clock.set(EXPIRES * 1000 - 1);
    assert_eq!(
        f.with_grant(&t, "GET", &uri("cas/b/pack-a"), &[])
            .await
            .body,
        TEXT
    );
    for after in [0, 1, 3_600_000] {
        f.clock.set(EXPIRES * 1000 + after);
        for (method, range) in [("GET", None), ("GET", Some("bytes=0-1")), ("HEAD", None)] {
            let extra: Vec<(&str, &str)> = range.map(|r| ("range", r)).into_iter().collect();
            let reply = f.with_grant(&t, method, &uri("cas/b/pack-a"), &extra).await;
            assert_eq!(refused(&reply), access_denied(), "{method} {after}");
            if method == "GET" {
                assert!(message(&reply).contains("expired"), "{}", reply.body);
            }
            assert!(!reply.body.contains(TEXT));
        }
    }
}

/// Asserts that a token the server cannot trust reads nothing, with an object the grant would
/// have listed: a list edited after the signature (a key added, the expiry moved), a signature
/// by another secret, a key id the server does not have, a grant for another bucket, a token that
/// is not one, one over the size limit, two grant headers, and a grant beside a good SigV4
/// signature. And that a server with no grant key trusts no grant.
#[tokio::test(start_paused = true)]
async fn a_tampered_or_foreign_grant_reads_nothing() {
    let f = fixture().await;
    let own = token(&["cas/b/pack-a"]);
    let (body, tag) = own.split_once('.').unwrap();
    let elsewhere = mint_grant(&key(), "other-bucket", EXPIRES, ["cas/b/pack-a"]).unwrap();
    let foreign = mint_grant(
        &AccessKey::new(ID, "another-secret-0123456789"),
        BUCKET,
        EXPIRES,
        ["cas/b/pack-a"],
    );
    let stranger = mint_grant(
        &AccessKey::new("stranger", SECRET),
        BUCKET,
        EXPIRES,
        ["cas/b/pack-a"],
    );
    let wider = token(&["cas/b/other", "cas/b/pack-a"]);
    let (wider_body, _) = wider.split_once('.').unwrap();
    let moved = mint_grant(&key(), BUCKET, EXPIRES + 1, ["cas/b/pack-a"]).unwrap();
    let (moved_body, _) = moved.split_once('.').unwrap();
    let oversize = "a".repeat(MAX_TOKEN_BYTES + 1);
    let tokens = [
        format!("{wider_body}.{tag}"),
        format!("{moved_body}.{tag}"),
        foreign.unwrap(),
        stranger.unwrap(),
        elsewhere,
        "not-a-token".to_owned(),
        format!("{body}."),
        oversize,
    ];
    for t in &tokens {
        for key in ["cas/b/pack-a", "cas/b/other"] {
            let reply = f.with_grant(t, "GET", &uri(key), &[]).await;
            assert_eq!(refused(&reply), access_denied(), "{key}");
            assert!(!reply.body.contains(TEXT));
        }
    }
    let twice = request(&own, "GET", &uri("cas/b/pack-a"), &[]);
    let (mut parts, empty) = twice.into_parts();
    parts.headers.append(GRANT_HEADER, own.parse().unwrap());
    let reply = f.rig.send_raw(Request::from_parts(parts, empty)).await;
    assert_eq!(refused(&reply), access_denied());
    let signed = sign(&f.rig.config, "GET", &uri("cas/b/pack-a"), &[], b"");
    let (mut parts, empty) = signed.into_parts();
    parts.headers.insert(GRANT_HEADER, own.parse().unwrap());
    let reply = f.rig.send_raw(Request::from_parts(parts, empty)).await;
    assert_eq!(refused(&reply), access_denied());
    assert!(message(&reply).contains("not both"), "{}", reply.body);
    let good = f.with_grant(&own, "GET", &uri("cas/b/pack-a"), &[]).await;
    assert_eq!(good.body, TEXT);

    let bare = Rig::new(MEBIBYTE).await;
    bare.put("cas/b/pack-a", TEXT.as_bytes()).await;
    let reply = bare
        .send_raw(request(&own, "GET", &uri("cas/b/pack-a"), &[]))
        .await;
    assert_eq!(refused(&reply), access_denied());
}

/// Asserts that a grant key signs nothing: a SigV4 request signed with the id and the secret of
/// the grant key is an unknown access key, and the full key goes on working.
#[tokio::test(start_paused = true)]
async fn a_grant_key_signs_no_request() {
    let f = fixture().await;
    let reply = f
        .rig
        .send_as((ID, SECRET), "GET", &uri("cas/b/pack-a"), &[], b"")
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.code(), "InvalidAccessKeyId");
    assert_eq!(f.rig.get("cas/b/pack-a", None).await.body, TEXT);
}

/// A server in front of a cache over an upstream that holds `cas/b/obj` and `cas/b/hidden`.
struct Front {
    rig: CacheRig,
    server: Arc<Server>,
}

impl Front {
    async fn new(name: &str) -> Self {
        Self::with(CacheRig::new(name), false).await
    }

    /// The same over a cache with `write_through` on, whose front takes writes.
    async fn writing(name: &str) -> Self {
        Self::with(CacheRig::writing(name), true).await
    }

    async fn with(rig: CacheRig, writes: bool) -> Self {
        rig.clock.set(NOW);
        rig.put("cas/b/obj", TEXT.as_bytes()).await;
        rig.put("cas/b/hidden", TEXT.as_bytes()).await;
        let mut config = ServerConfig::new(BUCKET, KEY_ID, FULL_SECRET, rig.clock.clone());
        config.rate_clock = rig.clock.clone();
        config.cache = Some(rig.cache.clone());
        config.cache_writes = writes;
        config.grant_keys = vec![key()];
        let backend: Arc<dyn Backend> = Arc::new(rig.cache.clone());
        let server = Server::new(backend, config);
        Self { rig, server }
    }

    async fn answer(&self, request: Request<Full<Bytes>>) -> Reply {
        let response = self.server.handle(request).await;
        let (parts, body) = response.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }
}

/// Asserts that a grant works on the front of a cache as on a storage front: a listed object is
/// read whole, by a range and by HEAD, served from the cache after the first read; an object the
/// grant does not list is refused before the cache looks at it (the upstream is not asked, and
/// nothing is filled); and a write is refused.
#[tokio::test]
async fn a_grant_reads_through_the_front_of_a_cache() {
    let front = Front::new("tsgrant-cache-front").await;
    let t = token(&["cas/b/obj"]);
    let object = uri("cas/b/obj");
    let whole = front.answer(request(&t, "GET", &object, &[])).await;
    assert_eq!((whole.status, whole.body.as_str()), (StatusCode::OK, TEXT));
    let gets = front.rig.calls(Op::Get);
    let again = front.answer(request(&t, "GET", &object, &[])).await;
    assert_eq!(again.body, TEXT);
    let part = front
        .answer(request(&t, "GET", &object, &[("range", "bytes=1-3")]))
        .await;
    assert_eq!(
        (part.status, part.body.as_str()),
        (StatusCode::PARTIAL_CONTENT, "123")
    );
    let head = front.answer(request(&t, "HEAD", &object, &[])).await;
    assert_eq!(head.header("content-length"), "10");
    assert_eq!(front.rig.calls(Op::Get), gets);

    let hidden = uri("cas/b/hidden");
    let heads = front.rig.calls(Op::Head);
    for method in ["GET", "HEAD"] {
        let reply = front.answer(request(&t, method, &hidden, &[])).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method}");
    }
    assert_eq!(front.rig.calls(Op::Get), gets);
    assert_eq!(front.rig.calls(Op::Head), heads);
    assert_eq!(front.rig.cache.held().1, 1);
    let write = front
        .answer(request(&t, "PUT", &object, &[("if-none-match", "*")]))
        .await;
    assert_eq!(refused(&write), access_denied());
    let missing = front.answer(request("garbage", "GET", &object, &[])).await;
    assert_eq!(refused(&missing), access_denied());
}

/// The calls of the upstream that write, not counting a HEAD.
fn upstream_writes(rig: &CacheRig) -> u64 {
    let ops = [
        Op::Put,
        Op::PutIfMatch,
        Op::Delete,
        Op::CreateUpload,
        Op::UploadPart,
        Op::CompleteUpload,
        Op::AbortUpload,
    ];
    ops.iter().map(|op| rig.calls(*op)).sum()
}

/// Asserts that on a cache whose front takes writes (`write_through`), a grant is still read-only:
/// a PUT, a DELETE and a POST (create, complete) of an object the grant lists, and of one it lists
/// that does not exist, are each 403, answered by the grant and not by the write path: the
/// upstream sees no write call, the cache stages and holds nothing new, the object is as it was,
/// and the absent one is not made.
#[tokio::test]
async fn a_grant_does_not_write_through_a_cache_that_takes_writes() {
    let front = Front::writing("tsgrant-cache-writes").await;
    let t = token(&["cas/b/new", "cas/b/obj"]);
    let (object, absent) = (uri("cas/b/obj"), uri("cas/b/new"));
    let writes = upstream_writes(&front.rig);
    let held = front.rig.cache.held();
    let staged = front.rig.staged_files();
    let none: &[(&str, &str)] = &[];
    let create: &[(&str, &str)] = &[("if-none-match", "*")];
    let calls: Vec<Call> = vec![
        ("PUT", object.clone(), none),
        ("PUT", absent.clone(), create),
        ("PUT", absent.clone(), none),
        ("DELETE", object.clone(), none),
        ("DELETE", absent.clone(), none),
        ("POST", format!("{absent}?uploads"), none),
        ("POST", format!("{object}?uploadId=u"), none),
        ("PUT", format!("{absent}?partNumber=1&uploadId=u"), none),
        ("DELETE", format!("{object}?uploadId=u"), none),
    ];
    for (method, uri, extra) in calls {
        let reply = front.answer(request(&t, method, &uri, extra)).await;
        assert_eq!(refused(&reply), access_denied(), "{method} {uri}");
        assert!(message(&reply).contains("GET or HEAD"), "{method} {uri}");
    }
    assert_eq!(upstream_writes(&front.rig), writes);
    assert_eq!(front.rig.cache.held(), held);
    assert_eq!(front.rig.staged_files(), staged);
    assert!(front.rig.tamper.memory.head("cas/b/new").await.is_err());
    let read = front.answer(request(&t, "GET", &object, &[])).await;
    assert_eq!(read.body, TEXT);
}

/// Asserts the longest life of a grant on the server: with the clock where it is, a grant that
/// ends 900 seconds on (the default cap) reads, one that ends a second later is 403 and reads
/// nothing, and one with the largest expiry is 403, so no grant is good for ever.
#[tokio::test(start_paused = true)]
async fn a_grant_that_ends_beyond_the_cap_reads_nothing() {
    let f = fixture().await;
    let object = uri("cas/b/pack-a");
    let ending = |at: u64| mint_grant(&key(), BUCKET, at, ["cas/b/pack-a"]).unwrap();
    let inside = ending(NOW / 1000 + 900);
    let reply = f.with_grant(&inside, "GET", &object, &[]).await;
    assert_eq!((reply.status, reply.body.as_str()), (StatusCode::OK, TEXT));
    for at in [NOW / 1000 + 901, u64::MAX] {
        for method in ["GET", "HEAD"] {
            let reply = f.with_grant(&ending(at), method, &object, &[]).await;
            assert_eq!(refused(&reply), access_denied(), "{method} {at}");
            assert!(!reply.body.contains(TEXT));
        }
    }
    f.clock.set(NOW + 1000);
    let reply = f
        .with_grant(&ending(NOW / 1000 + 901), "GET", &object, &[])
        .await;
    assert_eq!(reply.body, TEXT);
}

/// Asserts that a grant beside the parameters of a presigned URL (`X-Amz-Algorithm`, `-Expires`,
/// `-Signature`) is refused for a listed object, GET and HEAD: the grant takes no query parameter
/// but `x-id`, and the call is not read as a presigned one.
#[tokio::test(start_paused = true)]
async fn a_grant_beside_a_presigned_style_query_is_refused() {
    let f = fixture().await;
    let t = token(&["cas/b/pack-a"]);
    let signature = "0".repeat(64);
    let query = format!(
        "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKID%2F20260101%2Fus-east-1%2Fs3%2Faws4_request\
         &X-Amz-Date=20260101T000000Z&X-Amz-Expires=60&X-Amz-SignedHeaders=host&X-Amz-Signature={signature}"
    );
    for method in ["GET", "HEAD"] {
        for suffix in [
            query.as_str(),
            "X-Amz-Signature=00",
            "x-id=GetObject&X-Amz-Signature=00",
        ] {
            let target = format!("{}?{suffix}", uri("cas/b/pack-a"));
            let reply = f.with_grant(&t, method, &target, &[]).await;
            assert_eq!(refused(&reply), access_denied(), "{method} {suffix}");
            assert!(!reply.body.contains(TEXT));
        }
    }
    let plain = f.with_grant(&t, "GET", &uri("cas/b/pack-a"), &[]).await;
    assert_eq!(plain.body, TEXT);
}
