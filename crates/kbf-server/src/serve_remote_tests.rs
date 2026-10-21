//! A call from a remote peer is refused by the real listeners of a server behind a proxy
//! (`--proxy-auth-file`): the operator API as `serve_api` serves it, and the worker listener
//! as `worker_peers` builds it. The sockets are on loopback, as a test's must be, so the
//! connection says a documentation address (192.0.2.10) is its peer: the operator listener
//! through a [`Listener`] that reports it to `ConnectInfo`, the worker listener through an
//! incoming whose connections carry it as `kbf_tls::ConnInfo`, where `Incoming` puts the
//! address of the accepted socket. Each test runs the same call from a claimed loopback
//! peer too, so a refusal is the layer's and not a server that was never reachable.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};

use axum::Router;
use axum::routing::get;
use axum::serve::Listener;
use futures::stream;
use kbf_front::{ByteStreamService, Cache, MemoryMetaLog};
use kbf_objstore::MemoryStore;
use kbf_proto::google::bytestream::ReadRequest;
use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
use kbf_proto::google::bytestream::byte_stream_server::ByteStreamServer;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tonic::metadata::{MetadataKey, MetadataValue};
use tonic::transport::server::{Connected, Server};
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request};

use crate::Behind;
use crate::proxy_auth::{NODE_ID, PROXY_AUTH, ProxySecret, ROLE};
use crate::serve::{serve_api, worker_peers};

const REMOTE: &str = "192.0.2.10:4000";
const LOOPBACK: &str = "127.0.0.1:4000";
/// The 32 bytes of the secret, and the header value that presents them.
const SECRET: [u8; 32] = [0x5a; 32];

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("an address")
}

fn behind() -> Behind {
    static N: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "kbf-serve-remote-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, SECRET).expect("write the secret");
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .expect("chmod");
    let secret = ProxySecret::from_file(&path).expect("a usable secret");
    std::fs::remove_file(&path).expect("remove the secret file");
    Behind {
        secret: Arc::new(secret),
        deny_list: None,
    }
}

// ---- the operator API --------------------------------------------------------------

/// A listener on a loopback socket whose every peer is said to be `claimed`.
struct Claimed {
    socket: TcpListener,
    claimed: SocketAddr,
}

impl Listener for Claimed {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (TcpStream, SocketAddr) {
        loop {
            if let Ok((io, _real)) = self.socket.accept().await {
                return (io, self.claimed);
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

/// One HTTP/1.1 `GET /ping` to `to`: the status line's code and the body.
async fn ping(to: SocketAddr) -> (u16, String) {
    let mut tcp = TcpStream::connect(to).await.expect("connect");
    tcp.write_all(b"GET /ping HTTP/1.1\r\nhost: kbf\r\nconnection: close\r\n\r\n")
        .await
        .expect("write");
    let mut reply = String::new();
    tcp.read_to_string(&mut reply).await.expect("read");
    let code = reply
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {reply:?}"));
    (code, reply)
}

/// The operator API (`serve_api`) as `peer` reaches it, serving a route that answers 200.
async fn operator(peer: &str, proxy: Option<&Behind>) -> (u16, String) {
    let socket = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let at = socket.local_addr().expect("address");
    let listener = Claimed {
        socket,
        claimed: addr(peer),
    };
    let routes = Router::new().route("/ping", get(|| async { "pong" }));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let served = tokio::spawn(serve_api(
        Some(listener),
        routes,
        async move {
            let _ = stopped.await;
        },
        proxy.map(|behind| Arc::clone(&behind.secret)),
    ));
    let answer = ping(at).await;
    stop.send(()).expect("still serving");
    served.await.expect("task").expect("served");
    answer
}

/// Catches: the operator listener serving a remote peer when a proxy is configured, because
/// it does not apply the proxy gate (`serve_api` without its layer), or applies it with no
/// secret.
///
/// Mutants seen red: `serve_api` without the `gate_requests` layer; the layer given `None`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operator_listener_refuses_a_remote_peer_behind_the_proxy() {
    let behind = behind();
    let (code, reply) = operator(REMOTE, Some(&behind)).await;
    assert_eq!(code, 403, "{reply}");
    assert!(
        reply.contains("takes calls from its own host only"),
        "{reply}"
    );
    assert!(!reply.contains("pong"), "{reply}");
}

/// The control of the test above: the same listener answers a loopback peer, and answers a
/// remote one when no proxy is configured (the flag off changes nothing).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_operator_listener_serves_loopback_and_a_remote_peer_without_the_flag() {
    let behind = behind();
    let (code, reply) = operator(LOOPBACK, Some(&behind)).await;
    assert_eq!(code, 200, "{reply}");
    assert!(reply.ends_with("pong"), "{reply}");
    let (code, reply) = operator(REMOTE, None).await;
    assert_eq!(code, 200, "{reply}");
}

// ---- the worker listener -----------------------------------------------------------

/// A connection that says its peer is `info`'s, as `kbf_tls::Incoming` says the accepted
/// socket's.
struct Conn {
    io: TcpStream,
    info: kbf_tls::ConnInfo,
}

impl Connected for Conn {
    type ConnectInfo = kbf_tls::ConnInfo;

    fn connect_info(&self) -> kbf_tls::ConnInfo {
        self.info.clone()
    }
}

impl AsyncRead for Conn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Conn {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

/// The worker listener (`worker_peers`) with a `ByteStream` service, as `peer` reaches it;
/// a `Read` of a blob the cache lacks, with `claims` as request headers: the status code.
async fn worker(peer: &str, claims: &[(&str, String)]) -> (Code, String) {
    let behind = behind();
    let (mut server, _peers, _tls) = worker_peers(Server::builder(), None, Some(&behind));
    let cache: Arc<Cache<MemoryMetaLog, MemoryStore>> = Arc::new(Cache::memory());
    let service = ByteStreamServer::new(ByteStreamService::new(cache));
    let socket = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let at = socket.local_addr().expect("address");
    let info = kbf_tls::ConnInfo {
        tcp: tonic::transport::server::TcpConnectInfo {
            local_addr: Some(at),
            remote_addr: Some(addr(peer)),
        },
        certs: None,
    };
    let incoming = stream::unfold((socket, info), |(socket, info)| async move {
        let accepted = socket.accept().await.map(|(io, _real)| Conn {
            io,
            info: info.clone(),
        });
        Some((accepted, (socket, info)))
    });
    let served = tokio::spawn(server.add_service(service).serve_with_incoming(incoming));

    let channel: Channel = Endpoint::from_shared(format!("http://{at}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let mut request = Request::new(ReadRequest {
        resource_name: format!("blobs/{}/0", "0".repeat(64)),
        read_offset: 0,
        read_limit: 0,
    });
    for (name, value) in claims {
        let key = MetadataKey::from_bytes(name.as_bytes()).expect("a key");
        let value: MetadataValue<_> = value.parse().expect("a value");
        request.metadata_mut().insert(key, value);
    }
    let result = match ByteStreamClient::new(channel).read(request).await {
        Ok(response) => response.into_inner().message().await.map(drop),
        Err(status) => Err(status),
    };
    served.abort();
    match result {
        Ok(()) => (Code::Ok, String::new()),
        Err(status) => (status.code(), status.message().to_owned()),
    }
}

/// What the proxy forwards for a verified daemon.
fn forwarded() -> Vec<(&'static str, String)> {
    vec![
        (PROXY_AUTH, hex::encode(SECRET)),
        (NODE_ID, "node-a".to_owned()),
        (ROLE, "daemon".to_owned()),
    ]
}

/// Catches: the worker listener serving a remote peer that presents the proxy's own secret
/// and identity (a call that reached the child some way other than the proxy), because the
/// listener does not apply the proxy layer.
///
/// Mutants seen red: `worker_peers` without the `ProxyAuthLayer` on the behind-a-proxy arm;
/// the layer built with no secret.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_worker_listener_refuses_a_remote_peer_behind_the_proxy() {
    let (code, message) = worker(REMOTE, &forwarded()).await;
    assert_eq!(code, Code::Unauthenticated, "{message}");
    assert!(
        message.contains("takes calls from its own host only"),
        "{message}"
    );
}

/// The control of the test above: the same call from a loopback peer passes the layer and
/// the role table and is answered by the service (the blob is not there), and without the
/// secret it is not the proxy's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_worker_listener_serves_the_proxy_on_loopback() {
    let (code, message) = worker(LOOPBACK, &forwarded()).await;
    assert!(
        matches!(code, Code::Ok | Code::NotFound),
        "answered by the service: {code:?} {message}"
    );
}
