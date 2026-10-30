//! The S3 front over a cache: a read cache that answers GET and HEAD and refuses the rest.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, StatusCode};

use super::testing::Rig;
use crate::server::auth::sign;
use crate::server::testing::{BUCKET, KEY_ID, NOW, Reply, Rig as ProxyRig, SECRET};
use crate::server::{Server, ServerConfig};
use crate::{Backend, Op};

/// A server in front of a cache over an upstream that holds `cas/b/obj`, a hundred bytes of
/// text.
struct Front {
    rig: Rig,
    server: Arc<Server>,
    config: ServerConfig,
}

/// A hundred bytes that are text, so that a body is read back as it is.
const TEXT: &str = "0123456789abcdefghij0123456789abcdefghij0123456789abcdefghij0123456789abcdefghij0123456789abcdefghij";

/// A write: its method, its URI and the headers it carries besides the signature.
type Call<'a> = (&'a str, String, Vec<(&'a str, &'a str)>);

impl Front {
    async fn new(name: &str) -> Self {
        let rig = Rig::new(name);
        rig.clock.set(NOW);
        rig.put("cas/b/obj", TEXT.as_bytes()).await;
        let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, rig.clock.clone());
        config.rate_clock = rig.clock.clone();
        config.cache = Some(rig.cache.clone());
        let backend: Arc<dyn Backend> = Arc::new(rig.cache.clone());
        let server = Server::new(backend, config.clone());
        Self {
            rig,
            server,
            config,
        }
    }

    /// Sends a request and reads the answer.
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

    /// Sends a request signed with the full key.
    async fn send(&self, method: &str, uri: &str, extra: &[(&str, &str)], body: &[u8]) -> Reply {
        self.answer(sign(&self.config, method, uri, extra, body))
            .await
    }

    /// Sends a request with no signature.
    async fn unsigned(&self, method: &str, uri: &str) -> Reply {
        let request = Request::builder().method(method).uri(uri);
        self.answer(request.body(Full::new(Bytes::new())).unwrap())
            .await
    }
}

/// Asserts that GET, ranged GET and HEAD of a key under a cache policy are served with the
/// right status, headers and bytes, and that after the first read the upstream sees no more.
#[tokio::test]
async fn get_and_head_are_served_from_the_cache() {
    let front = Front::new("tscache-front-read").await;
    let uri = format!("/{BUCKET}/cas/b/obj");
    let first = front.send("GET", &uri, &[], b"").await;
    assert_eq!((first.status, first.body.as_str()), (StatusCode::OK, TEXT));
    let gets = front.rig.calls(Op::Get);

    let again = front.send("GET", &uri, &[], b"").await;
    assert_eq!((again.status, again.body.as_str()), (StatusCode::OK, TEXT));
    let part = front
        .send("GET", &uri, &[("range", "bytes=10-19")], b"")
        .await;
    assert_eq!(part.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(part.body, &TEXT[10..20]);
    assert_eq!(part.header("content-range"), "bytes 10-19/100");
    let head = front.send("HEAD", &uri, &[], b"").await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header("content-length"), "100");
    let tag = front.rig.tamper.memory.head("cas/b/obj").await.unwrap().tag;
    assert_eq!(head.header("etag"), format!("\"{tag}\""));
    assert_eq!(front.rig.calls(Op::Get), gets);
    assert_eq!(front.rig.cache.held(), (100, 1));
}

/// Asserts that every write the S3 API has is refused with 403 `AccessDenied` and a text that
/// says this is a read cache, and that nothing reaches the upstream or
/// the cache: a put, a put without the condition, a copy, a part, the start and the completion
/// of an upload, a delete, the abort of an upload, and CreateBucket.
#[tokio::test]
async fn every_write_is_refused_and_reaches_nothing() {
    let front = Front::new("tscache-front-write").await;
    let object = format!("/{BUCKET}/cas/b/new");
    let bucket = format!("/{BUCKET}");
    let copy = format!("/{BUCKET}/cas/b/obj");
    let calls: [Call; 9] = [
        ("PUT", object.clone(), vec![("if-none-match", "*")]),
        ("PUT", object.clone(), vec![]),
        (
            "PUT",
            object.clone(),
            vec![("x-amz-copy-source", copy.as_str())],
        ),
        ("PUT", format!("{object}?partNumber=1&uploadId=u"), vec![]),
        ("POST", format!("{object}?uploads"), vec![]),
        ("POST", format!("{object}?uploadId=u"), vec![]),
        ("DELETE", copy.clone(), vec![]),
        ("DELETE", format!("{object}?uploadId=u"), vec![]),
        ("PUT", bucket, vec![]),
    ];
    for (method, uri, extra) in calls {
        let reply = front.send(method, &uri, &extra, b"<x/>").await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(reply.code(), "AccessDenied", "{method} {uri}");
        assert!(
            reply.body.contains("read cache"),
            "{method} {uri}: {}",
            reply.body
        );
    }
    let rig = &front.rig;
    let writes = [
        Op::Put,
        Op::PutIfMatch,
        Op::Delete,
        Op::CreateUpload,
        Op::UploadPart,
    ];
    assert_eq!(writes.iter().map(|op| rig.calls(*op)).sum::<u64>(), 0);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(rig.tamper.memory.head("cas/b/obj").await.is_ok());
    assert!(rig.tamper.memory.head("cas/b/new").await.is_err());
}

/// Asserts that a request with no signature is refused as it is on any front, so the read-only
/// front is not a way past the keys.
#[tokio::test]
async fn an_unsigned_read_is_refused() {
    let front = Front::new("tscache-front-unsigned").await;
    let reply = front.unsigned("GET", &format!("/{BUCKET}/cas/b/obj")).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(front.rig.cache.held(), (0, 0));
    assert_eq!(front.rig.calls(Op::Get), 0);
}

/// Asserts that a listing is the upstream's and keeps nothing.
#[tokio::test]
async fn a_listing_passes_through_and_keeps_nothing() {
    let front = Front::new("tscache-front-list").await;
    let uri = format!("/{BUCKET}?list-type=2&prefix=cas%2F");
    let reply = front.send("GET", &uri, &[], b"").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("cas/b/obj"), "{}", reply.body);
    assert_eq!(front.rig.calls(Op::List), 1);
    assert_eq!(front.rig.cache.held(), (0, 0));
}

/// Asserts that `GET /metricz` needs no signature and shows the counters of the cache, after
/// the reads that made them; that HEAD answers it too; and that a server with no cache says so.
#[tokio::test]
async fn metricz_shows_the_counters_or_says_there_is_no_cache() {
    let front = Front::new("tscache-front-metrics").await;
    let uri = format!("/{BUCKET}/cas/b/obj");
    for _ in 0..2 {
        let _ = front.send("GET", &uri, &[], b"").await;
    }
    let reply = front.unsigned("GET", "/metricz").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), "text/plain");
    for line in [
        "tierstore_cache_hits_total 1\n",
        "tierstore_cache_misses_total 1\n",
        "tierstore_cache_fills_total 1\n",
        "tierstore_cache_bytes 100\n",
    ] {
        assert!(reply.body.contains(line), "{line} in {}", reply.body);
    }
    assert_eq!(
        front.unsigned("HEAD", "/metricz").await.status,
        StatusCode::OK
    );

    let storage = ProxyRig::new(1000).await;
    let request = Request::builder().method("GET").uri("/metricz");
    let reply = storage
        .send_raw(request.body(Full::new(Bytes::new())).unwrap())
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("no local cache"), "{}", reply.body);
}
