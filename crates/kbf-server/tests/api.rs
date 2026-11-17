//! The operator API (`/v1`) over a real listener: its bind, its write gates and its
//! protocols.

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kbf_front::{Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_server::token::ApiToken;
use kbf_server::{Api, Listeners, ServeError, bind_server_with_api};
use serde_json::Value;
use support::PROMPT;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

type MemoryCache = Cache<MemoryMetaLog, MemoryStore>;

fn cache() -> Arc<MemoryCache> {
    Arc::new(Cache::memory())
}

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// A server with the operator API in this process.
struct Server {
    api: SocketAddr,
    stop: tokio::sync::oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<()>,
}

impl Server {
    /// Shuts the server down and waits until it has stopped.
    async fn stop(self) {
        self.stop.send(()).expect("the server is running");
        self.serving.await.expect("the server stops cleanly");
    }
}

/// The token the test servers' writes need.
const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";

/// A token file holding `content`, mode 0600, in a directory of its own, read at once
/// (the directory is removed when this returns).
fn token_from(content: &str) -> ApiToken {
    use std::os::unix::fs::PermissionsExt;
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "kbf-server-api-token");
    let path = dir.join("token");
    std::fs::write(&path, content).expect("write the token");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    ApiToken::from_file(&path).expect("a usable token")
}

fn token() -> ApiToken {
    token_from(&format!("{TOKEN}\n"))
}

fn start() -> Server {
    start_with(Some(token()))
}

fn listeners() -> Listeners {
    Listeners {
        reapi: loopback(),
        worker: loopback(),
        reapi_tls: None,
        worker_tls: None,
        executor: None,
        finished_retention: kbf_server::executor::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

fn start_with(token: Option<ApiToken>) -> Server {
    let listeners = listeners();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let shutdown = async move {
        let _ = stopped.await;
    };
    let api = Api {
        listen: loopback(),
        token,
    };
    let bound = bind_server_with_api(cache(), listeners, Some(api), shutdown).expect("bind");
    let api = bound.api.expect("an API address");
    let serving = tokio::spawn(async move { bound.serving.await.expect("serve") });
    Server { api, stop, serving }
}

/// One HTTP/1.1 request with `headers` (each line ending `\r\n`); the status code,
/// the response head and the body.
async fn http_with(
    api: SocketAddr,
    method: &str,
    path: &str,
    headers: &str,
    body: &str,
) -> (u16, String, String) {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let length = body.len();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: kbf\r\nContent-Length: {length}\r\n\
         {headers}Connection: close\r\n\r\n{body}"
    );
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .expect("a status code");
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: application/json")
            || code == 404
            || code == 405,
        "{head}"
    );
    (code, head.to_owned(), body.to_owned())
}

/// Catches: an API address that cannot be bound being ignored (a server that starts
/// without the API it was asked for) instead of refused at start.
#[tokio::test]
async fn an_api_address_in_use_is_refused() {
    let taken = std::net::TcpListener::bind(loopback()).expect("bind a port");
    let addr = taken.local_addr().expect("its address");
    let listeners = Listeners {
        reapi: loopback(),
        worker: loopback(),
        reapi_tls: None,
        worker_tls: None,
        executor: None,
        finished_retention: kbf_server::executor::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    };
    let api = Api {
        listen: addr,
        token: None,
    };
    let refused = bind_server_with_api(cache(), listeners, Some(api), std::future::pending());
    let Err(ServeError::Bind { addr: at, .. }) = refused else {
        panic!("an API address in use was not refused");
    };
    assert_eq!(at, addr);
}

/// Every gate of a write over a real listener, each on its own request: no token, a
/// wrong or empty one, an `Origin` header, and a content type other than JSON are
/// refused (401 with `WWW-Authenticate: Bearer`, 403, 415) before the write is looked at; a
/// server started with no token refuses every write with a 403 that names the flag; the write
/// that passes every gate reaches its handler, which says this server keeps no storage members
/// (404), so a refusal and a pass cannot be mistaken for each other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_over_http_need_the_token_no_origin_and_json() {
    const WRITE: &str = "/v1/denied";
    let server = start();
    let api = server.api;
    let bearer = format!("Authorization: Bearer {TOKEN}\r\n");
    let json = "Content-Type: application/json\r\n";
    let wrong = "Authorization: Bearer kbf-test-token-wrong-0123456789abcdef\r\n";
    let origin = "Origin: http://localhost:3000\r\n";
    for (headers, want) in [
        (json.to_owned(), 401),
        (format!("{wrong}{json}"), 401),
        (format!("{bearer}{json}{origin}"), 403),
        (format!("{bearer}Content-Type: text/plain\r\n"), 415),
        (bearer.clone(), 415),
    ] {
        let (code, head, body) = http_with(api, "POST", WRITE, &headers, "{}").await;
        assert_eq!(code, want, "{headers}: {body}");
        if code == 401 {
            let head = head.to_ascii_lowercase();
            assert!(head.contains("www-authenticate: bearer"), "{head}");
        }
    }
    let (code, _, body) = http_with(api, "POST", WRITE, &format!("{bearer}{json}"), "{}").await;
    assert_eq!(code, 404, "{body}");
    assert!(body.contains("no storage members"), "{body}");
    server.stop().await;

    let server = start_with(None);
    let (code, _, body) = http_with(server.api, "POST", WRITE, json, "{}").await;
    assert_eq!(code, 403, "{body}");
    let body: Value = serde_json::from_str(&body).expect("JSON");
    let why = body["error"].as_str().unwrap_or_default();
    assert!(why.contains("--api-token-file"), "{why}");
    server.stop().await;
}

/// Catches: `docs/api.md` wrong about the protocols the listener speaks. It answers
/// HTTP/2 in clear text (h2c, prior knowledge) as well as HTTP/1.1, so the write
/// gates must hold for both; they read only the request's peer and headers, which
/// both carry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_api_listener_also_speaks_h2c() {
    let server = start();
    let mut stream = TcpStream::connect(server.api).await.expect("connect");
    // The HTTP/2 connection preface and an empty SETTINGS frame.
    let mut preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n".to_vec();
    preface.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 0, 0]);
    stream.write_all(&preface).await.expect("send");
    let mut head = [0u8; 9];
    tokio::time::timeout(PROMPT, stream.read_exact(&mut head))
        .await
        .expect("an answer")
        .expect("a frame header");
    assert_eq!(head[3], 4, "the server's first frame is SETTINGS: {head:?}");
    drop(stream);
    server.stop().await;
}

/// Catches the invocation recorder's task run inside the serving future, beside the
/// accept loops (`recording.run()` back in their `select!`): once
/// the serving future has been polled, the recorder applies events while nothing
/// polls that future again.
#[tokio::test]
async fn the_recorder_runs_as_a_task_of_its_own() {
    let bound =
        bind_server_with_api(cache(), listeners(), None, std::future::pending()).expect("bind");
    let recorder = bound.recorder.clone();
    let mut serving = Box::pin(bound.serving);
    assert!(futures::poll!(serving.as_mut()).is_pending());
    recorder.emit(kbf_invocation::Event::Started {
        operation: kbf_types::OperationId(1),
        lease: kbf_types::LeaseId::new(1, 1),
        at: kbf_front::wall_now(),
    });
    tokio::time::timeout(Duration::from_secs(10), recorder.caught_up())
        .await
        .expect("the recorder applies events on its own task");
    drop(serving);
}
