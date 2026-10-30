//! The S3 front of a cache over a list of endpoints: what `/readyz` and `/metricz` say, and what
//! a cache that has lost every endpoint still serves.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, StatusCode};
use kbf_testkit::dir::TestDir;

use crate::server::auth::sign;
use crate::server::local_cache::testing::FakeDisk;
use crate::server::local_cache::{CacheConfig, LocalCache};
use crate::server::testing::{BUCKET, KEY_ID, NOW, Reply, SECRET};
use crate::server::{Server, ServerConfig};
use crate::{Backend, Op};

use super::testing::Fleet;

const BODY: &[u8] = b"0123456789abcdefghij";

/// A server in front of a cache over a fleet of two endpoints that hold `cas/b/cached` and
/// `cas/b/cold`.
struct Front {
    fleet: Fleet,
    server: Arc<Server>,
    config: ServerConfig,
    _dir: TestDir,
}

impl Front {
    async fn new(name: &str) -> Self {
        let fleet = Fleet::new(2);
        fleet.clock.set(NOW);
        fleet.put("cas/b/cached", BODY).await;
        fleet.put("cas/b/cold", BODY).await;
        let dir = TestDir::under(std::env::temp_dir(), name);
        let policies = vec![("cas/".to_owned(), true)];
        let settings = CacheConfig::new(dir.path().join("cache"), 1 << 20, policies);
        let upstream: Arc<dyn Backend> = fleet.endpoints.clone();
        let disk = Arc::new(FakeDisk::new());
        let cache = LocalCache::open(settings, upstream, disk, fleet.clock.clone()).unwrap();
        let mut config = ServerConfig::new(BUCKET, KEY_ID, SECRET, fleet.clock.clone());
        config.rate_clock = fleet.clock.clone();
        config.cache = Some(cache.clone());
        config.upstream = Some(fleet.endpoints.clone());
        let server = Server::new(Arc::new(cache), config.clone());
        Self {
            fleet,
            server,
            config,
            _dir: dir,
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

    /// A signed request for an object.
    async fn object(&self, method: &str, key: &str) -> Reply {
        let uri = format!("/{BUCKET}/{key}");
        self.answer(sign(&self.config, method, &uri, &[], b""))
            .await
    }

    /// An unsigned GET of a probe path.
    async fn probe(&self, path: &str) -> Reply {
        let request = Request::builder().method("GET").uri(path);
        self.answer(request.body(Full::new(Bytes::new())).unwrap())
            .await
    }

    /// The reads the endpoint at `place` has had, failed ones included.
    fn asked(&self, place: usize) -> u64 {
        let endpoint = &self.fleet.faulty[place];
        endpoint.calls(Op::Get) + endpoint.calls(Op::Head)
    }

    /// The reads every endpoint has had.
    fn calls(&self) -> u64 {
        (0..self.fleet.faulty.len())
            .map(|place| self.asked(place))
            .sum()
    }

    /// Tells the poll that these endpoints are ready (the others are not), and runs a round.
    async fn ready_only(&self, ready: &[usize]) {
        for (place, name) in self.fleet.names.iter().enumerate() {
            self.fleet.probe.set(name, ready.contains(&place));
        }
        self.fleet.endpoints.poll().await;
    }
}

/// Asserts that `/readyz` is 200 while an endpoint is ready, 503 with the reason once none is
/// (the poll said so), and 200 again when one is; that a drain says `draining` whatever the
/// endpoints say; and that `/healthz` stays 200 throughout, since the process is up.
#[tokio::test]
async fn readyz_is_503_while_no_endpoint_is_ready() {
    let front = Front::new("tsep-front-readyz").await;
    let ready = front.probe("/readyz").await;
    assert_eq!(
        (ready.status, ready.body.as_str()),
        (StatusCode::OK, "ready")
    );

    front.ready_only(&[]).await;
    let down = front.probe("/readyz").await;
    assert_eq!(down.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(down.body, "no upstream endpoint is ready");
    assert_eq!(front.probe("/healthz").await.status, StatusCode::OK);

    front.ready_only(&[1]).await;
    let one = front.probe("/readyz").await;
    assert_eq!((one.status, one.body.as_str()), (StatusCode::OK, "ready"));

    front.server.begin_drain();
    let draining = front.probe("/readyz").await;
    assert_eq!(draining.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(draining.body, "draining");
}

/// Asserts that `/metricz` has the cache's counters and, after them, the text of the endpoints,
/// with the ready gauge of each endpoint at 1, then 0 for the one the poll said is not ready.
#[tokio::test]
async fn metricz_adds_the_counters_of_the_endpoints() {
    let front = Front::new("tsep-front-metricz").await;
    let text = front.probe("/metricz").await.body;
    assert!(text.contains("tierstore_cache_hits_total 0\n"), "{text}");
    assert!(text.ends_with(&front.fleet.endpoints.metrics()), "{text}");
    let first = &front.fleet.names[0];
    let gauge = |ready: u8| format!("endpoint_ready{{endpoint=\"{first}\"}} {ready}\n");
    assert!(text.contains(&gauge(1)), "{text}");

    front.ready_only(&[1]).await;
    let text = front.probe("/metricz").await.body;
    assert!(text.contains(&gauge(0)), "{text}");
}

/// Asserts that a cache that has lost every endpoint serves the objects it holds, GET and HEAD,
/// without a request to an endpoint; answers an object it does not hold with 503
/// `ServiceUnavailable`, without a request to an endpoint that is known not to be ready; and
/// serves that object again once an endpoint is ready, from the endpoint that is.
#[tokio::test]
async fn a_cache_without_endpoints_serves_what_it_holds() {
    let front = Front::new("tsep-front-down").await;
    let filled = front.object("GET", "cas/b/cached").await;
    assert_eq!(
        (filled.status, filled.body.as_bytes()),
        (StatusCode::OK, BODY)
    );

    for faulty in &front.fleet.faulty {
        faulty.set_down(true);
    }
    front.ready_only(&[]).await;
    let before = front.calls();

    let hit = front.object("GET", "cas/b/cached").await;
    assert_eq!((hit.status, hit.body.as_bytes()), (StatusCode::OK, BODY));
    let head = front.object("HEAD", "cas/b/cached").await;
    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.header("content-length"), "20");
    let cold = front.object("GET", "cas/b/cold").await;
    assert_eq!(cold.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(cold.code(), "ServiceUnavailable");
    let cold_head = front.object("HEAD", "cas/b/cold").await;
    assert_eq!(cold_head.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(front.calls(), before, "no endpoint was asked");
    assert_eq!(
        front.probe("/readyz").await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );

    front.fleet.faulty[1].set_down(false);
    front.ready_only(&[1]).await;
    let asked_first = front.asked(0);
    let served = front.object("GET", "cas/b/cold").await;
    assert_eq!(
        (served.status, served.body.as_bytes()),
        (StatusCode::OK, BODY)
    );
    assert_eq!(
        front.asked(0),
        asked_first,
        "the endpoint that is not ready is skipped"
    );
    assert_eq!(front.probe("/readyz").await.status, StatusCode::OK);
}
