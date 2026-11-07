//! A server started on sockets its caller already holds (`bind_server_holding`): the port a
//! caller picks is held from the pick until the server serves on it, so nothing else can
//! take it in between.

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use kbf_auth::Policy;
use kbf_front::Cache;
use kbf_server::{Api, Held, Listeners, ServeError, bind_server_holding, bind_server_with_ui};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// A listener on a port the system picks, and where it is.
fn hold() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind(loopback()).expect("bind");
    let addr = listener.local_addr().expect("address");
    (listener, addr)
}

fn listeners(reapi: SocketAddr, worker: SocketAddr) -> Listeners {
    Listeners {
        reapi,
        worker,
        reapi_tls: None,
        worker_tls: None,
        heartbeat_interval: Duration::from_millis(200),
        hello_wait: Duration::from_secs(5),
        tick: Duration::from_millis(50),
        unservable_wait: kbf_sched::UNSERVABLE_WAIT,
        default_platform: kbf_front::DefaultPlatform::NONE,
        qos_allowed: kbf_front::QosAllow::DEFAULT,
        memory_ladder: kbf_server::MemoryLadder::default(),
        memory_climb: true,
        vm_host_floor: kbf_server::HostFloor::default(),
        learned_sizes: kbf_server::LearnedSizes::Off,
        exclusive_actions: false,
        placement_load: kbf_server::PlacementLoad::Off,
        placement_predictions: kbf_server::PlacementPredictions::Off,
        usage_profiles: kbf_server::UsageProfiles::Off,
        whole_node: None,
        finished_retention: kbf_sched::FINISHED_RETENTION,
        shutdown_timeout: Duration::from_secs(10),
        streams: kbf_server::StreamTiming::default(),
        store_probe_timeout: kbf_server::health::STORE_PROBE_TIMEOUT,
        invocation_memory: kbf_front::DEFAULT_INVOCATION_MEMORY,
        segments: kbf_front::SegmentsConfig::DEFAULT,
        stream_limits: kbf_server::StreamLimits::DEFAULT,
    }
}

fn api(listen: SocketAddr) -> Option<Api> {
    Some(Api {
        listen,
        token: None,
        local_cache: None,
    })
}

/// The status line of a `GET path` on `addr`.
async fn status(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let request = format!("GET {path} HTTP/1.1\r\nHost: kbf\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("send the request");
    let mut answer = String::new();
    stream
        .read_to_string(&mut answer)
        .await
        .expect("read the answer");
    answer.lines().next().unwrap_or_default().to_owned()
}

/// Catches: a server that binds the addresses of the sockets it is handed again instead of
/// serving on them (the start fails with `AddrInUse`, the port being the held socket's
/// own: what `bind_server_with_ui` does, shown first), one that reports other addresses
/// than the held ones, and an operator API that is not served on its held socket.
#[tokio::test]
async fn a_server_serves_on_the_sockets_it_is_handed() {
    let (reapi, reapi_at) = hold();
    let (worker, worker_at) = hold();
    let (operator, operator_at) = hold();

    let refused = bind_server_with_ui(
        Arc::new(Cache::memory()),
        listeners(reapi_at, worker_at),
        api(operator_at),
        Policy::allow_all(),
        None,
        None,
        None,
        None,
        true,
        std::future::pending(),
    )
    .err()
    .expect("the ports are held");
    let ServeError::Bind { addr, source } = refused else {
        panic!("{refused}");
    };
    assert_eq!(
        (addr, source.kind()),
        (reapi_at, std::io::ErrorKind::AddrInUse)
    );

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let bound = bind_server_holding(
        Held::new(vec![reapi, worker, operator]).expect("held"),
        Arc::new(Cache::memory()),
        listeners(reapi_at, worker_at),
        api(operator_at),
        Policy::allow_all(),
        None,
        None,
        None,
        None,
        true,
        async move {
            let _ = stopped.await;
        },
    )
    .expect("served on the held sockets");
    assert_eq!(
        (bound.reapi, bound.worker, bound.api),
        (reapi_at, worker_at, Some(operator_at))
    );
    let serving = tokio::spawn(bound.serving);
    assert_eq!(status(operator_at, "/readyz").await, "HTTP/1.1 200 OK");
    stop.send(()).expect("the server is running");
    serving
        .await
        .expect("the serving task")
        .expect("the server stops cleanly");
}

/// Catches: a held socket that no listener asks for left open once the server is bound (its
/// port stays taken), and a listener with no held socket that is not bound as usual: the
/// worker and the operator API ask for port 0 here and get ports of their own.
#[tokio::test]
async fn listeners_without_a_held_socket_bind_and_a_spare_one_is_closed() {
    let (reapi, reapi_at) = hold();
    let (spare, spare_at) = hold();
    let mut waiting = std::net::TcpStream::connect(spare_at).expect("connect");

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let bound = bind_server_holding(
        Held::new(vec![reapi, spare]).expect("held"),
        Arc::new(Cache::memory()),
        listeners(reapi_at, loopback()),
        api(loopback()),
        Policy::allow_all(),
        None,
        None,
        None,
        None,
        false,
        async move {
            let _ = stopped.await;
        },
    )
    .expect("bound");
    assert_eq!(bound.reapi, reapi_at);
    let operator_at = bound.api.expect("an operator API");
    for port in [bound.worker.port(), operator_at.port()] {
        assert!(
            ![0, reapi_at.port(), spare_at.port()].contains(&port),
            "{port}"
        );
    }

    // The spare socket is closed: the connection waiting in its queue is reset.
    let read = std::io::Read::read(&mut waiting, &mut [0; 1]);
    assert!(
        matches!(&read, Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset)
            || matches!(read, Ok(0)),
        "{read:?}"
    );

    let serving = tokio::spawn(bound.serving);
    assert_eq!(status(operator_at, "/readyz").await, "HTTP/1.1 200 OK");
    stop.send(()).expect("the server is running");
    serving
        .await
        .expect("the serving task")
        .expect("the server stops cleanly");
}
