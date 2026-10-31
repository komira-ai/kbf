//! The S3 front over a cache that writes through: what each key may do, what the upstream
//! answers to the client, and what is left in the upstream and in the cache.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, StatusCode};

use super::testing::{Rig, digest};
use crate::server::auth::{sign, sign_as};
use crate::server::ops_tests::completion;
use crate::server::presign::presign_url;
use crate::server::presign::tests::{SECRET as PRESIGN_SECRET, access, grant, request};
use crate::server::testing::{BUCKET, KEY_ID, NOW, Reply, SECRET, texts};
use crate::server::{AccessKey, GcKey, Prefixes, PresignKey, ScopedKey, Server, ServerConfig};
use crate::{Backend, Error, Op};

const GC: (&str, &str) = ("pack-gc", "gc-secret-0123456789");
const SCOPED: (&str, &str) = ("scoped-a", "scoped-secret-0123456789");
const READER: (&str, &str) = ("AKIDREADER", "reader-secret");
const PRESIGN: (&str, &str) = ("pack-presign", PRESIGN_SECRET);

/// A server in front of a cache, with one key of each kind: the full key, a read-only key, a GC
/// key limited to `cas/b/`, a scoped key that reads, creates and deletes under `cas/s/`, and a
/// presigning key limited to `cas/b/`.
struct Front {
    rig: Rig,
    server: Arc<Server>,
    config: ServerConfig,
}

impl Front {
    /// The server over a cache with `write_through` as given, which also sets the front's
    /// `cache_writes`, as the binary does.
    fn new(name: &str, writes: bool) -> Self {
        let rig = if writes {
            Rig::writing(name)
        } else {
            Rig::new(name)
        };
        rig.clock.set(NOW);
        let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, rig.clock.clone());
        config.rate_clock = rig.clock.clone();
        config.cache = Some(rig.cache.clone());
        config.cache_writes = writes;
        config.read_only_keys = vec![AccessKey::new(READER.0, READER.1)];
        config.gc_keys = vec![GcKey::new(GC.0, GC.1, "cas/b/").unwrap()];
        let scopes = Prefixes {
            read: &["cas/s/"],
            write: &["cas/s/"],
            delete: &["cas/s/"],
        };
        config.scoped_keys = vec![ScopedKey::new(SCOPED.0, SCOPED.1, &scopes).unwrap()];
        config.presign_keys = vec![PresignKey::new(PRESIGN.0, PRESIGN.1, grant()).unwrap()];
        let backend: Arc<dyn Backend> = Arc::new(rig.cache.clone());
        let server = Server::new(backend, config.clone());
        Self {
            rig,
            server,
            config,
        }
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

    /// A call signed with the full key.
    async fn full(&self, method: &str, key: &str, extra: &[(&str, &str)], body: &[u8]) -> Reply {
        let uri = format!("/{BUCKET}/{key}");
        self.answer(sign(&self.config, method, &uri, extra, body))
            .await
    }

    /// A call signed with the key `who`.
    async fn by(
        &self,
        who: (&str, &str),
        method: &str,
        key: &str,
        extra: &[(&str, &str)],
        body: &[u8],
    ) -> Reply {
        let uri = format!("/{BUCKET}/{key}");
        self.answer(sign_as(&self.config, who, method, &uri, extra, body))
            .await
    }

    /// A `PUT` of a presigned URL.
    async fn presigned(&self, url: &str, body: &[u8]) -> Reply {
        let rest = url.split_once("://").unwrap().1;
        let at = rest.find('/').unwrap();
        let built = Request::builder()
            .method("PUT")
            .uri(&rest[at..])
            .header("host", &rest[..at])
            .body(Full::new(Bytes::copy_from_slice(body)))
            .unwrap();
        self.answer(built).await
    }

    /// The calls of the upstream that a write makes, or a HEAD before it.
    fn writes_seen(&self) -> u64 {
        let ops = [
            Op::Put,
            Op::PutIfMatch,
            Op::Delete,
            Op::CreateUpload,
            Op::UploadPart,
            Op::CompleteUpload,
            Op::AbortUpload,
            Op::Head,
        ];
        ops.iter().map(|op| self.rig.calls(*op)).sum()
    }

    /// Whether the upstream holds the object, read behind the cache.
    async fn upstream_holds(&self, key: &str) -> bool {
        self.rig.tamper.memory.head(key).await.is_ok()
    }

    /// The object read through the cache with the full key, which fills it.
    async fn read(&self, key: &str) -> Reply {
        self.full("GET", key, &[], b"").await
    }
}

fn denied(reply: &Reply) -> (u16, String) {
    (reply.status.as_u16(), reply.code())
}

fn access_denied() -> (u16, String) {
    (403, "AccessDenied".to_owned())
}

/// A write call: its method, its path and query, and the headers it carries.
type Write = (&'static str, String, Vec<(&'static str, String)>);

/// The calls that write, each with the key it names: a put and a plain put, a part, the start,
/// the completion and the abort of an upload, a delete, and a copy.
fn writes(key: &str) -> Vec<Write> {
    let copy = format!("/{BUCKET}/{key}");
    vec![
        (
            "PUT",
            key.to_owned(),
            vec![("if-none-match", "*".to_owned())],
        ),
        ("PUT", key.to_owned(), vec![]),
        ("PUT", key.to_owned(), vec![("x-amz-copy-source", copy)]),
        ("PUT", format!("{key}?partNumber=1&uploadId=u"), vec![]),
        ("POST", format!("{key}?uploads"), vec![]),
        ("POST", format!("{key}?uploadId=u"), vec![]),
        ("DELETE", key.to_owned(), vec![]),
        ("DELETE", format!("{key}?uploadId=u"), vec![]),
    ]
}

/// Sends every write call with the key `who` (or the full key) and gives the replies.
async fn send_writes(front: &Front, who: Option<(&str, &str)>, key: &str) -> Vec<Reply> {
    let mut replies = Vec::new();
    for (method, target, extra) in writes(key) {
        let extra: Vec<(&str, &str)> = extra.iter().map(|(n, v)| (*n, v.as_str())).collect();
        let reply = match who {
            Some(who) => front.by(who, method, &target, &extra, b"<x/>").await,
            None => front.full(method, &target, &extra, b"<x/>").await,
        };
        replies.push(reply);
    }
    replies
}

/// Asserts that with the setting off every write call is 403 `AccessDenied` with the text of a
/// read cache, whatever key signs it (the full key, a read-only key, a GC key, a scoped key and a
/// presigned URL), and that the upstream sees no call at all: the refusal comes before the role
/// of the key is looked at. This is what a cache that is not told to write has always answered.
#[tokio::test]
async fn with_the_setting_off_every_write_is_refused_for_every_key_and_reaches_nothing() {
    let front = Front::new("tscache-wfront-off", false);
    let key = "cas/b/obj";
    let signers = [None, Some(READER), Some(GC), Some(SCOPED)];
    for who in signers {
        for reply in send_writes(&front, who, key).await {
            assert_eq!(denied(&reply), access_denied(), "{who:?}");
            assert!(reply.body.contains("read cache"), "{who:?}: {}", reply.body);
        }
    }
    let url = presign_url("PUT", &access(PRESIGN.0), &request(key, 300), &[]).unwrap();
    let reply = front.presigned(&url, b"x").await;
    assert_eq!(denied(&reply), access_denied());
    assert!(reply.body.contains("read cache"), "{}", reply.body);
    assert_eq!(front.writes_seen(), 0);
    assert_eq!(front.rig.cache.held(), (0, 0));
}

/// Asserts a create-only put through the front: 200 and the object is at the upstream, with and
/// without `If-None-Match: *` and under a prefix the cache keeps and one it does not; the same
/// key again is 412 `PreconditionFailed` and the upstream got one put of it, whose bytes stay;
/// the put makes one HEAD before the body; nothing is left in the stage directory. On the code
/// this change is stacked on, every one of these is a 403.
#[tokio::test]
async fn a_create_only_put_is_200_and_a_repeat_is_412() {
    let front = Front::new("tscache-wfront-create", true);
    for key in ["cas/b/new", "other/new"] {
        let first = front
            .full("PUT", key, &[("if-none-match", "*")], b"first")
            .await;
        assert_eq!(first.status, StatusCode::OK, "{key}: {}", first.body);
        let puts = front.rig.calls(Op::Put);
        let again = front
            .full("PUT", key, &[("if-none-match", "*")], b"second")
            .await;
        assert_eq!(
            denied(&again),
            (412, "PreconditionFailed".to_owned()),
            "{key}"
        );
        let plain = front.full("PUT", key, &[], b"third").await;
        assert_eq!(plain.status, StatusCode::PRECONDITION_FAILED, "{key}");
        assert_eq!(front.rig.calls(Op::Put), puts, "{key}");
        assert_eq!(front.read(key).await.body, "first", "{key}");
    }
    let fresh = front.full("PUT", "other/plain", &[], b"plain").await;
    assert_eq!(fresh.status, StatusCode::OK);
    assert_eq!(front.rig.calls(Op::Put), 3);
    assert_eq!(front.rig.calls(Op::PutIfMatch), 0);
    assert_eq!(front.rig.staged_files(), 0);
}

/// Asserts that a put of a key named by its content (`cas/l/<sha256>`) whose body hashes to
/// another name is 400 `BadDigest` through the front and leaves nothing at the upstream or in
/// the cache; and that the body that does hash to the name is 200 and is a hit afterwards.
#[tokio::test]
async fn a_body_that_does_not_hash_to_its_name_is_400_bad_digest() {
    let front = Front::new("tscache-wfront-bad-digest", true);
    let key = format!("cas/l/{}", digest(b"the right bytes"));
    // The front hands the backend the length of the body only when the request says it, as a
    // client of an S3 upstream must; without it the copy cannot be reserved for and is not kept.
    let create = [("if-none-match", "*"), ("content-length", "15")];
    let wrong = front.full("PUT", &key, &create, b"the wrong bytes").await;
    assert_eq!(
        denied(&wrong),
        (400, "BadDigest".to_owned()),
        "{}",
        wrong.body
    );
    assert!(!front.upstream_holds(&key).await);
    assert_eq!(front.rig.cache.held(), (0, 0));

    let right = front.full("PUT", &key, &create, b"the right bytes").await;
    assert_eq!(right.status, StatusCode::OK, "{}", right.body);
    assert_eq!(front.rig.cache.held(), (15, 1));
    assert_eq!(front.read(&key).await.body, "the right bytes");
}

/// Asserts that a put that replaces on a tag is not done: `If-Match` on an object that is there
/// is answered as on every front (the object cannot be replaced), and nothing is sent to the
/// upstream as a write.
#[tokio::test]
async fn a_put_that_replaces_is_not_done() {
    let front = Front::new("tscache-wfront-replace", true);
    front.rig.put("cas/b/obj", b"old").await;
    let tag = front.rig.tamper.memory.head("cas/b/obj").await.unwrap().tag;
    let right = format!("\"{tag}\"");
    let replaced = front
        .full("PUT", "cas/b/obj", &[("if-match", right.as_str())], b"new")
        .await;
    assert_eq!(replaced.status, StatusCode::NOT_IMPLEMENTED);
    let wrong = front
        .full("PUT", "cas/b/obj", &[("if-match", "\"nope\"")], b"new")
        .await;
    assert_eq!(wrong.status, StatusCode::PRECONDITION_FAILED);
    assert_eq!(front.rig.calls(Op::Put), 0);
    assert_eq!(front.rig.calls(Op::PutIfMatch), 0);
    assert_eq!(front.read("cas/b/obj").await.body, "old");
}

/// Asserts how the refusals of the upstream reach the client, for a put whose HEAD found nothing:
/// a conditional put it refuses because another writer was first is 412 `PreconditionFailed`; a
/// full upstream (507 in S3) is 503 `SlowDown` with `Retry-After`; an upstream that does not
/// answer is 503 `ServiceUnavailable`. In each the object is not stored and the stage directory is empty.
#[tokio::test]
async fn the_refusals_of_the_upstream_reach_the_client_as_412_and_503() {
    let front = Front::new("tscache-wfront-refusals", true);
    let key = "cas/b/refused";
    let cases = [
        (
            Error::AlreadyExists(key.to_owned()),
            412,
            "PreconditionFailed",
        ),
        (Error::Full("507".to_owned()), 503, "SlowDown"),
        (
            Error::Unavailable("503".to_owned()),
            503,
            "ServiceUnavailable",
        ),
    ];
    for (error, status, code) in cases {
        front.rig.upstream.fail_next_with(Op::Put, error);
        let reply = front
            .full("PUT", key, &[("if-none-match", "*")], b"body")
            .await;
        assert_eq!(denied(&reply), (status, code.to_owned()), "{code}");
        if code == "SlowDown" {
            assert_eq!(reply.header("retry-after"), "1");
        }
        assert!(!front.upstream_holds(key).await, "{code}");
        assert_eq!(front.rig.staged_files(), 0, "{code}");
    }
    let stored = front
        .full("PUT", key, &[("if-none-match", "*")], b"body")
        .await;
    assert_eq!(stored.status, StatusCode::OK);
}

/// Runs the four calls of a multipart upload of `key` as `who` and gives the status of the
/// completion. The object is two parts, "hello " and "world".
async fn multipart(front: &Front, who: Option<(&str, &str)>, key: &str) -> StatusCode {
    let call = |method: &'static str, target: String, body: Vec<u8>| async move {
        match who {
            Some(who) => front.by(who, method, &target, &[], &body).await,
            None => front.full(method, &target, &[], &body).await,
        }
    };
    let started = call("POST", format!("{key}?uploads"), Vec::new()).await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let id = texts(&started.body, "UploadId").concat();
    let mut tags = Vec::new();
    for (number, part) in [(1, b"hello ".as_slice()), (2, b"world".as_slice())] {
        let target = format!("{key}?partNumber={number}&uploadId={id}");
        let sent = call("PUT", target, part.to_vec()).await;
        assert_eq!(sent.status, StatusCode::OK, "{}", sent.body);
        tags.push(sent.header("etag").to_owned());
    }
    let done = completion(&[(1, tags[0].as_str()), (2, tags[1].as_str())]);
    let ended = call("POST", format!("{key}?uploadId={id}"), done.into_bytes()).await;
    ended.status
}

/// Asserts a multipart upload through the front, with the full key and with a scoped key under
/// its write prefix: the object is made at the upstream from the parts in order, the upstream
/// saw the start, two parts and the completion, and a repeat of the whole upload is refused at
/// the completion (412) and leaves the object as it was; an aborted upload makes nothing and its
/// id is `NoSuchUpload` after. It catches a front that refuses an upload, and a completion that
/// replaces.
#[tokio::test]
async fn a_multipart_upload_goes_through_for_the_full_key_and_a_scoped_key() {
    let front = Front::new("tscache-wfront-multipart", true);
    for (who, key) in [(None, "cas/b/big"), (Some(SCOPED), "cas/s/big")] {
        assert_eq!(multipart(&front, who, key).await, StatusCode::OK, "{key}");
        assert_eq!(front.read(key).await.body, "hello world", "{key}");
        let again = multipart(&front, who, key).await;
        assert_eq!(again, StatusCode::PRECONDITION_FAILED, "{key}");
        assert_eq!(front.read(key).await.body, "hello world", "{key}");
    }
    assert_eq!(front.rig.calls(Op::CreateUpload), 4);
    assert_eq!(front.rig.calls(Op::UploadPart), 8);
    assert_eq!(front.rig.calls(Op::CompleteUpload), 4);

    let started = front
        .by(SCOPED, "POST", "cas/s/gone?uploads", &[], b"")
        .await;
    let id = texts(&started.body, "UploadId").concat();
    let abort = format!("cas/s/gone?uploadId={id}");
    let aborted = front.by(SCOPED, "DELETE", &abort, &[], b"").await;
    assert_eq!(aborted.status, StatusCode::NO_CONTENT);
    let part = format!("cas/s/gone?partNumber=1&uploadId={id}");
    let late = front.by(SCOPED, "PUT", &part, &[], b"p").await;
    assert_eq!(denied(&late), (404, "NoSuchUpload".to_owned()));
    assert!(!front.upstream_holds("cas/s/gone").await);
}

/// Asserts that the completion of an upload through the front drops the cache's entry for the
/// key, for the case where it has one: the object was read into the cache, deleted at the
/// upstream behind the cache's back, and made again by an upload (a scoped key's, so the roles
/// run too). The entry and its file are gone after the completion, and the next read is of the
/// new object.
#[tokio::test]
async fn the_completion_of_an_upload_drops_the_entry_for_the_key() {
    let front = Front::new("tscache-wfront-complete", true);
    front.rig.put("cas/s/big", b"old bytes").await;
    assert_eq!(front.read("cas/s/big").await.body, "old bytes");
    assert_eq!(front.rig.cache.held(), (9, 1));
    front.rig.tamper.memory.delete("cas/s/big").await.unwrap();

    let ended = multipart(&front, Some(SCOPED), "cas/s/big").await;
    assert_eq!(ended, StatusCode::OK);
    assert_eq!(front.rig.cache.held(), (0, 0));
    assert!(!front.rig.has_file("cas/s/big"));
    assert_eq!(front.read("cas/s/big").await.body, "hello world");
    assert_eq!(front.rig.cache.held(), (11, 1));
}

/// Asserts a presigned PUT through the front: the URL of a presigning key stores the object at
/// the upstream (200), the same URL again is 412 and the first bytes stay, a URL for a key outside
/// the ceiling of the key is 403 and stores nothing, the key signing in the header is 403, and
/// the upstream gets one put in all. A presigned PUT takes the way of the hot-only put of a
/// storage node, which here is the same streamed put.
#[tokio::test]
async fn a_presigned_put_goes_through_the_cache_front() {
    let front = Front::new("tscache-wfront-presigned", true);
    let url = presign_url("PUT", &access(PRESIGN.0), &request("cas/b/pre", 300), &[]).unwrap();
    let first = front.presigned(&url, b"presigned").await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(front.rig.calls(Op::Put), 1);
    assert_eq!(front.read("cas/b/pre").await.body, "presigned");
    let again = front.presigned(&url, b"second").await;
    assert_eq!(denied(&again), (412, "PreconditionFailed".to_owned()));
    assert_eq!(front.read("cas/b/pre").await.body, "presigned");

    let outside = request("cas/c/pre", 300);
    let url = presign_url("PUT", &access(PRESIGN.0), &outside, &[]).unwrap();
    let refused = front.presigned(&url, b"x").await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(!front.upstream_holds("cas/c/pre").await);
    let header = front.by(PRESIGN, "PUT", "cas/b/hdr", &[], b"x").await;
    assert_eq!(header.status, StatusCode::FORBIDDEN);
    assert!(!front.upstream_holds("cas/b/hdr").await);
    assert_eq!(front.rig.calls(Op::Put), 1);
    assert_eq!(front.rig.staged_files(), 0);
}

/// Asserts the roles over the cache front, one refusal after another, each leaving the upstream
/// with no write and no HEAD: a read-only key may not put, start an upload or delete; a scoped
/// key may not write or delete outside its prefixes; a GC key may not put or start an upload and
/// may not delete outside its prefix; the full key may not name a reserved key. The calls that
/// each key may do are then done: a put and a delete under the scoped key's prefixes, and a
/// delete under the GC key's prefix.
#[tokio::test]
async fn the_roles_of_the_keys_apply_over_the_cache_front() {
    let front = Front::new("tscache-wfront-roles", true);
    front.rig.put("cas/b/gc-owned", b"x").await;
    front.rig.put("cas/c/other", b"x").await;
    let before = front.writes_seen();
    let refused = [
        (READER, "PUT", "cas/b/r"),
        (READER, "POST", "cas/b/r?uploads"),
        (READER, "DELETE", "cas/b/gc-owned"),
        (SCOPED, "PUT", "cas/o/x"),
        (SCOPED, "PUT", "cas/b/x"),
        (SCOPED, "POST", "cas/o/x?uploads"),
        (SCOPED, "PUT", "cas/o/x?partNumber=1&uploadId=u"),
        (SCOPED, "DELETE", "cas/b/gc-owned"),
        (GC, "PUT", "cas/b/x"),
        (GC, "POST", "cas/b/x?uploads"),
        (GC, "DELETE", "cas/c/other"),
    ];
    for (who, method, target) in refused {
        let reply = front.by(who, method, target, &[], b"x").await;
        assert_eq!(
            denied(&reply),
            access_denied(),
            "{} {method} {target}",
            who.0
        );
    }
    let reserved = front
        .full("PUT", "_journal/p1/1", &[("if-none-match", "*")], b"x")
        .await;
    assert_eq!(denied(&reserved), access_denied());
    assert_eq!(front.writes_seen(), before);

    let create = [("if-none-match", "*")];
    let put = front.by(SCOPED, "PUT", "cas/s/own", &create, b"mine").await;
    assert_eq!(put.status, StatusCode::OK);
    let deleted = front.by(SCOPED, "DELETE", "cas/s/own", &[], b"").await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    let deleted = front.by(GC, "DELETE", "cas/b/gc-owned", &[], b"").await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert!(!front.upstream_holds("cas/s/own").await);
    assert!(!front.upstream_holds("cas/b/gc-owned").await);
    assert!(front.upstream_holds("cas/c/other").await);
}

/// Asserts that a delete by a GC key and by a scoped key leaves neither a copy in the cache nor
/// one at the upstream: the object was read into the cache first (so a copy and its file are
/// there), the delete is 204, the entry and the file are gone, the upstream does not have the
/// object, a read and a HEAD of it are 404, and deleting it again is 204. It catches a delete
/// that removes only one of the two copies.
#[tokio::test]
async fn a_gc_or_scoped_delete_leaves_no_copy_in_the_cache_or_at_the_upstream() {
    let front = Front::new("tscache-wfront-delete", true);
    for (who, key) in [(GC, "cas/b/g1"), (SCOPED, "cas/s/s1")] {
        front.rig.put(key, b"twelve bytes").await;
        assert_eq!(front.read(key).await.body, "twelve bytes", "{key}");
        assert_eq!(front.rig.cache.held(), (12, 1), "{key}");
        assert!(front.rig.has_file(key), "{key}");

        let deleted = front.by(who, "DELETE", key, &[], b"").await;
        assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{key}");
        assert_eq!(front.rig.cache.held(), (0, 0), "{key}");
        assert!(!front.rig.has_file(key), "{key}");
        assert!(!front.upstream_holds(key).await, "{key}");
        assert_eq!(front.read(key).await.status, StatusCode::NOT_FOUND, "{key}");
        let head = front.full("HEAD", key, &[], b"").await;
        assert_eq!(head.status, StatusCode::NOT_FOUND, "{key}");
        let again = front.by(who, "DELETE", key, &[], b"").await;
        assert_eq!(again.status, StatusCode::NO_CONTENT, "{key}");
    }
}
